//! Anthropic Messages streaming provider.
//! Full port of `packages/ai/src/providers/anthropic.ts`: OAuth/Claude-Code
//! header modes, beta headers, adaptive vs budget-based thinking, SSE event
//! iteration with in-stream error classification, usage accounting from
//! message_start/message_delta including blended cache-write pricing, tool
//! call JSON accumulation, and cache_control injection.

use serde_json::{json, Map, Value};

use crate::cache_pricing::{
    get_anthropic_cache_write_cost, has_standard_anthropic_cache_pricing,
    AnthropicCacheCreationUsage,
};
use crate::env_api_keys::get_env_api_key;
use crate::event_stream::{
    create_assistant_message_event_stream, AssistantMessageEvent, AssistantMessageEventStream,
    AssistantMessageEventWriter,
};
use crate::models::{calculate_cost, clamp_thinking_level, CostOverrides};
use crate::providers::simple_options::{adjust_max_tokens_for_thinking, build_base_options};
use crate::registry::Provider;
use crate::types::{
    done_reason, error_reason, AssistantContent, AssistantMessage, CacheRetention, Context, Model,
    ModelExt, ModelThinkingLevel, SimpleStreamOptions, StopReason, StreamOptions, TextContent,
    ThinkingContent, Tool, ToolCall, Usage, UserMessageContent, UserOrToolContent,
};
use crate::utils_inner::diagnostics::now_ms;
use crate::utils_inner::http::{send, HttpResponse, RequestOptions};
use crate::utils_inner::json_parse::{parse_json_with_repair, parse_streaming_json};
use crate::utils_inner::sanitize_unicode::sanitize_surrogates;
use crate::utils_inner::sse::{ServerSentEvent, SseDecoder};
use crate::utils_inner::stream_failure::{
    classify_stream_failure, format_stream_failure_message, record_stream_failure,
    stream_failure_from_stop_reason, stream_failure_message, truncate_raw_payload, ProviderError,
    StreamFailureError, StreamFailureInfo, StreamFailureKind,
};

pub const API_ANTHROPIC_MESSAGES: &str = "anthropic-messages";

/// Claude Code version mimicked in OAuth mode.
const CLAUDE_CODE_VERSION: &str = "2.1.261";
const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

const CLAUDE_CODE_TOOLS: [&str; 17] = [
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];

fn to_claude_code_name(name: &str) -> String {
    CLAUDE_CODE_TOOLS
        .iter()
        .find(|tool| tool.eq_ignore_ascii_case(name))
        .map(|tool| tool.to_string())
        .unwrap_or_else(|| name.to_string())
}

fn from_claude_code_name(name: &str, tools: Option<&[Tool]>) -> String {
    if let Some(tools) = tools {
        if let Some(matched) = tools
            .iter()
            .find(|tool| tool.name.eq_ignore_ascii_case(name))
        {
            return matched.name.clone();
        }
    }
    name.to_string()
}

/// Effort levels for adaptive thinking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnthropicEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl AnthropicEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            AnthropicEffort::Low => "low",
            AnthropicEffort::Medium => "medium",
            AnthropicEffort::High => "high",
            AnthropicEffort::Xhigh => "xhigh",
            AnthropicEffort::Max => "max",
        }
    }
}

/// Thinking display mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // full TS option surface; variants set by callers
pub enum AnthropicThinkingDisplay {
    Summarized,
    Omitted,
}

impl AnthropicThinkingDisplay {
    pub fn as_str(self) -> &'static str {
        match self {
            AnthropicThinkingDisplay::Summarized => "summarized",
            AnthropicThinkingDisplay::Omitted => "omitted",
        }
    }
}

/// Tool selection passed to the API.
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)] // full TS option surface; variants set by callers
pub enum AnthropicToolChoice {
    Auto,
    Any,
    None,
    Tool { name: String },
}

impl AnthropicToolChoice {
    fn to_json(&self) -> Value {
        match self {
            AnthropicToolChoice::Auto => json!({ "type": "auto" }),
            AnthropicToolChoice::Any => json!({ "type": "any" }),
            AnthropicToolChoice::None => json!({ "type": "none" }),
            AnthropicToolChoice::Tool { name } => json!({ "type": "tool", "name": name }),
        }
    }
}

/// Provider-native options (`AnthropicOptions` in the TS reference).
#[derive(Clone, Default)]
pub struct AnthropicOptions {
    pub base: StreamOptions,
    pub thinking_enabled: Option<bool>,
    pub thinking_budget_tokens: Option<u64>,
    pub effort: Option<AnthropicEffort>,
    pub thinking_display: Option<AnthropicThinkingDisplay>,
    pub interleaved_thinking: Option<bool>,
    pub tool_choice: Option<AnthropicToolChoice>,
}

/// Resolved anthropic compat (`Required<AnthropicMessagesCompat>`).
pub struct ResolvedAnthropicCompat {
    pub supports_eager_tool_input_streaming: bool,
    pub supports_long_cache_retention: bool,
}

pub fn get_anthropic_compat(model: &Model) -> ResolvedAnthropicCompat {
    let compat = model.compat_kind().and_then(|kind| match kind {
        crate::types::CompatKind::AnthropicMessages(compat) => Some(compat),
        _ => None,
    });
    ResolvedAnthropicCompat {
        supports_eager_tool_input_streaming: compat
            .as_ref()
            .and_then(|c| c.supports_eager_tool_input_streaming)
            .unwrap_or(true),
        supports_long_cache_retention: compat
            .as_ref()
            .and_then(|c| c.supports_long_cache_retention)
            .unwrap_or(true),
    }
}

fn resolve_cache_retention(cache_retention: Option<CacheRetention>) -> CacheRetention {
    if let Some(retention) = cache_retention {
        return retention;
    }
    if std::env::var("PI_CACHE_RETENTION").as_deref() == Ok("long") {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

/// `cache_control` payload derived from the retention preference.
pub struct CacheControl {
    pub ttl: Option<&'static str>,
}

impl CacheControl {
    fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("type".into(), json!("ephemeral"));
        if let Some(ttl) = self.ttl {
            map.insert("ttl".into(), json!(ttl));
        }
        Value::Object(map)
    }

    fn duration(&self) -> &'static str {
        if self.ttl == Some("1h") {
            "1h"
        } else {
            "5m"
        }
    }
}

fn get_cache_control(
    model: &Model,
    cache_retention: Option<CacheRetention>,
) -> (CacheRetention, Option<CacheControl>) {
    let retention = resolve_cache_retention(cache_retention);
    if retention == CacheRetention::None {
        return (retention, None);
    }
    let ttl = if retention == CacheRetention::Long
        && get_anthropic_compat(model).supports_long_cache_retention
    {
        Some("1h")
    } else {
        None
    };
    (retention, Some(CacheControl { ttl }))
}

/// Fable/Mythos models think every turn and reject an explicit
/// `thinking: {type: "disabled"}` (and any sampling params) with a 400.
fn is_always_on_adaptive_thinking_model(model_id: &str) -> bool {
    model_id.contains("fable-5")
        || model_id.contains("mythos-5")
        || model_id.contains("mythos-preview")
}

/// Check if a model supports adaptive thinking (Opus 4.6+, Sonnet 4.6+).
fn supports_adaptive_thinking(model_id: &str) -> bool {
    model_id.contains("opus-4-6")
        || model_id.contains("opus-4.6")
        || model_id.contains("opus-5")
        || model_id.contains("opus-4-7")
        || model_id.contains("opus-4.7")
        || model_id.contains("opus-4-8")
        || model_id.contains("opus-4.8")
        || model_id.contains("sonnet-4-6")
        || model_id.contains("sonnet-4.6")
        || model_id.contains("sonnet-5")
        || model_id.contains("fable-5")
        || model_id.contains("mythos-5")
        || model_id.contains("mythos-preview")
}

fn map_thinking_level_to_effort(
    model: &Model,
    level: Option<ModelThinkingLevel>,
) -> AnthropicEffort {
    let effective = level.map(|level| clamp_thinking_level(model, level));
    let mapped = effective.and_then(|level| {
        model
            .thinking_level_map_value(level)
            .and_then(|value| value.cloned())
    });
    if let Some(mapped) = mapped {
        return match mapped.as_str() {
            "low" => AnthropicEffort::Low,
            "medium" => AnthropicEffort::Medium,
            "high" => AnthropicEffort::High,
            "xhigh" => AnthropicEffort::Xhigh,
            "max" => AnthropicEffort::Max,
            _ => AnthropicEffort::High,
        };
    }
    match effective {
        Some(ModelThinkingLevel::Minimal) | Some(ModelThinkingLevel::Low) => AnthropicEffort::Low,
        Some(ModelThinkingLevel::Medium) => AnthropicEffort::Medium,
        Some(ModelThinkingLevel::Xhigh) => AnthropicEffort::Xhigh,
        Some(ModelThinkingLevel::Max) => AnthropicEffort::Max,
        Some(ModelThinkingLevel::High) => AnthropicEffort::High,
        _ => AnthropicEffort::High,
    }
}

fn is_oauth_token(api_key: &str) -> bool {
    api_key.contains("sk-ant-oat")
}

fn should_use_fine_grained_tool_streaming_beta(model: &Model, context: &Context) -> bool {
    context
        .tools
        .as_ref()
        .map(|tools| !tools.is_empty())
        .unwrap_or(false)
        && !get_anthropic_compat(model).supports_eager_tool_input_streaming
}

fn merge_headers(sources: &[Option<Map<String, Value>>]) -> Map<String, Value> {
    let mut merged = Map::new();
    for source in sources.iter().flatten() {
        for (key, value) in source {
            merged.insert(key.clone(), value.clone());
        }
    }
    merged
}

fn headers_to_pairs(headers: &Map<String, Value>) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(key, value)| match value {
            Value::String(text) => Some((key.clone(), text.clone())),
            Value::Null => None,
            _ => None,
        })
        .collect()
}

/// Build the request headers for the messages endpoint, mirroring the SDK
/// client configurations (OAuth/Claude Code mode, cloudflare gateway,
/// github-copilot, plain API key).
fn build_request_headers(
    model: &Model,
    api_key: &str,
    interleaved_thinking: bool,
    use_fine_grained_tool_streaming_beta: bool,
    options_headers: Option<&std::collections::HashMap<String, String>>,
    session_id: Option<&str>,
) -> (Vec<(String, String)>, bool) {
    let is_oauth = is_oauth_token(api_key);
    let needs_interleaved_beta = interleaved_thinking && !supports_adaptive_thinking(&model.id);
    let mut beta_features: Vec<&str> = Vec::new();
    if use_fine_grained_tool_streaming_beta {
        beta_features.push(FINE_GRAINED_TOOL_STREAMING_BETA);
    }
    if needs_interleaved_beta {
        beta_features.push(INTERLEAVED_THINKING_BETA);
    }
    let beta_header = if beta_features.is_empty() {
        None
    } else {
        Some(beta_features.join(","))
    };

    let model_headers: Option<Map<String, Value>> = model.headers.as_ref().map(|headers| {
        headers
            .iter()
            .map(|(key, value)| (key.clone(), json!(value)))
            .collect()
    });
    let options_headers_json: Option<Map<String, Value>> = options_headers.map(|headers| {
        headers
            .iter()
            .map(|(key, value)| (key.clone(), json!(value)))
            .collect()
    });

    let mut default_headers = match model.provider.as_str() {
        "cloudflare-ai-gateway" => {
            let mut headers = Map::new();
            headers.insert("accept".into(), json!("application/json"));
            headers.insert(
                "anthropic-dangerous-direct-browser-access".into(),
                json!("true"),
            );
            headers.insert(
                "cf-aig-authorization".into(),
                json!(format!("Bearer {api_key}")),
            );
            headers.insert("x-api-key".into(), Value::Null);
            headers.insert("Authorization".into(), Value::Null);
            if let Some(beta) = &beta_header {
                headers.insert("anthropic-beta".into(), json!(beta));
            }
            merge_headers(&[
                Some(headers),
                model_headers.clone(),
                options_headers_json.clone(),
            ])
        }
        "github-copilot" => {
            let mut headers = Map::new();
            headers.insert("accept".into(), json!("application/json"));
            headers.insert(
                "anthropic-dangerous-direct-browser-access".into(),
                json!("true"),
            );
            if let Some(beta) = &beta_header {
                headers.insert("anthropic-beta".into(), json!(beta));
            }
            merge_headers(&[
                Some(headers),
                model_headers.clone(),
                options_headers_json.clone(),
            ])
        }
        _ => {
            let mut headers = Map::new();
            headers.insert("accept".into(), json!("application/json"));
            headers.insert(
                "anthropic-dangerous-direct-browser-access".into(),
                json!("true"),
            );
            if is_oauth {
                headers.insert(
                    "anthropic-beta".into(),
                    json!(["claude-code-20250219", "oauth-2025-04-20"]
                        .iter()
                        .chain(beta_features.iter())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(",")),
                );
                headers.insert(
                    "user-agent".into(),
                    json!(format!("claude-cli/{CLAUDE_CODE_VERSION}")),
                );
                headers.insert("x-app".into(), json!("cli"));
            } else if let Some(beta) = &beta_header {
                headers.insert("anthropic-beta".into(), json!(beta));
            }
            merge_headers(&[
                Some(headers),
                model_headers.clone(),
                options_headers_json.clone(),
            ])
        }
    };

    // withOpenCodeHeaders: session header for opencode providers.
    if model.provider == "opencode" || model.provider == "opencode-go" {
        if let Some(session_id) = session_id {
            default_headers.insert("session_id".into(), json!(session_id));
        }
    }

    let mut pairs = headers_to_pairs(&default_headers);
    // The Anthropic SDK always sends the API version; mirror it.
    pairs.insert(0, ("anthropic-version".into(), "2023-06-01".into()));
    if model.provider == "cloudflare-ai-gateway" || model.provider == "github-copilot" || is_oauth {
        pairs.push(("Authorization".into(), format!("Bearer {api_key}")));
    } else {
        pairs.push(("x-api-key".into(), api_key.to_string()));
    }
    (pairs, is_oauth)
}

// ---------------------------------------------------------------------------
// Message / tool conversion
// ---------------------------------------------------------------------------

fn normalize_tool_call_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn convert_content_blocks(content: &[UserOrToolContent]) -> Value {
    let has_images = content
        .iter()
        .any(|block| matches!(block, UserOrToolContent::Image(_)));
    if !has_images {
        let text = content
            .iter()
            .map(|block| match block {
                UserOrToolContent::Text(text) => text.text.clone(),
                UserOrToolContent::Image(image) => image.data.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        return json!(sanitize_surrogates(&text));
    }
    let mut blocks: Vec<Value> = content
        .iter()
        .map(|block| match block {
            UserOrToolContent::Text(text) => json!({
                "type": "text",
                "text": sanitize_surrogates(&text.text),
            }),
            UserOrToolContent::Image(image) => json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": image.mime_type,
                    "data": image.data,
                },
            }),
        })
        .collect();
    let has_text = blocks
        .iter()
        .any(|block| block.get("type").and_then(|value| value.as_str()) == Some("text"));
    if !has_text {
        blocks.insert(0, json!({ "type": "text", "text": "(see attached image)" }));
    }
    json!(blocks)
}

pub fn convert_messages(
    context: &Context,
    model: &Model,
    is_oauth_token: bool,
    cache_control: Option<&CacheControl>,
) -> Vec<Value> {
    use crate::types::Message;
    let mut params: Vec<Value> = Vec::new();
    let transformed = crate::providers::transform_messages::transform_messages_with_normalizer(
        &context.messages,
        model,
        &|id, _model, _source| Some(normalize_tool_call_id(id)),
    );

    let mut index = 0usize;
    while index < transformed.len() {
        let msg = &transformed[index];
        match msg {
            Message::User(user) => match &user.content {
                UserMessageContent::Text(text) => {
                    if !text.trim().is_empty() {
                        params.push(json!({
                            "role": "user",
                            "content": sanitize_surrogates(text),
                        }));
                    }
                }
                UserMessageContent::Blocks(blocks) => {
                    let converted: Vec<Value> = blocks
                        .iter()
                        .map(|item| match item {
                            UserOrToolContent::Text(text) => json!({
                                "type": "text",
                                "text": sanitize_surrogates(&text.text),
                            }),
                            UserOrToolContent::Image(image) => json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": image.mime_type,
                                    "data": image.data,
                                },
                            }),
                        })
                        .collect();
                    let filtered: Vec<Value> = converted
                        .into_iter()
                        .filter(|block| {
                            if block.get("type").and_then(|value| value.as_str()) == Some("text") {
                                block["text"]
                                    .as_str()
                                    .map(|text| !text.trim().is_empty())
                                    .unwrap_or(false)
                            } else {
                                true
                            }
                        })
                        .collect();
                    if filtered.is_empty() {
                        index += 1;
                        continue;
                    }
                    params.push(json!({
                        "role": "user",
                        "content": filtered,
                    }));
                }
            },
            Message::Assistant(assistant) => {
                let mut blocks: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => {
                            if text.text.trim().is_empty() {
                                continue;
                            }
                            blocks.push(json!({
                                "type": "text",
                                "text": sanitize_surrogates(&text.text),
                            }));
                        }
                        AssistantContent::Thinking(thinking) => {
                            if thinking.redacted.unwrap_or(false) {
                                blocks.push(json!({
                                    "type": "redacted_thinking",
                                    "data": thinking.thinking_signature.clone().unwrap_or_default(),
                                }));
                                continue;
                            }
                            if thinking.thinking.trim().is_empty() {
                                continue;
                            }
                            let signature = thinking.thinking_signature.as_deref().unwrap_or("");
                            if signature.trim().is_empty() {
                                blocks.push(json!({
                                    "type": "text",
                                    "text": sanitize_surrogates(&thinking.thinking),
                                }));
                            } else {
                                blocks.push(json!({
                                    "type": "thinking",
                                    "thinking": sanitize_surrogates(&thinking.thinking),
                                    "signature": signature,
                                }));
                            }
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": tool_call.id,
                                "name": if is_oauth_token {
                                    to_claude_code_name(&tool_call.name)
                                } else {
                                    tool_call.name.clone()
                                },
                                "input": Value::Object(tool_call.arguments.clone()),
                            }));
                        }
                    }
                }
                if blocks.is_empty() {
                    index += 1;
                    continue;
                }
                params.push(json!({
                    "role": "assistant",
                    "content": blocks,
                }));
            }
            Message::ToolResult(_tool_result) => {
                // Collect all consecutive toolResult messages into one user turn.
                let mut tool_results: Vec<Value> = Vec::new();
                let mut j = index;
                while j < transformed.len() {
                    let Message::ToolResult(result) = &transformed[j] else {
                        break;
                    };
                    tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": result.tool_call_id,
                        "content": convert_content_blocks(&result.content),
                        "is_error": result.is_error,
                    }));
                    j += 1;
                }
                index = j;
                params.push(json!({
                    "role": "user",
                    "content": tool_results,
                }));
                continue;
            }
        }
        index += 1;
    }

    // Add cache_control to the last user message to cache conversation history.
    if let Some(cache_control) = cache_control {
        if let Some(last) = params.last_mut() {
            if last.get("role").and_then(|value| value.as_str()) == Some("user") {
                let content = &mut last["content"];
                if let Value::String(text) = content {
                    let text = text.clone();
                    *last = json!({
                        "role": "user",
                        "content": [{
                            "type": "text",
                            "text": text,
                            "cache_control": cache_control.to_json(),
                        }],
                    });
                } else if let Value::Array(blocks) = content {
                    if let Some(last_block) = blocks.last_mut() {
                        let block_type = last_block
                            .get("type")
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        if block_type == "text"
                            || block_type == "image"
                            || block_type == "tool_result"
                        {
                            last_block
                                .as_object_mut()
                                .expect("content blocks are objects")
                                .insert("cache_control".into(), cache_control.to_json());
                        }
                    }
                }
            }
        }
    }

    params
}

pub fn convert_tools(
    tools: &[Tool],
    is_oauth_token: bool,
    supports_eager_tool_input_streaming: bool,
    cache_control: Option<&CacheControl>,
) -> Vec<Value> {
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let mut entry = Map::new();
            entry.insert(
                "name".into(),
                json!(if is_oauth_token {
                    to_claude_code_name(&tool.name)
                } else {
                    tool.name.clone()
                }),
            );
            entry.insert("description".into(), json!(tool.description));
            if supports_eager_tool_input_streaming {
                entry.insert("eager_input_streaming".into(), json!(true));
            }
            entry.insert(
                "input_schema".into(),
                json!({
                    "type": "object",
                    "properties": tool.parameters.get("properties").cloned().unwrap_or_else(|| json!({})),
                    "required": tool.parameters.get("required").cloned().unwrap_or_else(|| json!([])),
                }),
            );
            if let Some(cache_control) = cache_control {
                if index == tools.len() - 1 {
                    entry.insert("cache_control".into(), cache_control.to_json());
                }
            }
            Value::Object(entry)
        })
        .collect()
}

fn map_stop_reason(reason: &str) -> Result<StopReason, String> {
    match reason {
        "end_turn" => Ok(StopReason::Stop),
        "max_tokens" => Ok(StopReason::Length),
        "tool_use" => Ok(StopReason::ToolUse),
        "refusal" => Ok(StopReason::Error),
        "pause_turn" => Ok(StopReason::Stop),
        "stop_sequence" => Ok(StopReason::Stop),
        "sensitive" => Ok(StopReason::Error),
        other => Err(format!("Unhandled stop reason: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

fn build_params(
    model: &Model,
    context: &Context,
    is_oauth_token: bool,
    options: Option<&AnthropicOptions>,
    cache_control: Option<&CacheControl>,
) -> Value {
    let base = options
        .map(|options| options.base.clone())
        .unwrap_or_default();
    let mut params = Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert(
        "messages".into(),
        json!(convert_messages(
            context,
            model,
            is_oauth_token,
            cache_control
        )),
    );
    params.insert(
        "max_tokens".into(),
        json!(base.max_tokens.unwrap_or(model.max_tokens / 3)),
    );
    params.insert("stream".into(), json!(true));

    // For OAuth tokens, we MUST include Claude Code identity.
    if is_oauth_token {
        let mut system = vec![json!({
            "type": "text",
            "text": "You are Claude Code, Anthropic's official CLI for Claude.",
        })];
        if let Some(cache_control) = cache_control {
            system[0]
                .as_object_mut()
                .expect("system entry is an object")
                .insert("cache_control".into(), cache_control.to_json());
        }
        if let Some(system_prompt) = &context.system_prompt {
            let mut entry = json!({
                "type": "text",
                "text": sanitize_surrogates(system_prompt),
            });
            if let Some(cache_control) = cache_control {
                entry
                    .as_object_mut()
                    .expect("system entry is an object")
                    .insert("cache_control".into(), cache_control.to_json());
            }
            system.push(entry);
        }
        params.insert("system".into(), json!(system));
    } else if let Some(system_prompt) = &context.system_prompt {
        let mut entry = json!({
            "type": "text",
            "text": sanitize_surrogates(system_prompt),
        });
        if let Some(cache_control) = cache_control {
            entry
                .as_object_mut()
                .expect("system entry is an object")
                .insert("cache_control".into(), cache_control.to_json());
        }
        params.insert("system".into(), json!([entry]));
    }

    // Temperature is incompatible with extended thinking (adaptive or
    // budget-based), and always-on models reject sampling params outright.
    if let Some(temperature) = base.temperature {
        if options.map(|options| options.thinking_enabled) != Some(Some(true))
            && !is_always_on_adaptive_thinking_model(&model.id)
        {
            params.insert("temperature".into(), json!(temperature));
        }
    }

    if let Some(tools) = &context.tools {
        if !tools.is_empty() {
            params.insert(
                "tools".into(),
                json!(convert_tools(
                    tools,
                    is_oauth_token,
                    get_anthropic_compat(model).supports_eager_tool_input_streaming,
                    cache_control,
                )),
            );
        }
    }

    // Configure thinking mode: adaptive, budget-based, or explicitly disabled.
    if model.reasoning {
        if options.map(|options| options.thinking_enabled) == Some(Some(true)) {
            let display = options
                .and_then(|options| options.thinking_display)
                .unwrap_or(AnthropicThinkingDisplay::Summarized);
            if supports_adaptive_thinking(&model.id) {
                params.insert(
                    "thinking".into(),
                    json!({ "type": "adaptive", "display": display.as_str() }),
                );
                if let Some(effort) = options.and_then(|options| options.effort) {
                    params.insert("output_config".into(), json!({ "effort": effort.as_str() }));
                }
            } else {
                params.insert(
                    "thinking".into(),
                    json!({
                        "type": "enabled",
                        "budget_tokens": options.and_then(|options| options.thinking_budget_tokens).unwrap_or(1024),
                        "display": display.as_str(),
                    }),
                );
            }
        } else if options.map(|options| options.thinking_enabled) == Some(Some(false))
            && !is_always_on_adaptive_thinking_model(&model.id)
        {
            params.insert("thinking".into(), json!({ "type": "disabled" }));
        }
    }

    if let Some(metadata) = &base.metadata {
        if let Some(user_id) = metadata.get("user_id").and_then(|value| value.as_str()) {
            params.insert("metadata".into(), json!({ "user_id": user_id }));
        }
    }

    if let Some(tool_choice) = options.and_then(|options| options.tool_choice.as_ref()) {
        params.insert("tool_choice".into(), tool_choice.to_json());
    }

    Value::Object(params)
}

// ---------------------------------------------------------------------------
// SSE iteration + streaming core
// ---------------------------------------------------------------------------

const ANTHROPIC_MESSAGE_EVENTS: [&str; 6] = [
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
];

/// Turn an in-stream `error` SSE event into a classified failure.
fn anthropic_sse_error(data: &str, request_id: Option<&str>) -> StreamFailureError {
    let mut error_type: Option<String> = None;
    let mut detail: Option<String> = None;
    let mut request_id = request_id.map(|id| id.to_string());
    match parse_json_with_repair(data) {
        Ok(parsed) => {
            if let Some(error) = parsed.get("error") {
                error_type = error
                    .get("type")
                    .and_then(|value| value.as_str())
                    .map(|text| text.to_string());
                detail = error
                    .get("message")
                    .and_then(|value| value.as_str())
                    .map(|text| text.to_string());
            }
            if let Some(id) = parsed.get("request_id").and_then(|value| value.as_str()) {
                request_id = Some(id.to_string());
            }
        }
        Err(_) => {
            detail = Some(data.to_string());
        }
    }
    let info = StreamFailureInfo {
        kind: classify_stream_failure(error_type.as_deref(), None),
        provider_error_type: error_type,
        status: None,
        request_id,
        retry_after_ms: None,
        raw: Some(truncate_raw_payload(data)),
    };
    let message = stream_failure_message(&info, detail.as_deref());
    StreamFailureError { message, info }
}

/// Marker error for an unhandled Anthropic stop reason.
struct StopReasonError(String);

/// Streaming block with its wire index.
struct IndexedBlocks {
    blocks: Vec<AssistantContent>,
    indices: Vec<u64>,
    partial_json: Vec<String>,
}

impl IndexedBlocks {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            indices: Vec::new(),
            partial_json: Vec::new(),
        }
    }

    fn position(&self, index: u64) -> Option<usize> {
        self.indices.iter().position(|existing| *existing == index)
    }
}

/// Port of `streamAnthropic`.
pub fn stream_anthropic(
    model: &Model,
    context: &Context,
    options: Option<&AnthropicOptions>,
) -> AssistantMessageEventStream {
    let options = options.cloned();
    let model = model.clone();
    let context = context.clone();
    let (writer, reader) = create_assistant_message_event_stream();

    tokio::spawn(async move {
        let mut output = AssistantMessage {
            content: Vec::new(),
            api: model.api.clone(),
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

async fn run_stream(
    model: &Model,
    context: &Context,
    options: Option<&AnthropicOptions>,
    output: &mut AssistantMessage,
    writer: &AssistantMessageEventWriter,
) -> Result<(), ProviderError> {
    let base_options = options
        .map(|options| options.base.clone())
        .unwrap_or_default();
    let api_key = base_options
        .api_key
        .clone()
        .or_else(|| get_env_api_key(&model.provider))
        .unwrap_or_default();

    let interleaved_thinking = options
        .and_then(|options| options.interleaved_thinking)
        .unwrap_or(true);
    let use_fine_grained = should_use_fine_grained_tool_streaming_beta(model, context);
    let (headers, is_oauth) = build_request_headers(
        model,
        &api_key,
        interleaved_thinking,
        use_fine_grained,
        base_options.headers.as_ref(),
        base_options.session_id.as_deref(),
    );

    let (_retention, cache_control) = get_cache_control(model, base_options.cache_retention);
    let uses_anthropic_cache_pricing = has_standard_anthropic_cache_pricing(model);
    let mut cache_write_cost: Option<f64> = match (&cache_control, uses_anthropic_cache_pricing) {
        (Some(cache_control), true) => Some(get_anthropic_cache_write_cost(
            model.cost.input.as_f64(),
            cache_control.duration(),
            None,
        )),
        _ => None,
    };

    let mut params = build_params(model, context, is_oauth, options, cache_control.as_ref());
    if let Some(on_payload) = &base_options.on_payload {
        if let Some(next) = on_payload(params.clone(), model) {
            params = next;
        }
    }

    let url = format!("{}/v1/messages", model.base_url.trim_end_matches('/'));
    let mut response: HttpResponse = send(RequestOptions {
        method: reqwest::Method::POST,
        url,
        headers,
        body: Some(params.to_string()),
        signal: base_options.signal.clone(),
        timeout_ms: base_options.timeout_ms,
    })
    .await?;

    if let Some(on_response) = &base_options.on_response {
        on_response(
            crate::types::ProviderResponse {
                status: response.status,
                headers: response.headers.clone(),
            },
            model,
        );
    }
    let request_id = response
        .headers
        .get("request-id")
        .or_else(|| response.headers.get("x-request-id"))
        .cloned();

    if response.status >= 400 {
        let body = response.read_all_text().await.unwrap_or_default();
        return Err(ProviderError::from_http_status_body(
            response.status,
            &body,
            response.headers.clone(),
        ));
    }

    writer.push(AssistantMessageEvent::Start {
        partial: output.clone(),
    });

    let mut blocks = IndexedBlocks::new();
    let mut decoder = SseDecoder::new();
    let mut saw_message_start = false;
    let mut saw_message_end = false;

    macro_rules! handle_event {
        ($event:expr) => {{
            let event: Value = $event;
            (|| {
                match event
                    .get("type")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                {
                    "message_start" => {
                        saw_message_start = true;
                        if let Some(message) = event.get("message") {
                            if let Some(id) = message.get("id").and_then(|value| value.as_str()) {
                                output.response_id = Some(id.to_string());
                            }
                            let get = |field: &str| {
                                message
                                    .get(field)
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0)
                            };
                            output.usage.input = get("input_tokens");
                            output.usage.output = get("output_tokens");
                            output.usage.cache_read = get("cache_read_input_tokens");
                            output.usage.cache_write = get("cache_creation_input_tokens");
                            output.usage.total_tokens =
                                crate::types::usage_total_tokens(&output.usage);
                            if cache_control.is_some() && uses_anthropic_cache_pricing {
                                let creation = message
                                    .get("cache_creation")
                                    .and_then(|value| value.as_object())
                                    .map(|map| AnthropicCacheCreationUsage {
                                        ephemeral_5m_input_tokens: map
                                            .get("ephemeral_5m_input_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0),
                                        ephemeral_1h_input_tokens: map
                                            .get("ephemeral_1h_input_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0),
                                    });
                                cache_write_cost = Some(get_anthropic_cache_write_cost(
                                    model.cost.input.as_f64(),
                                    cache_control.as_ref().expect("checked").duration(),
                                    creation.as_ref(),
                                ));
                            }
                            recalculate_cost(model, output, cache_write_cost);
                        }
                    }
                    "content_block_start" => {
                        let index = event
                            .get("index")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0);
                        let content_block =
                            event.get("content_block").cloned().unwrap_or(Value::Null);
                        match content_block
                            .get("type")
                            .and_then(|value| value.as_str())
                            .unwrap_or("")
                        {
                            "text" => {
                                blocks.blocks.push(AssistantContent::Text(TextContent {
                                    text: String::new(),
                                    text_signature: None,
                                    rest: Default::default(),
                                }));
                                blocks.indices.push(index);
                                blocks.partial_json.push(String::new());
                                sync_blocks(output, &blocks);
                                writer.push(AssistantMessageEvent::TextStart {
                                    content_index: (blocks.blocks.len() - 1) as u64,
                                    partial: output.clone(),
                                });
                            }
                            "thinking" => {
                                blocks
                                    .blocks
                                    .push(AssistantContent::Thinking(ThinkingContent {
                                        thinking: String::new(),
                                        thinking_signature: Some(String::new()),
                                        redacted: None,
                                        rest: Default::default(),
                                    }));
                                blocks.indices.push(index);
                                blocks.partial_json.push(String::new());
                                sync_blocks(output, &blocks);
                                writer.push(AssistantMessageEvent::ThinkingStart {
                                    content_index: (blocks.blocks.len() - 1) as u64,
                                    partial: output.clone(),
                                });
                            }
                            "redacted_thinking" => {
                                blocks
                                    .blocks
                                    .push(AssistantContent::Thinking(ThinkingContent {
                                        thinking: "[Reasoning redacted]".to_string(),
                                        thinking_signature: Some(
                                            content_block
                                                .get("data")
                                                .and_then(|value| value.as_str())
                                                .unwrap_or_default()
                                                .to_string(),
                                        ),
                                        redacted: Some(true),
                                        rest: Default::default(),
                                    }));
                                blocks.indices.push(index);
                                blocks.partial_json.push(String::new());
                                sync_blocks(output, &blocks);
                                writer.push(AssistantMessageEvent::ThinkingStart {
                                    content_index: (blocks.blocks.len() - 1) as u64,
                                    partial: output.clone(),
                                });
                            }
                            "tool_use" => {
                                blocks.blocks.push(AssistantContent::ToolCall(ToolCall {
                                    id: content_block
                                        .get("id")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                    name: if is_oauth {
                                        from_claude_code_name(
                                            content_block
                                                .get("name")
                                                .and_then(|value| value.as_str())
                                                .unwrap_or_default(),
                                            context.tools.as_deref(),
                                        )
                                    } else {
                                        content_block
                                            .get("name")
                                            .and_then(|value| value.as_str())
                                            .unwrap_or_default()
                                            .to_string()
                                    },
                                    arguments: content_block
                                        .get("input")
                                        .and_then(|value| value.as_object())
                                        .cloned()
                                        .unwrap_or_default(),
                                    thought_signature: None,
                                    rest: Default::default(),
                                }));
                                blocks.indices.push(index);
                                blocks.partial_json.push(String::new());
                                sync_blocks(output, &blocks);
                                writer.push(AssistantMessageEvent::ToolcallStart {
                                    content_index: (blocks.blocks.len() - 1) as u64,
                                    partial: output.clone(),
                                });
                            }
                            _ => {}
                        }
                    }
                    "content_block_delta" => {
                        let index = event
                            .get("index")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0);
                        let delta = event.get("delta").cloned().unwrap_or(Value::Null);
                        let delta_type = delta
                            .get("type")
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        if let Some(position) = blocks.position(index) {
                            match delta_type {
                                "text_delta" => {
                                    let text = delta
                                        .get("text")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or("");
                                    if let Some(AssistantContent::Text(block)) =
                                        blocks.blocks.get_mut(position)
                                    {
                                        block.text.push_str(text);
                                    }
                                    sync_blocks(output, &blocks);
                                    writer.push(AssistantMessageEvent::TextDelta {
                                        content_index: position as u64,
                                        delta: text.to_string(),
                                        partial: output.clone(),
                                    });
                                }
                                "thinking_delta" => {
                                    let thinking = delta
                                        .get("thinking")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or("");
                                    if let Some(AssistantContent::Thinking(block)) =
                                        blocks.blocks.get_mut(position)
                                    {
                                        block.thinking.push_str(thinking);
                                    }
                                    sync_blocks(output, &blocks);
                                    writer.push(AssistantMessageEvent::ThinkingDelta {
                                        content_index: position as u64,
                                        delta: thinking.to_string(),
                                        partial: output.clone(),
                                    });
                                }
                                "input_json_delta" => {
                                    let partial_json = delta
                                        .get("partial_json")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or("");
                                    if let Some(scratch) = blocks.partial_json.get_mut(position) {
                                        scratch.push_str(partial_json);
                                    }
                                    let parsed = blocks
                                        .partial_json
                                        .get(position)
                                        .map(|partial| parse_streaming_json(Some(partial)))
                                        .unwrap_or_else(|| json!({}));
                                    if let Some(AssistantContent::ToolCall(tool_call)) =
                                        blocks.blocks.get_mut(position)
                                    {
                                        tool_call.arguments =
                                            parsed.as_object().cloned().unwrap_or_default();
                                    }
                                    sync_blocks(output, &blocks);
                                    writer.push(AssistantMessageEvent::ToolcallDelta {
                                        content_index: position as u64,
                                        delta: partial_json.to_string(),
                                        partial: output.clone(),
                                    });
                                }
                                "signature_delta" => {
                                    let signature = delta
                                        .get("signature")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or("");
                                    if let Some(AssistantContent::Thinking(block)) =
                                        blocks.blocks.get_mut(position)
                                    {
                                        let existing = block
                                            .thinking_signature
                                            .get_or_insert_with(String::new);
                                        existing.push_str(signature);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    "content_block_stop" => {
                        let index = event
                            .get("index")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0);
                        if let Some(position) = blocks.position(index) {
                            match &blocks.blocks[position] {
                                AssistantContent::Text(text) => {
                                    writer.push(AssistantMessageEvent::TextEnd {
                                        content_index: position as u64,
                                        content: text.text.clone(),
                                        partial: output.clone(),
                                    })
                                }
                                AssistantContent::Thinking(thinking) => {
                                    writer.push(AssistantMessageEvent::ThinkingEnd {
                                        content_index: position as u64,
                                        content: thinking.thinking.clone(),
                                        partial: output.clone(),
                                    })
                                }
                                AssistantContent::ToolCall(_) => {
                                    let parsed = blocks
                                        .partial_json
                                        .get(position)
                                        .map(|partial| parse_streaming_json(Some(partial)))
                                        .unwrap_or_else(|| json!({}));
                                    if let Some(AssistantContent::ToolCall(tool_call)) =
                                        blocks.blocks.get_mut(position)
                                    {
                                        tool_call.arguments =
                                            parsed.as_object().cloned().unwrap_or_default();
                                    }
                                    sync_blocks(output, &blocks);
                                    let tool_call = match &blocks.blocks[position] {
                                        AssistantContent::ToolCall(tool_call) => tool_call.clone(),
                                        _ => unreachable!("position points at a tool call"),
                                    };
                                    writer.push(AssistantMessageEvent::ToolcallEnd {
                                        content_index: position as u64,
                                        tool_call,
                                        partial: output.clone(),
                                    });
                                }
                            }
                        }
                    }
                    "message_delta" => {
                        if let Some(delta) = event.get("delta") {
                            if let Some(stop_reason) =
                                delta.get("stop_reason").and_then(|value| value.as_str())
                            {
                                match map_stop_reason(stop_reason) {
                                    Ok(mapped) => {
                                        output.stop_reason = mapped;
                                        if mapped == StopReason::Error {
                                            output.stop_reason_raw = Some(stop_reason.to_string());
                                        }
                                    }
                                    Err(message) => return Err(StopReasonError(message)),
                                }
                            }
                        }
                        if let Some(usage) = event.get("usage") {
                            let get =
                                |field: &str| usage.get(field).and_then(|value| value.as_u64());
                            if let Some(input) = get("input_tokens") {
                                output.usage.input = input;
                            }
                            if let Some(out) = get("output_tokens") {
                                output.usage.output = out;
                            }
                            if let Some(cache_read) = get("cache_read_input_tokens") {
                                output.usage.cache_read = cache_read;
                            }
                            if let Some(cache_write) = get("cache_creation_input_tokens") {
                                output.usage.cache_write = cache_write;
                            }
                            if cache_control.is_some() && uses_anthropic_cache_pricing {
                                if let Some(creation) = usage
                                    .get("cache_creation")
                                    .and_then(|value| value.as_object())
                                {
                                    let creation = AnthropicCacheCreationUsage {
                                        ephemeral_5m_input_tokens: creation
                                            .get("ephemeral_5m_input_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0),
                                        ephemeral_1h_input_tokens: creation
                                            .get("ephemeral_1h_input_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0),
                                    };
                                    cache_write_cost = Some(get_anthropic_cache_write_cost(
                                        model.cost.input.as_f64(),
                                        cache_control.as_ref().expect("checked").duration(),
                                        Some(&creation),
                                    ));
                                }
                            }
                            output.usage.total_tokens =
                                crate::types::usage_total_tokens(&output.usage);
                            recalculate_cost(model, output, cache_write_cost);
                        }
                    }
                    "message_stop" => {
                        saw_message_end = true;
                    }
                    _ => {}
                }
                Ok::<(), StopReasonError>(())
            })()
        }};
    }

    loop {
        let chunk = match response.next_text().await? {
            Some(chunk) => chunk,
            None => break,
        };
        for sse in decoder.push_text(&chunk) {
            handle_sse(&sse, request_id.as_deref(), |event| {
                handle_event!(event).map_err(|error| error.0)
            })?;
        }
    }
    for sse in decoder.finish() {
        handle_sse(&sse, request_id.as_deref(), |event| {
            handle_event!(event).map_err(|error| error.0)
        })?;
    }
    if saw_message_start && !saw_message_end {
        return Err(ProviderError::StreamFailure(StreamFailureError {
            message: "Anthropic stream ended before message_stop".to_string(),
            info: StreamFailureInfo {
                kind: StreamFailureKind::MalformedResponse,
                request_id,
                ..StreamFailureInfo::unknown()
            },
        }));
    }

    sync_blocks(output, &blocks);

    if base_options
        .signal
        .as_ref()
        .map(|signal| signal.is_cancelled())
        .unwrap_or(false)
    {
        return Err(ProviderError::Aborted);
    }
    if matches!(output.stop_reason, StopReason::Aborted | StopReason::Error) {
        return Err(ProviderError::StreamFailure(
            stream_failure_from_stop_reason(
                output.stop_reason_raw.as_deref(),
                request_id.as_deref(),
            ),
        ));
    }
    if output.stop_reason == StopReason::Length {
        // "length" is a successful stop.
    }

    Ok(())
}

fn sync_blocks(output: &mut AssistantMessage, blocks: &IndexedBlocks) {
    output.content = blocks.blocks.clone();
}

fn recalculate_cost(model: &Model, output: &mut AssistantMessage, cache_write_cost: Option<f64>) {
    let overrides = cache_write_cost.map(|cache_write| CostOverrides {
        cache_write: Some(cache_write),
    });
    calculate_cost(model, &mut output.usage, overrides.as_ref());
}

/// Handle one SSE event: error events become classified failures; message
/// events are parsed and forwarded.
fn handle_sse<E>(
    sse: &ServerSentEvent,
    request_id: Option<&str>,
    mut on_event: E,
) -> Result<(), ProviderError>
where
    E: FnMut(Value) -> Result<(), String>,
{
    if sse.event.as_deref() == Some("error") {
        return Err(ProviderError::StreamFailure(anthropic_sse_error(
            &sse.data, request_id,
        )));
    }
    if !ANTHROPIC_MESSAGE_EVENTS.contains(&sse.event.as_deref().unwrap_or("")) {
        return Ok(());
    }
    let event = match parse_json_with_repair(&sse.data) {
        Ok(event) => event,
        Err(error) => {
            return Err(ProviderError::StreamFailure(StreamFailureError {
                message: format!(
                    "Could not parse Anthropic SSE event {}: {error}; data={}; raw={}",
                    sse.event.as_deref().unwrap_or_default(),
                    sse.data,
                    sse.raw.join("\\n")
                ),
                info: StreamFailureInfo {
                    kind: StreamFailureKind::MalformedResponse,
                    request_id: request_id.map(|id| id.to_string()),
                    raw: Some(truncate_raw_payload(&sse.data)),
                    ..StreamFailureInfo::unknown()
                },
            }));
        }
    };
    on_event(event).map_err(ProviderError::Message)?;
    Ok(())
}

/// Port of `streamSimpleAnthropic`.
pub fn stream_simple_anthropic(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    let api_key = options
        .and_then(|options| options.base.api_key.clone())
        .or_else(|| get_env_api_key(&model.provider));
    let Some(api_key) = api_key else {
        let (writer, reader) = create_assistant_message_event_stream();
        let message = AssistantMessage {
            content: Vec::new(),
            api: model.api.clone(),
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
        writer.push(AssistantMessageEvent::Error {
            reason: crate::types::ErrorStopReason::Error,
            error: message.clone(),
        });
        writer.end(Some(message));
        return reader;
    };

    let base = build_base_options(model, options, Some(&api_key));
    let reasoning = options.and_then(|options| options.reasoning);
    if reasoning.is_none() || reasoning == Some(ModelThinkingLevel::Off) {
        let mut anthropic_options = AnthropicOptions::from_base(base);
        anthropic_options.thinking_enabled = Some(false);
        return stream_anthropic(model, context, Some(&anthropic_options));
    }

    // Adaptive thinking models use effort; older models use budgets.
    if supports_adaptive_thinking(&model.id) {
        let effort = map_thinking_level_to_effort(model, reasoning);
        let mut anthropic_options = AnthropicOptions::from_base(base);
        anthropic_options.thinking_enabled = Some(true);
        anthropic_options.effort = Some(effort);
        return stream_anthropic(model, context, Some(&anthropic_options));
    }

    let budgets = options.and_then(|options| options.thinking_budgets.as_ref());
    let adjusted = match adjust_max_tokens_for_thinking(
        base.max_tokens.unwrap_or(0),
        model.max_tokens,
        reasoning.expect("checked above"),
        budgets,
    ) {
        Ok(adjusted) => adjusted,
        Err(message) => {
            let (writer, reader) = create_assistant_message_event_stream();
            let message = AssistantMessage {
                content: Vec::new(),
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: Usage::default(),
                stop_reason: StopReason::Error,
                stop_reason_raw: None,
                error_message: Some(message),
                timestamp: now_ms(),
                rest: Default::default(),
            };
            writer.push(AssistantMessageEvent::Error {
                reason: crate::types::ErrorStopReason::Error,
                error: message.clone(),
            });
            writer.end(Some(message));
            return reader;
        }
    };
    let mut anthropic_options = AnthropicOptions::from_base(base);
    anthropic_options.base.max_tokens = Some(adjusted.0);
    anthropic_options.thinking_enabled = Some(true);
    anthropic_options.thinking_budget_tokens = Some(adjusted.1);
    stream_anthropic(model, context, Some(&anthropic_options))
}

impl AnthropicOptions {
    pub fn from_base(base: StreamOptions) -> Self {
        Self {
            base,
            thinking_enabled: None,
            thinking_budget_tokens: None,
            effort: None,
            thinking_display: None,
            interleaved_thinking: None,
            tool_choice: None,
        }
    }
}

/// Registry provider for the `anthropic-messages` API.
pub struct AnthropicMessagesProvider;

impl Provider for AnthropicMessagesProvider {
    fn api(&self) -> &str {
        API_ANTHROPIC_MESSAGES
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let options = options.map(|base| AnthropicOptions::from_base(base.clone()));
        stream_anthropic(model, context, options.as_ref())
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        stream_simple_anthropic(model, context, options)
    }
}
