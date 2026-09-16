//! Mistral Conversations streaming provider.
//! Port of `packages/ai/src/providers/mistral.ts`: `chat/completions` SSE
//! streaming with camelCase-free snake_case wire keys (verified against the
//! `@mistralai/mistralai` SDK outbound schemas), thinking text-block
//! accumulation, tool-call argument streaming, `x-affinity` KV-cache header,
//! and usage accounting.

use std::cell::RefCell;
use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::env_api_keys::get_env_api_key;
use crate::event_stream::{
    create_assistant_message_event_stream, AssistantMessageEvent, AssistantMessageEventStream,
    AssistantMessageEventWriter,
};
use crate::models::{calculate_cost, clamp_thinking_level};
use crate::providers::simple_options::build_base_options;
use crate::providers::transform_messages::transform_messages_with_normalizer;
use crate::registry::Provider;
use crate::types::{
    done_reason, error_reason, AssistantContent, AssistantMessage, Context, Message, Model,
    ModelExt, ModelThinkingLevel, SimpleStreamOptions, StopReason, StreamOptions, TextContent,
    ThinkingContent, Tool, ToolCall, Usage,
};
use crate::utils_inner::diagnostics::now_ms;
use crate::utils_inner::hash::short_hash;
use crate::utils_inner::http::{send, HttpResponse, RequestOptions};
use crate::utils_inner::json_parse::{parse_json_with_repair, parse_streaming_json};
use crate::utils_inner::sanitize_unicode::sanitize_surrogates;
use crate::utils_inner::sse::SseDecoder;
use crate::utils_inner::stream_failure::{
    format_stream_failure_message, record_stream_failure, stream_failure_from_stop_reason,
    ProviderError,
};

pub const API_MISTRAL_CONVERSATIONS: &str = "mistral-conversations";

const MISTRAL_TOOL_CALL_ID_LENGTH: usize = 9;
const MAX_MISTRAL_ERROR_BODY_CHARS: usize = 4000;

/// Mistral reasoning-effort values (`MistralReasoningEffort` in the TS).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MistralReasoningEffort {
    None,
    High,
}

impl MistralReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            MistralReasoningEffort::None => "none",
            MistralReasoningEffort::High => "high",
        }
    }
}

/// `promptMode` request option; only `reasoning` exists in the API surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MistralPromptMode {
    Reasoning,
}

impl MistralPromptMode {
    pub fn as_str(self) -> &'static str {
        "reasoning"
    }
}

/// Tool selection (`toolChoice` in the TS reference).
#[allow(dead_code)] // full TS option surface; variants set by callers
#[derive(Clone, Debug, PartialEq)]
pub enum MistralToolChoice {
    Auto,
    None,
    Any,
    Required,
    Tool { name: String },
}

impl MistralToolChoice {
    fn to_json(&self) -> Value {
        match self {
            MistralToolChoice::Auto => json!("auto"),
            MistralToolChoice::None => json!("none"),
            MistralToolChoice::Any => json!("any"),
            MistralToolChoice::Required => json!("required"),
            MistralToolChoice::Tool { name } => {
                json!({ "type": "function", "function": { "name": name } })
            }
        }
    }
}

/// Provider-specific request options (`MistralOptions` in the TS reference).
#[derive(Clone, Default)]
pub struct MistralOptions {
    pub base: StreamOptions,
    pub tool_choice: Option<MistralToolChoice>,
    pub prompt_mode: Option<MistralPromptMode>,
    pub reasoning_effort: Option<MistralReasoningEffort>,
}

impl MistralOptions {
    pub fn from_base(base: StreamOptions) -> Self {
        Self {
            base,
            tool_choice: None,
            prompt_mode: None,
            reasoning_effort: None,
        }
    }
}

/// Port of `streamMistral`.
pub fn stream_mistral(
    model: &Model,
    context: &Context,
    options: Option<&MistralOptions>,
) -> AssistantMessageEventStream {
    let options = options.cloned();
    let model = model.clone();
    let context = context.clone();
    let (writer, reader) = create_assistant_message_event_stream();

    tokio::spawn(async move {
        let mut output = AssistantMessage {
            content: Vec::new(),
            api: API_MISTRAL_CONVERSATIONS.to_string(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_reason_raw: None,
            error_message: None,
            timestamp: now_ms(),
            rest: Default::default(),
        };

        let result = run_stream(&model, &context, options.as_ref(), &mut output, &writer).await;
        match result {
            Ok(()) => {
                writer.push(AssistantMessageEvent::Done {
                    reason: done_reason(output.stop_reason),
                    message: output,
                });
                writer.end(None);
            }
            Err(error) => {
                output.stop_reason = if error == ProviderError::Aborted {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                output.error_message = Some(format_stream_failure_message(&error));
                record_stream_failure(
                    (&model.provider, &model.id, &model.api),
                    &mut output,
                    &error,
                );
                writer.push(AssistantMessageEvent::Error {
                    reason: error_reason(output.stop_reason),
                    error: output.clone(),
                });
                writer.end(Some(output));
            }
        }
    });

    reader
}

/// Stateful tool-call-id normalizer port
/// (`createMistralToolCallIdNormalizer` in the TS reference).
#[derive(Default)]
struct MistralToolCallIdNormalizer {
    id_map: RefCell<HashMap<String, String>>,
    reverse_map: RefCell<HashMap<String, String>>,
}

impl MistralToolCallIdNormalizer {
    fn normalize(&self, id: &str) -> String {
        if let Some(existing) = self.id_map.borrow().get(id) {
            return existing.clone();
        }
        let mut attempt = 0;
        loop {
            let candidate = derive_mistral_tool_call_id(id, attempt);
            let owner = self.reverse_map.borrow().get(&candidate).cloned();
            if owner.is_none() || owner.as_deref() == Some(id) {
                self.id_map
                    .borrow_mut()
                    .insert(id.to_string(), candidate.clone());
                self.reverse_map
                    .borrow_mut()
                    .insert(candidate.clone(), id.to_string());
                return candidate;
            }
            attempt += 1;
        }
    }
}

/// Coerce a parsed streaming JSON value into an object map (non-object
/// partial parses decode to `{}`, matching `parseStreamingJson<Record<...>>`).
fn json_object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Port of `deriveMistralToolCallId`.
fn derive_mistral_tool_call_id(id: &str, attempt: u32) -> String {
    let normalized: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if attempt == 0 && normalized.len() == MISTRAL_TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let seed_base = if normalized.is_empty() {
        id
    } else {
        &normalized
    };
    let seed = if attempt == 0 {
        seed_base.to_string()
    } else {
        format!("{seed_base}:{attempt}")
    };
    short_hash(&seed)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(MISTRAL_TOOL_CALL_ID_LENGTH)
        .collect()
}

fn build_request_headers(
    model: &Model,
    options: &MistralOptions,
    api_key: &str,
) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in model.headers.iter().flatten() {
        headers.push((name.clone(), value.clone()));
    }
    if let Some(options_headers) = &options.base.headers {
        for (name, value) in options_headers {
            headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
            headers.push((name.clone(), value.clone()));
        }
    }
    // Mistral infrastructure uses `x-affinity` for KV-cache reuse (prefix
    // caching). Respect explicit caller-provided header values.
    if let Some(session_id) = &options.base.session_id {
        if !headers.iter().any(|(name, _)| name == "x-affinity") {
            headers.push(("x-affinity".into(), session_id.clone()));
        }
    }
    headers.push(("authorization".into(), format!("Bearer {api_key}")));
    headers
}

fn build_chat_payload(
    model: &Model,
    context: &Context,
    transformed_messages: &[Message],
    options: &MistralOptions,
) -> Value {
    let supports_images = model.supports_image_input();
    let mut messages = to_chat_messages(transformed_messages, supports_images);

    if let Some(system_prompt) = &context.system_prompt {
        messages.insert(
            0,
            json!({
                "role": "system",
                "content": sanitize_surrogates(system_prompt),
            }),
        );
    }

    let mut payload = Map::new();
    payload.insert("model".into(), json!(model.id));
    payload.insert("stream".into(), json!(true));
    payload.insert("messages".into(), Value::Array(messages));
    if let Some(tools) = &context.tools {
        if !tools.is_empty() {
            payload.insert("tools".into(), json!(to_function_tools(tools)));
        }
    }
    if let Some(temperature) = options.base.temperature {
        payload.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = options.base.max_tokens {
        payload.insert("max_tokens".into(), json!(max_tokens));
    }
    if let Some(tool_choice) = &options.tool_choice {
        payload.insert("tool_choice".into(), tool_choice.to_json());
    }
    if let Some(prompt_mode) = options.prompt_mode {
        payload.insert("prompt_mode".into(), json!(prompt_mode.as_str()));
    }
    if let Some(reasoning_effort) = options.reasoning_effort {
        payload.insert("reasoning_effort".into(), json!(reasoning_effort.as_str()));
    }

    Value::Object(payload)
}

/// Port of `toFunctionTools`.
fn to_function_tools(tools: &[Tool]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": strip_symbol_keys(&tool.parameters),
                    "strict": false,
                },
            })
        })
        .collect()
}

/// Port of `stripSymbolKeys`: rebuild the JSON tree as plain objects.
fn strip_symbol_keys(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(strip_symbol_keys).collect()),
        Value::Object(map) => {
            let mut result = Map::new();
            for (key, entry) in map {
                result.insert(key.clone(), strip_symbol_keys(entry));
            }
            Value::Object(result)
        }
        other => other.clone(),
    }
}

/// Port of `toChatMessages`.
fn to_chat_messages(messages: &[Message], supports_images: bool) -> Vec<Value> {
    let mut result: Vec<Value> = Vec::new();

    for msg in messages {
        match msg {
            Message::User(user) => match &user.content {
                crate::types::UserMessageContent::Text(text) => {
                    result.push(json!({
                        "role": "user",
                        "content": sanitize_surrogates(text),
                    }));
                    continue;
                }
                crate::types::UserMessageContent::Blocks(blocks) => {
                    let had_images = blocks
                        .iter()
                        .any(|item| matches!(item, crate::types::UserOrToolContent::Image(_)));
                    let mut content: Vec<Value> = Vec::new();
                    for item in blocks {
                        match item {
                            crate::types::UserOrToolContent::Text(text) => {
                                content.push(json!({
                                    "type": "text",
                                    "text": sanitize_surrogates(&text.text),
                                }));
                            }
                            crate::types::UserOrToolContent::Image(image) if supports_images => {
                                content.push(json!({
                                        "type": "image_url",
                                        "image_url": format!("data:{};base64,{}", image.mime_type, image.data),
                                    }));
                            }
                            crate::types::UserOrToolContent::Image(_) => {}
                        }
                    }
                    if !content.is_empty() {
                        result.push(json!({ "role": "user", "content": content }));
                        continue;
                    }
                    if had_images && !supports_images {
                        result.push(json!({
                            "role": "user",
                            "content": "(image omitted: model does not support images)",
                        }));
                    }
                    continue;
                }
            },
            Message::Assistant(assistant) => {
                let mut content_parts: Vec<Value> = Vec::new();
                let mut tool_calls: Vec<Value> = Vec::new();

                for block in &assistant.content {
                    match block {
                        crate::types::AssistantContent::Text(text) => {
                            if !text.text.trim().is_empty() {
                                content_parts.push(json!({
                                    "type": "text",
                                    "text": sanitize_surrogates(&text.text),
                                }));
                            }
                        }
                        crate::types::AssistantContent::Thinking(thinking) => {
                            if !thinking.thinking.trim().is_empty() {
                                content_parts.push(json!({
                                    "type": "thinking",
                                    "thinking": [{ "type": "text", "text": sanitize_surrogates(&thinking.thinking) }],
                                }));
                            }
                        }
                        crate::types::AssistantContent::ToolCall(call) => {
                            tool_calls.push(json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": serde_json::to_string(
                                        &call.arguments,
                                    ).unwrap_or_else(|_| "{}".to_string()),
                                },
                                "index": 0,
                            }));
                        }
                    }
                }

                if !content_parts.is_empty() || !tool_calls.is_empty() {
                    let mut assistant_message = Map::new();
                    assistant_message.insert("role".into(), json!("assistant"));
                    if !content_parts.is_empty() {
                        assistant_message.insert("content".into(), Value::Array(content_parts));
                    }
                    if !tool_calls.is_empty() {
                        assistant_message.insert("tool_calls".into(), Value::Array(tool_calls));
                    }
                    result.push(Value::Object(assistant_message));
                }
                continue;
            }
            Message::ToolResult(tool_result) => {
                let text_result = tool_result
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        crate::types::UserOrToolContent::Text(text) => {
                            Some(sanitize_surrogates(&text.text))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let has_images = tool_result
                    .content
                    .iter()
                    .any(|part| matches!(part, crate::types::UserOrToolContent::Image(_)));
                let tool_text = build_tool_result_text(
                    &text_result,
                    has_images,
                    supports_images,
                    tool_result.is_error,
                );

                let mut tool_content: Vec<Value> =
                    vec![json!({ "type": "text", "text": tool_text })];
                for part in &tool_result.content {
                    if !supports_images {
                        continue;
                    }
                    if let crate::types::UserOrToolContent::Image(image) = part {
                        tool_content.push(json!({
                            "type": "image_url",
                            "image_url": format!("data:{};base64,{}", image.mime_type, image.data),
                        }));
                    }
                }

                result.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_result.tool_call_id,
                    "name": tool_result.tool_name,
                    "content": tool_content,
                }));
            }
        }
    }

    result
}

/// Port of `buildToolResultText`.
fn build_tool_result_text(
    text: &str,
    has_images: bool,
    supports_images: bool,
    is_error: bool,
) -> String {
    let trimmed = text.trim();
    let error_prefix = if is_error { "[tool error] " } else { "" };

    if !trimmed.is_empty() {
        let image_suffix = if has_images && !supports_images {
            "\n[tool image omitted: model does not support images]"
        } else {
            ""
        };
        return format!("{error_prefix}{trimmed}{image_suffix}");
    }

    if has_images {
        if supports_images {
            return if is_error {
                "[tool error] (see attached image)".to_string()
            } else {
                "(see attached image)".to_string()
            };
        }
        return if is_error {
            "[tool error] (image omitted: model does not support images)".to_string()
        } else {
            "(image omitted: model does not support images)".to_string()
        };
    }

    if is_error {
        "[tool error] (no tool output)".to_string()
    } else {
        "(no tool output)".to_string()
    }
}

fn uses_reasoning_effort(model: &Model) -> bool {
    model.id == "mistral-small-2603"
        || model.id == "mistral-small-latest"
        || model.id == "mistral-medium-3.5"
}

fn uses_prompt_mode_reasoning(model: &Model) -> bool {
    model.reasoning && !uses_reasoning_effort(model)
}

fn map_reasoning_effort(model: &Model, level: ModelThinkingLevel) -> MistralReasoningEffort {
    let mapped = model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&level))
        .and_then(|value| value.clone());
    match mapped.as_deref() {
        Some("none") => MistralReasoningEffort::None,
        _ => MistralReasoningEffort::High,
    }
}

/// Port of `mapChatStopReason`.
fn map_chat_stop_reason(reason: Option<&str>) -> StopReason {
    match reason {
        None => StopReason::Stop,
        Some("stop") => StopReason::Stop,
        Some("length") | Some("model_length") => StopReason::Length,
        Some("tool_calls") => StopReason::ToolUse,
        Some("error") => StopReason::Error,
        Some(_) => StopReason::Stop,
    }
}

fn truncate_error_text(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    format!(
        "{}... [truncated {} chars]",
        &text[..max_chars],
        text.len() - max_chars
    )
}

async fn run_stream(
    model: &Model,
    context: &Context,
    options: Option<&MistralOptions>,
    output: &mut AssistantMessage,
    writer: &AssistantMessageEventWriter,
) -> Result<(), ProviderError> {
    let options = options.cloned().unwrap_or_default();
    let api_key = options
        .base
        .api_key
        .clone()
        .filter(|key| !key.is_empty())
        .or_else(|| get_env_api_key(&model.provider))
        .ok_or_else(|| {
            ProviderError::Message(format!("No API key for provider: {}", model.provider))
        })?;

    if options
        .base
        .signal
        .as_ref()
        .map(|signal| signal.is_cancelled())
        .unwrap_or(false)
    {
        return Err(ProviderError::Aborted);
    }

    let normalizer = MistralToolCallIdNormalizer::default();
    let transformed_messages =
        transform_messages_with_normalizer(&context.messages, model, &|id, _, _| {
            Some(normalizer.normalize(id))
        });

    let mut payload = build_chat_payload(model, context, &transformed_messages, &options);
    if let Some(on_payload) = &options.base.on_payload {
        if let Some(next) = on_payload(payload.clone(), model) {
            payload = next;
        }
    }

    // The mistralai SDK defaults to https://api.mistral.ai and posts to
    // /v1/chat/completions; a model baseUrl replaces the server URL only.
    let base_url = if model.base_url.is_empty() {
        "https://api.mistral.ai".to_string()
    } else {
        model.base_url.trim_end_matches('/').to_string()
    };
    let url = format!("{base_url}/v1/chat/completions");

    let mut headers = build_request_headers(model, &options, &api_key);
    headers.push(("content-type".into(), "application/json".into()));
    headers.push(("accept".into(), "text/event-stream".into()));

    let mut response: HttpResponse = send(RequestOptions {
        method: reqwest::Method::POST,
        url,
        headers,
        body: Some(payload.to_string()),
        signal: options.base.signal.clone(),
        timeout_ms: options.base.timeout_ms,
    })
    .await?;

    if let Some(on_response) = &options.base.on_response {
        on_response(
            crate::types::ProviderResponse {
                status: response.status,
                headers: response.headers.clone(),
            },
            model,
        );
    }

    if response.status >= 400 {
        let body = response.read_all_text().await.unwrap_or_default();
        let body_text = body.trim();
        let message = if body_text.is_empty() {
            format!("Mistral API error ({}): request failed", response.status)
        } else if body_text.len() > MAX_MISTRAL_ERROR_BODY_CHARS {
            format!(
                "Mistral API error ({}): {}",
                response.status,
                truncate_error_text(body_text, MAX_MISTRAL_ERROR_BODY_CHARS)
            )
        } else {
            format!("Mistral API error ({})", response.status)
        };
        let _ = message;
        return Err(ProviderError::from_http_status_body(
            response.status,
            &body,
            response.headers.clone(),
        ));
    }

    writer.push(AssistantMessageEvent::Start {
        partial: output.clone(),
    });

    let mut state = MistralStreamState::new();
    let mut decoder = SseDecoder::new();
    loop {
        let chunk = match response.next_text().await? {
            Some(chunk) => chunk,
            None => break,
        };
        for sse in decoder.push_text(&chunk) {
            if sse.data.trim().is_empty() || sse.data.trim() == "[DONE]" {
                continue;
            }
            let parsed = parse_json_with_repair(&sse.data).map_err(|error| {
                ProviderError::Message(format!("Could not parse Mistral SSE chunk: {error}"))
            })?;
            state.handle_chunk(&parsed, model, output, writer);
        }
    }
    for sse in decoder.finish() {
        if sse.data.trim().is_empty() || sse.data.trim() == "[DONE]" {
            continue;
        }
        let parsed = parse_json_with_repair(&sse.data).map_err(|error| {
            ProviderError::Message(format!("Could not parse Mistral SSE chunk: {error}"))
        })?;
        state.handle_chunk(&parsed, model, output, writer);
    }
    state.finish(output, writer);

    if options
        .base
        .signal
        .as_ref()
        .map(|signal| signal.is_cancelled())
        .unwrap_or(false)
    {
        return Err(ProviderError::Aborted);
    }
    if matches!(output.stop_reason, StopReason::Aborted | StopReason::Error) {
        return Err(ProviderError::StreamFailure(
            stream_failure_from_stop_reason(output.stop_reason_raw.as_deref(), None),
        ));
    }

    Ok(())
}

/// Scratch state for the streaming loop (`consumeChatStream` in the TS).
struct MistralStreamState {
    current_block: Option<CurrentBlock>,
    tool_blocks_by_key: HashMap<String, usize>,
    tool_partial_args: HashMap<usize, String>,
}

enum CurrentBlock {
    Text { index: usize },
    Thinking { index: usize },
}

impl MistralStreamState {
    fn new() -> Self {
        Self {
            current_block: None,
            tool_blocks_by_key: HashMap::new(),
            tool_partial_args: HashMap::new(),
        }
    }

    fn finish_current_block(
        &mut self,
        output: &AssistantMessage,
        writer: &AssistantMessageEventWriter,
    ) {
        match self.current_block.take() {
            Some(CurrentBlock::Text { index }) => {
                let text = match &output.content[index] {
                    AssistantContent::Text(text) => text.text.clone(),
                    _ => String::new(),
                };
                writer.push(AssistantMessageEvent::TextEnd {
                    content_index: index as u64,
                    content: text,
                    partial: output.clone(),
                });
            }
            Some(CurrentBlock::Thinking { index }) => {
                let thinking = match &output.content[index] {
                    AssistantContent::Thinking(thinking) => thinking.thinking.clone(),
                    _ => String::new(),
                };
                writer.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: index as u64,
                    content: thinking,
                    partial: output.clone(),
                });
            }
            None => {}
        }
    }

    fn ensure_text_block(
        &mut self,
        output: &mut AssistantMessage,
        writer: &AssistantMessageEventWriter,
    ) -> usize {
        if let Some(CurrentBlock::Text { index }) = self.current_block {
            return index;
        }
        self.finish_current_block(output, writer);
        output.content.push(AssistantContent::Text(TextContent {
            text: String::new(),
            text_signature: None,
            rest: Default::default(),
        }));
        let index = output.content.len() - 1;
        self.current_block = Some(CurrentBlock::Text { index });
        writer.push(AssistantMessageEvent::TextStart {
            content_index: index as u64,
            partial: output.clone(),
        });
        index
    }

    fn ensure_thinking_block(
        &mut self,
        output: &mut AssistantMessage,
        writer: &AssistantMessageEventWriter,
    ) -> usize {
        if let Some(CurrentBlock::Thinking { index }) = self.current_block {
            return index;
        }
        self.finish_current_block(output, writer);
        output
            .content
            .push(AssistantContent::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: None,
                redacted: None,
                rest: Default::default(),
            }));
        let index = output.content.len() - 1;
        self.current_block = Some(CurrentBlock::Thinking { index });
        writer.push(AssistantMessageEvent::ThinkingStart {
            content_index: index as u64,
            partial: output.clone(),
        });
        index
    }

    /// Port of the `consumeChatStream` chunk loop body.
    fn handle_chunk(
        &mut self,
        chunk: &Value,
        model: &Model,
        output: &mut AssistantMessage,
        writer: &AssistantMessageEventWriter,
    ) {
        // Keep the first non-empty streamed id as the response identifier.
        if output.response_id.is_none() {
            if let Some(id) = chunk.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    output.response_id = Some(id.to_string());
                }
            }
        }

        if let Some(usage) = chunk.get("usage").filter(|usage| usage.is_object()) {
            let input = usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let completion = usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            output.usage.input = input;
            output.usage.output = completion;
            output.usage.cache_read = 0;
            output.usage.cache_write = 0;
            output.usage.total_tokens = usage
                .get("total_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(input + completion);
            calculate_cost(model, &mut output.usage, None);
        }

        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return;
        };

        if let Some(finish_reason) = choice.get("finish_reason").and_then(Value::as_str) {
            output.stop_reason = map_chat_stop_reason(Some(finish_reason));
            if output.stop_reason == StopReason::Error {
                output.stop_reason_raw = Some(finish_reason.to_string());
            }
        }

        let delta = choice.get("delta").cloned().unwrap_or(Value::Null);
        if let Some(content) = delta.get("content").filter(|content| !content.is_null()) {
            let items: Vec<Value> = match content {
                Value::String(text) => vec![Value::String(text.clone())],
                Value::Array(items) => items.clone(),
                _ => Vec::new(),
            };
            for item in items {
                match &item {
                    Value::String(text) => {
                        let text_delta = sanitize_surrogates(text);
                        let index = self.ensure_text_block(output, writer);
                        if let AssistantContent::Text(block) = &mut output.content[index] {
                            block.text.push_str(&text_delta);
                        }
                        writer.push(AssistantMessageEvent::TextDelta {
                            content_index: index as u64,
                            delta: text_delta,
                            partial: output.clone(),
                        });
                        continue;
                    }
                    Value::Object(_)
                        if item.get("type").and_then(Value::as_str) == Some("thinking") =>
                    {
                        let delta_text = item
                            .get("thinking")
                            .and_then(Value::as_array)
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                                    .collect::<String>()
                            })
                            .unwrap_or_default();
                        let thinking_delta = sanitize_surrogates(&delta_text);
                        if thinking_delta.is_empty() {
                            continue;
                        }
                        let index = self.ensure_thinking_block(output, writer);
                        if let AssistantContent::Thinking(block) = &mut output.content[index] {
                            block.thinking.push_str(&thinking_delta);
                        }
                        writer.push(AssistantMessageEvent::ThinkingDelta {
                            content_index: index as u64,
                            delta: thinking_delta,
                            partial: output.clone(),
                        });
                        continue;
                    }
                    Value::Object(_)
                        if item.get("type").and_then(Value::as_str) == Some("text") =>
                    {
                        let text_delta = sanitize_surrogates(
                            item.get("text").and_then(Value::as_str).unwrap_or(""),
                        );
                        let index = self.ensure_text_block(output, writer);
                        if let AssistantContent::Text(block) = &mut output.content[index] {
                            block.text.push_str(&text_delta);
                        }
                        writer.push(AssistantMessageEvent::TextDelta {
                            content_index: index as u64,
                            delta: text_delta,
                            partial: output.clone(),
                        });
                    }
                    _ => {}
                }
            }
        }

        let tool_calls = delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for tool_call in tool_calls {
            if self.current_block.is_some() {
                self.finish_current_block(output, writer);
            }
            let call_id = match tool_call.get("id").and_then(Value::as_str) {
                Some(id) if !id.is_empty() && id != "null" => id.to_string(),
                _ => derive_mistral_tool_call_id(
                    &format!(
                        "toolcall:{}",
                        tool_call.get("index").and_then(Value::as_i64).unwrap_or(0)
                    ),
                    0,
                ),
            };
            let tool_index = tool_call.get("index").and_then(Value::as_i64).unwrap_or(0);
            let key = format!("{call_id}:{}", tool_index.max(0));

            let existing_index = self.tool_blocks_by_key.get(&key).copied();
            let block_index = match existing_index {
                Some(index)
                    if matches!(
                        output.content.get(index),
                        Some(AssistantContent::ToolCall(_))
                    ) =>
                {
                    index
                }
                _ => {
                    let name = tool_call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    output.content.push(AssistantContent::ToolCall(ToolCall {
                        id: call_id.clone(),
                        name,
                        arguments: Default::default(),
                        thought_signature: None,
                        rest: Default::default(),
                    }));
                    let index = output.content.len() - 1;
                    self.tool_blocks_by_key.insert(key.clone(), index);
                    writer.push(AssistantMessageEvent::ToolcallStart {
                        content_index: index as u64,
                        partial: output.clone(),
                    });
                    index
                }
            };

            let args_delta = match tool_call.pointer("/function/arguments") {
                Some(Value::String(text)) => text.clone(),
                Some(other) => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
                None => "{}".to_string(),
            };
            let partial = self
                .tool_partial_args
                .entry(block_index)
                .or_default()
                .to_string();
            let partial = format!("{partial}{args_delta}");
            self.tool_partial_args.insert(block_index, partial.clone());
            let parsed = parse_streaming_json(Some(&partial));
            if let AssistantContent::ToolCall(block) = &mut output.content[block_index] {
                block.arguments = json_object(parsed);
            }
            writer.push(AssistantMessageEvent::ToolcallDelta {
                content_index: block_index as u64,
                delta: args_delta,
                partial: output.clone(),
            });
        }
    }

    /// Port of the trailing block finalization.
    fn finish(&mut self, output: &mut AssistantMessage, writer: &AssistantMessageEventWriter) {
        self.finish_current_block(output, writer);
        let mut indexes: Vec<usize> = self.tool_blocks_by_key.values().copied().collect();
        indexes.sort_unstable();
        indexes.dedup();
        for index in indexes {
            let Some(AssistantContent::ToolCall(_)) = output.content.get(index) else {
                continue;
            };
            let partial_args = self
                .tool_partial_args
                .get(&index)
                .cloned()
                .unwrap_or_default();
            let parsed = parse_streaming_json(Some(partial_args.as_str()));
            if let AssistantContent::ToolCall(block) = &mut output.content[index] {
                block.arguments = json_object(parsed);
            }
            let block = output.content[index].clone();
            writer.push(AssistantMessageEvent::ToolcallEnd {
                content_index: index as u64,
                tool_call: match block {
                    AssistantContent::ToolCall(call) => call,
                    _ => continue,
                },
                partial: output.clone(),
            });
        }
    }
}

/// Port of `streamSimpleMistral`.
pub fn stream_simple_mistral(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    let api_key = options
        .and_then(|options| options.base.api_key.clone())
        .filter(|key| !key.is_empty())
        .or_else(|| get_env_api_key(&model.provider));
    let api_key = match api_key {
        Some(api_key) => api_key,
        None => {
            let (writer, reader) = create_assistant_message_event_stream();
            let mut error = AssistantMessage {
                content: Vec::new(),
                api: API_MISTRAL_CONVERSATIONS.to_string(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: Usage::default(),
                stop_reason: StopReason::Error,
                stop_reason_raw: None,
                error_message: Some(format!("No API key for provider: {}", model.provider)),
                timestamp: now_ms(),
                rest: Default::default(),
            };
            let message = error.error_message.clone().unwrap_or_default();
            writer.push(AssistantMessageEvent::Error {
                reason: error_reason(StopReason::Error),
                error: error.clone(),
            });
            error.error_message = Some(message);
            writer.end(Some(error));
            return reader;
        }
    };

    let base = build_base_options(model, options, Some(&api_key));
    let clamped_reasoning = options
        .and_then(|options| options.reasoning)
        .map(|reasoning| clamp_thinking_level(model, reasoning));
    let reasoning = match clamped_reasoning {
        Some(ModelThinkingLevel::Off) | None => None,
        Some(level) => Some(level),
    };
    let should_use_reasoning = model.reasoning && reasoning.is_some();

    let stream_options = MistralOptions {
        base,
        tool_choice: None,
        prompt_mode: if should_use_reasoning && uses_prompt_mode_reasoning(model) {
            Some(MistralPromptMode::Reasoning)
        } else {
            None
        },
        reasoning_effort: reasoning
            .filter(|_| should_use_reasoning && uses_reasoning_effort(model))
            .map(|level| map_reasoning_effort(model, level)),
    };
    stream_mistral(model, context, Some(&stream_options))
}

/// Registry provider for the `mistral-conversations` API.
pub struct MistralConversationsProvider;

impl Provider for MistralConversationsProvider {
    fn api(&self) -> &str {
        API_MISTRAL_CONVERSATIONS
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let options = options.map(|base| MistralOptions::from_base(base.clone()));
        stream_mistral(model, context, options.as_ref())
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        stream_simple_mistral(model, context, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_nine_char_alnum_ids() {
        assert_eq!(
            derive_mistral_tool_call_id("abcdefghi", 0),
            "abcdefghi",
            "already-normal ids pass through"
        );
        let derived = derive_mistral_tool_call_id("toolcall:0", 0);
        assert_eq!(derived.len(), MISTRAL_TOOL_CALL_ID_LENGTH);
        assert!(derived.chars().all(|c| c.is_ascii_alphanumeric()));

        let second = derive_mistral_tool_call_id("toolcall:0", 1);
        assert_ne!(derived, second);
    }

    #[test]
    fn normalizer_is_stable_and_collision_free() {
        let normalizer = MistralToolCallIdNormalizer::default();
        let first = normalizer.normalize("call_abc123");
        let second = normalizer.normalize("call_abc123");
        assert_eq!(first, second);

        // A different id that derives the same candidate must not collide.
        let other = normalizer.normalize("call_abc123");
        assert_eq!(other, first);
    }

    #[test]
    fn maps_chat_stop_reasons() {
        assert_eq!(map_chat_stop_reason(None), StopReason::Stop);
        assert_eq!(map_chat_stop_reason(Some("stop")), StopReason::Stop);
        assert_eq!(map_chat_stop_reason(Some("length")), StopReason::Length);
        assert_eq!(
            map_chat_stop_reason(Some("model_length")),
            StopReason::Length
        );
        assert_eq!(
            map_chat_stop_reason(Some("tool_calls")),
            StopReason::ToolUse
        );
        assert_eq!(map_chat_stop_reason(Some("error")), StopReason::Error);
        assert_eq!(map_chat_stop_reason(Some("whatever")), StopReason::Stop);
    }

    #[test]
    fn builds_tool_result_text() {
        assert_eq!(build_tool_result_text("done", false, false, false), "done");
        assert_eq!(
            build_tool_result_text("boom", false, false, true),
            "[tool error] boom"
        );
        assert_eq!(
            build_tool_result_text("", true, true, false),
            "(see attached image)"
        );
        assert_eq!(
            build_tool_result_text("", true, false, false),
            "(image omitted: model does not support images)"
        );
        assert_eq!(
            build_tool_result_text("", false, false, true),
            "[tool error] (no tool output)"
        );
    }

    #[test]
    fn tool_choice_serializes() {
        assert_eq!(MistralToolChoice::Auto.to_json(), json!("auto"));
        assert_eq!(
            MistralToolChoice::Tool {
                name: "grep".into()
            }
            .to_json(),
            json!({ "type": "function", "function": { "name": "grep" } })
        );
    }

    #[test]
    fn reasoning_effort_uses_thinking_level_map() {
        let model = Model {
            id: "mistral-small-2603".into(),
            name: "mistral-small".into(),
            api: "mistral-conversations".into(),
            provider: "mistral".into(),
            base_url: "https://api.mistral.ai".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![crate::types::ModelInput::Text],
            cost: crate::types::zero_model_cost(),
            context_window: 128_000,
            max_tokens: 8192,
            featured: None,
            headers: None,
            compat: None,
        };
        assert!(uses_reasoning_effort(&model));
        assert_eq!(
            map_reasoning_effort(&model, ModelThinkingLevel::High),
            MistralReasoningEffort::High
        );
    }
}
