//! OpenAI Chat Completions streaming provider.
//! Full port of `packages/ai/src/providers/openai-completions.ts`: message and
//! tool conversion, compat detection (provider/baseUrl heuristics plus
//! explicit `model.compat`), reasoning replay, usage normalization with
//! cache accounting, SSE chunk parsing with json-parse tolerance, and
//! anthropic-style cache_control injection.

use serde_json::{json, Map, Value};
use std::collections::HashMap;

use crate::cache_pricing::{get_anthropic_cache_write_cost, has_standard_anthropic_cache_pricing};
use crate::env_api_keys::{get_env_api_key, get_prime_team_id};
use crate::event_stream::{
    create_assistant_message_event_stream, AssistantMessageEvent, AssistantMessageEventStream,
    AssistantMessageEventWriter,
};
use crate::models::{calculate_cost, clamp_thinking_level, CostOverrides, ModelThinkingLevelExt};
use crate::registry::Provider;
use crate::types::{
    done_reason, error_reason, AssistantContent, AssistantMessage, CacheRetention, Context,
    MessageExt, Model, ModelExt, ModelInput, ModelThinkingLevel, SimpleStreamOptions, StopReason,
    StreamOptions, TextContent, ThinkingContent, Tool, ToolCall, Usage, UserMessageContent,
    UserOrToolContent,
};
use crate::utils_inner::http::{send, HttpResponse, RequestOptions};
use crate::utils_inner::json_parse::{parse_json_with_repair, parse_streaming_json};
use crate::utils_inner::sanitize_unicode::sanitize_surrogates;
use crate::utils_inner::sse::{ServerSentEvent, SseDecoder};
use crate::utils_inner::stream_failure::{record_stream_failure, ProviderError};

use super::simple_options::build_base_options;
use super::transform_messages::transform_messages_with_normalizer;

pub const API_OPENAI_COMPLETIONS: &str = "openai-completions";

const REASONING_DETAILS_SIGNATURE_TYPE: &str = "openai-completions.reasoning_details.v1";
const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "reasoning_text"];

/// Tool selection passed to the API.
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)] // full TS option surface; variants set by callers
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Function { name: String },
}

impl ToolChoice {
    fn to_json(&self) -> Value {
        match self {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Function { name } => json!({
                "type": "function",
                "function": { "name": name }
            }),
        }
    }
}

/// Provider-native options (`OpenAICompletionsOptions` in the TS reference).
#[derive(Clone, Default)]
pub struct OpenAICompletionsOptions {
    pub base: StreamOptions,
    pub tool_choice: Option<ToolChoice>,
    pub reasoning_effort: Option<ModelThinkingLevel>,
    /// Explicit reasoning toggle. `None` preserves the provider/model default.
    pub reasoning_enabled: Option<bool>,
}

impl OpenAICompletionsOptions {
    pub fn from_base(base: StreamOptions) -> Self {
        Self {
            base,
            tool_choice: None,
            reasoning_effort: None,
            reasoning_enabled: None,
        }
    }
}

/// Anthropic-style cache_control payload on OpenAI-compat proxies.
#[derive(Clone, Debug, PartialEq)]
struct OpenAICompatCacheControl {
    ttl: Option<&'static str>, // Some("1h") or None (default 5m)
}

impl OpenAICompatCacheControl {
    fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("type".into(), json!("ephemeral"));
        if let Some(ttl) = self.ttl {
            map.insert("ttl".into(), json!(ttl));
        }
        Value::Object(map)
    }
}

/// Fully resolved compat settings (`ResolvedOpenAICompletionsCompat`).
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedCompat {
    pub supports_store: bool,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_usage_in_streaming: bool,
    pub max_tokens_field: crate::types::MaxTokensField,
    pub requires_tool_result_name: bool,
    pub requires_assistant_after_tool_result: bool,
    pub requires_thinking_as_text: bool,
    pub requires_reasoning_content_on_assistant_messages: bool,
    pub thinking_format: crate::types::ThinkingFormat,
    pub supports_strict_mode: bool,
    pub cache_control_format: Option<crate::types::CacheControlFormat>,
    pub send_session_affinity_headers: bool,
    pub supports_long_cache_retention: bool,
    pub zai_tool_stream: bool,
    pub open_router_routing: Option<crate::types::OpenRouterRouting>,
    pub vercel_gateway_routing: Option<pa_types::ai::VercelGatewayRouting>,
}

/// Detect compatibility settings from provider and baseUrl for known providers.
/// Provider takes precedence over URL-based detection since it's explicitly configured.
pub fn detect_compat(model: &Model) -> ResolvedCompat {
    let provider = model.provider.as_str();
    let base_url = model.base_url.as_str();

    let is_zai = provider == "zai" || base_url.contains("api.z.ai");
    let is_moonshot = provider == "moonshotai"
        || provider == "moonshotai-cn"
        || base_url.contains("api.moonshot.");
    let is_cloudflare_workers_ai =
        provider == "cloudflare-workers-ai" || base_url.contains("api.cloudflare.com");
    let is_cloudflare_ai_gateway =
        provider == "cloudflare-ai-gateway" || base_url.contains("gateway.ai.cloudflare.com");
    let is_prime_inference =
        provider == "prime-inference" || base_url.contains("api.pinference.ai");

    let is_non_standard = provider == "cerebras"
        || base_url.contains("cerebras.ai")
        || provider == "xai"
        || base_url.contains("api.x.ai")
        || base_url.contains("chutes.ai")
        || base_url.contains("deepseek.com")
        || is_zai
        || is_moonshot
        || provider == "opencode"
        || base_url.contains("opencode.ai")
        || is_cloudflare_workers_ai
        || is_cloudflare_ai_gateway
        || is_prime_inference;

    let use_max_tokens = base_url.contains("chutes.ai")
        || is_moonshot
        || is_cloudflare_ai_gateway
        || is_prime_inference;

    let is_grok = provider == "xai" || base_url.contains("api.x.ai");
    let is_deep_seek = provider == "deepseek" || base_url.contains("deepseek.com");
    let is_anthropic_model = model.id.starts_with("anthropic/");
    let cache_control_format =
        if is_anthropic_model && (provider == "openrouter" || is_prime_inference) {
            Some(crate::types::CacheControlFormat::Anthropic)
        } else {
            None
        };

    ResolvedCompat {
        supports_store: !is_non_standard,
        supports_developer_role: !is_non_standard,
        supports_reasoning_effort: !is_grok && !is_zai && !is_moonshot && !is_cloudflare_ai_gateway,
        supports_usage_in_streaming: true,
        max_tokens_field: if use_max_tokens {
            crate::types::MaxTokensField::MaxTokens
        } else {
            crate::types::MaxTokensField::MaxCompletionTokens
        },
        requires_tool_result_name: false,
        requires_assistant_after_tool_result: false,
        requires_thinking_as_text: false,
        requires_reasoning_content_on_assistant_messages: is_deep_seek,
        thinking_format: if is_deep_seek {
            crate::types::ThinkingFormat::Deepseek
        } else if is_zai {
            crate::types::ThinkingFormat::Zai
        } else if provider == "openrouter" || base_url.contains("openrouter.ai") {
            crate::types::ThinkingFormat::Openrouter
        } else {
            crate::types::ThinkingFormat::Openai
        },
        open_router_routing: None,
        vercel_gateway_routing: None,
        zai_tool_stream: false,
        supports_strict_mode: !is_moonshot && !is_cloudflare_ai_gateway && !is_prime_inference,
        cache_control_format,
        send_session_affinity_headers: false,
        supports_long_cache_retention: !(is_cloudflare_workers_ai || is_cloudflare_ai_gateway),
    }
}

/// Resolve compat for a model: explicit `model.compat` fields override the
/// detected defaults.
pub fn get_compat(model: &Model) -> ResolvedCompat {
    let detected = detect_compat(model);
    let Some(compat) = model.compat_kind() else {
        return detected;
    };
    let crate::types::CompatKind::OpenAiCompletions(compat) = compat else {
        return detected;
    };
    let compat = compat.as_ref();
    ResolvedCompat {
        supports_store: compat.supports_store.unwrap_or(detected.supports_store),
        supports_developer_role: compat
            .supports_developer_role
            .unwrap_or(detected.supports_developer_role),
        supports_reasoning_effort: compat
            .supports_reasoning_effort
            .unwrap_or(detected.supports_reasoning_effort),
        supports_usage_in_streaming: compat
            .supports_usage_in_streaming
            .unwrap_or(detected.supports_usage_in_streaming),
        max_tokens_field: compat.max_tokens_field.unwrap_or(detected.max_tokens_field),
        requires_tool_result_name: compat
            .requires_tool_result_name
            .unwrap_or(detected.requires_tool_result_name),
        requires_assistant_after_tool_result: compat
            .requires_assistant_after_tool_result
            .unwrap_or(detected.requires_assistant_after_tool_result),
        requires_thinking_as_text: compat
            .requires_thinking_as_text
            .unwrap_or(detected.requires_thinking_as_text),
        requires_reasoning_content_on_assistant_messages: compat
            .requires_reasoning_content_on_assistant_messages
            .unwrap_or(detected.requires_reasoning_content_on_assistant_messages),
        thinking_format: compat.thinking_format.unwrap_or(detected.thinking_format),
        open_router_routing: compat
            .open_router_routing
            .clone()
            .or(detected.open_router_routing),
        vercel_gateway_routing: compat
            .vercel_gateway_routing
            .clone()
            .or(detected.vercel_gateway_routing),
        zai_tool_stream: compat.zai_tool_stream.unwrap_or(detected.zai_tool_stream),
        supports_strict_mode: compat
            .supports_strict_mode
            .unwrap_or(detected.supports_strict_mode),
        cache_control_format: compat
            .cache_control_format
            .or(detected.cache_control_format),
        send_session_affinity_headers: compat
            .send_session_affinity_headers
            .unwrap_or(detected.send_session_affinity_headers),
        supports_long_cache_retention: compat
            .supports_long_cache_retention
            .unwrap_or(detected.supports_long_cache_retention),
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

fn get_compat_cache_control(
    compat: &ResolvedCompat,
    cache_retention: CacheRetention,
) -> Option<OpenAICompatCacheControl> {
    if compat.cache_control_format != Some(crate::types::CacheControlFormat::Anthropic)
        || cache_retention == CacheRetention::None
    {
        return None;
    }
    let ttl = if cache_retention == CacheRetention::Long && compat.supports_long_cache_retention {
        Some("1h")
    } else {
        None
    };
    Some(OpenAICompatCacheControl { ttl })
}

fn has_tool_history(messages: &[crate::types::Message]) -> bool {
    use crate::types::Message;
    for msg in messages {
        if let Message::ToolResult(_) = msg {
            return true;
        }
        if let Message::Assistant(assistant) = msg {
            if assistant
                .content
                .iter()
                .any(|block| matches!(block, AssistantContent::ToolCall(_)))
            {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Reasoning details signatures
// ---------------------------------------------------------------------------

fn encode_reasoning_details(details: &[Value]) -> String {
    json!({
        "type": REASONING_DETAILS_SIGNATURE_TYPE,
        "details": details,
    })
    .to_string()
}

fn decode_reasoning_details(signature: Option<&str>) -> Option<Vec<Value>> {
    let signature = signature?;
    if !signature.starts_with('{') {
        return None;
    }
    let parsed: Value = serde_json::from_str(signature).ok()?;
    if parsed.get("type")?.as_str()? != REASONING_DETAILS_SIGNATURE_TYPE {
        return None;
    }
    let details = parsed.get("details")?.as_array()?;
    for detail in details {
        if !detail.is_object() {
            return None;
        }
    }
    Some(details.clone())
}

// ---------------------------------------------------------------------------
// Message conversion
// ---------------------------------------------------------------------------

/// Convert a conversation into Chat Completions `messages` params.
/// Port of `convertMessages` including tool-result bridging and image replay.
pub fn convert_messages(model: &Model, context: &Context, compat: &ResolvedCompat) -> Vec<Value> {
    use crate::types::Message;
    let mut params: Vec<Value> = Vec::new();

    let transformed =
        transform_messages_with_normalizer(&context.messages, model, &|id, model, _| {
            Some(normalize_tool_call_id(id, model))
        });

    if let Some(system_prompt) = &context.system_prompt {
        let use_developer_role = model.reasoning && compat.supports_developer_role;
        let role = if use_developer_role {
            "developer"
        } else {
            "system"
        };
        params.push(json!({
            "role": role,
            "content": sanitize_surrogates(system_prompt),
        }));
    }

    let mut last_role: Option<&'static str> = None;
    let mut index = 0usize;
    while index < transformed.len() {
        let msg = &transformed[index];
        // Some providers don't allow user messages directly after tool results.
        if compat.requires_assistant_after_tool_result
            && last_role == Some("toolResult")
            && matches!(msg, Message::User(_))
        {
            params.push(json!({
                "role": "assistant",
                "content": "I have processed the tool results.",
            }));
        }

        match msg {
            Message::User(user) => match &user.content {
                UserMessageContent::Text(text) => params.push(json!({
                    "role": "user",
                    "content": sanitize_surrogates(text),
                })),
                UserMessageContent::Blocks(blocks) => {
                    let content: Vec<Value> = blocks
                        .iter()
                        .map(|item| match item {
                            UserOrToolContent::Text(text) => json!({
                                "type": "text",
                                "text": sanitize_surrogates(&text.text),
                            }),
                            UserOrToolContent::Image(image) => json!({
                                "type": "image_url",
                                "image_url": { "url": format!("data:{};base64,{}", image.mime_type, image.data) },
                            }),
                        })
                        .collect();
                    if content.is_empty() {
                        index += 1;
                        continue;
                    }
                    params.push(json!({
                        "role": "user",
                        "content": content,
                    }));
                }
            },
            Message::Assistant(assistant) => {
                let mut assistant_msg = Map::new();
                assistant_msg.insert("role".into(), json!("assistant"));
                // Some providers don't accept null content; use empty string instead.
                assistant_msg.insert(
                    "content".into(),
                    if compat.requires_assistant_after_tool_result {
                        json!("")
                    } else {
                        Value::Null
                    },
                );

                let text_blocks: Vec<&TextContent> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Text(text) if !text.text.trim().is_empty() => Some(text),
                        _ => None,
                    })
                    .collect();
                let assistant_text = text_blocks
                    .iter()
                    .map(|block| sanitize_surrogates(&block.text))
                    .collect::<Vec<_>>()
                    .join("");

                let replay_reasoning_details: Vec<Value> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Thinking(thinking) => {
                            decode_reasoning_details(thinking.thinking_signature.as_deref())
                        }
                        _ => None,
                    })
                    .flatten()
                    .collect();
                if !replay_reasoning_details.is_empty() {
                    assistant_msg
                        .insert("reasoning_details".into(), json!(replay_reasoning_details));
                }

                let non_empty_thinking_blocks: Vec<&ThinkingContent> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Thinking(thinking)
                            if decode_reasoning_details(thinking.thinking_signature.as_deref())
                                .is_none()
                                && !thinking.thinking.trim().is_empty() =>
                        {
                            Some(thinking)
                        }
                        _ => None,
                    })
                    .collect();

                if !non_empty_thinking_blocks.is_empty() {
                    if compat.requires_thinking_as_text {
                        let thinking_text = non_empty_thinking_blocks
                            .iter()
                            .map(|block| sanitize_surrogates(&block.thinking))
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        assistant_msg.insert(
                            "content".into(),
                            json!([{
                                "type": "text",
                                "text": thinking_text,
                            }]),
                        );
                        for block in &text_blocks {
                            assistant_msg["content"]
                                .as_array_mut()
                                .expect("content is an array")
                                .push(json!({
                                    "type": "text",
                                    "text": sanitize_surrogates(&block.text),
                                }));
                        }
                    } else {
                        // Always send assistant text as a plain string.
                        if !assistant_text.is_empty() {
                            assistant_msg.insert("content".into(), json!(assistant_text));
                        }

                        let reasoning_text = non_empty_thinking_blocks
                            .iter()
                            .map(|block| sanitize_surrogates(&block.thinking))
                            .collect::<Vec<_>>()
                            .join("\n");
                        let reasoning_field =
                            if compat.requires_reasoning_content_on_assistant_messages {
                                Some("reasoning_content")
                            } else {
                                non_empty_thinking_blocks[0].thinking_signature.as_deref()
                            };
                        match reasoning_field {
                            Some(field) => {
                                assistant_msg.insert(field.to_string(), json!(reasoning_text));
                            }
                            None => {
                                assistant_msg.insert(
                                    "content".into(),
                                    if !assistant_text.is_empty() {
                                        json!(format!("{reasoning_text}\n\n{assistant_text}"))
                                    } else {
                                        json!(reasoning_text)
                                    },
                                );
                            }
                        }
                    }
                } else if !assistant_text.is_empty() {
                    assistant_msg.insert("content".into(), json!(assistant_text));
                }

                let tool_calls: Vec<&ToolCall> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::ToolCall(tool_call) => Some(tool_call),
                        _ => None,
                    })
                    .collect();
                if !tool_calls.is_empty() {
                    assistant_msg.insert(
                        "tool_calls".into(),
                        json!(tool_calls
                            .iter()
                            .map(|tool_call| json!({
                                "id": tool_call.id,
                                "type": "function",
                                "function": {
                                    "name": tool_call.name,
                                    "arguments": serde_json::Value::Object(tool_call.arguments.clone()).to_string(),
                                },
                            }))
                            .collect::<Vec<_>>()),
                    );
                    let reasoning_details: Vec<Value> = tool_calls
                        .iter()
                        .filter_map(|tool_call| {
                            tool_call
                                .thought_signature
                                .as_ref()
                                .and_then(|signature| serde_json::from_str(signature).ok())
                        })
                        .collect();
                    if !reasoning_details.is_empty() && replay_reasoning_details.is_empty() {
                        assistant_msg.insert("reasoning_details".into(), json!(reasoning_details));
                    }
                }
                if compat.requires_reasoning_content_on_assistant_messages
                    && model.reasoning
                    && !assistant_msg.contains_key("reasoning_content")
                {
                    assistant_msg.insert("reasoning_content".into(), json!(""));
                }
                if !replay_reasoning_details.is_empty()
                    && assistant_msg.get("content") == Some(&Value::Null)
                    && !assistant_msg.contains_key("tool_calls")
                {
                    assistant_msg.insert("content".into(), json!(""));
                }
                // Skip assistant messages that have no content and no tool calls.
                let content = assistant_msg.get("content");
                let has_content = match content {
                    Some(Value::Null) | None => false,
                    Some(Value::String(text)) => !text.is_empty(),
                    Some(Value::Array(array)) => !array.is_empty(),
                    Some(_) => true,
                };
                if !has_content
                    && !assistant_msg.contains_key("tool_calls")
                    && replay_reasoning_details.is_empty()
                {
                    index += 1;
                    continue;
                }
                params.push(Value::Object(assistant_msg));
            }
            Message::ToolResult(_) => {
                let mut image_blocks: Vec<Value> = Vec::new();
                let mut j = index;
                while j < transformed.len() {
                    let Message::ToolResult(tool_msg) = &transformed[j] else {
                        break;
                    };

                    let text_result = tool_msg
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            UserOrToolContent::Text(text) => Some(text.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let has_images = tool_msg
                        .content
                        .iter()
                        .any(|block| matches!(block, UserOrToolContent::Image(_)));

                    let has_text = !text_result.is_empty();
                    let mut tool_result_msg = Map::new();
                    tool_result_msg.insert("role".into(), json!("tool"));
                    tool_result_msg.insert(
                        "content".into(),
                        json!(sanitize_surrogates(if has_text {
                            text_result.as_str()
                        } else if has_images {
                            "(see attached image)"
                        } else {
                            ""
                        })),
                    );
                    tool_result_msg.insert("tool_call_id".into(), json!(tool_msg.tool_call_id));
                    if compat.requires_tool_result_name && !tool_msg.tool_name.is_empty() {
                        tool_result_msg.insert("name".into(), json!(tool_msg.tool_name));
                    }
                    params.push(Value::Object(tool_result_msg));

                    if has_images
                        && model
                            .input
                            .iter()
                            .any(|mode| matches!(mode, ModelInput::Image))
                    {
                        for block in &tool_msg.content {
                            if let UserOrToolContent::Image(image) = block {
                                image_blocks.push(json!({
                                    "type": "image_url",
                                    "image_url": { "url": format!("data:{};base64,{}", image.mime_type, image.data) },
                                }));
                            }
                        }
                    }
                    j += 1;
                }

                index = j;
                if !image_blocks.is_empty() {
                    if compat.requires_assistant_after_tool_result {
                        params.push(json!({
                            "role": "assistant",
                            "content": "I have processed the tool results.",
                        }));
                    }
                    let mut content = vec![json!({
                        "type": "text",
                        "text": "Attached image(s) from tool result:",
                    })];
                    content.extend(image_blocks);
                    params.push(json!({
                        "role": "user",
                        "content": content,
                    }));
                    last_role = Some("user");
                } else {
                    last_role = Some("toolResult");
                }
                continue;
            }
        }

        last_role = Some(msg.role());
        index += 1;
    }

    params
}

/// Normalize a tool call ID for providers that reject long/pipe-separated IDs.
pub fn normalize_tool_call_id(id: &str, model: &Model) -> String {
    if id.contains('|') {
        let call_id = id.split('|').next().unwrap_or("");
        call_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(40)
            .collect()
    } else if model.provider == "openai" {
        id.chars().take(40).collect()
    } else {
        id.to_string()
    }
}

pub fn convert_tools(tools: &[Tool], compat: &ResolvedCompat) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let mut function = Map::new();
            function.insert("name".into(), json!(tool.name));
            function.insert("description".into(), json!(tool.description));
            function.insert("parameters".into(), tool.parameters.clone());
            // Only include strict if provider supports it. Some reject unknown fields.
            if compat.supports_strict_mode {
                function.insert("strict".into(), json!(false));
            }
            json!({
                "type": "function",
                "function": Value::Object(function),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

fn build_params(
    model: &Model,
    context: &Context,
    options: Option<&OpenAICompletionsOptions>,
    compat: &ResolvedCompat,
    cache_retention: CacheRetention,
    cache_control: Option<&OpenAICompatCacheControl>,
) -> Value {
    let options = options.cloned().unwrap_or_default();
    let messages = convert_messages(model, context, compat);
    let mut params = Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert("messages".into(), json!(messages));
    params.insert("stream".into(), json!(true));

    let prompt_cache_key = if (model.base_url.contains("api.openai.com")
        && cache_retention != CacheRetention::None)
        || (cache_retention == CacheRetention::Long && compat.supports_long_cache_retention)
    {
        options.base.session_id.clone().map(Value::String)
    } else {
        None
    };
    if let Some(key) = prompt_cache_key {
        params.insert("prompt_cache_key".into(), key);
    }
    if cache_retention == CacheRetention::Long && compat.supports_long_cache_retention {
        params.insert("prompt_cache_retention".into(), json!("24h"));
    }

    if compat.supports_usage_in_streaming {
        params.insert("stream_options".into(), json!({ "include_usage": true }));
    }

    if compat.supports_store {
        params.insert("store".into(), json!(false));
    }

    if let Some(max_tokens) = options.base.max_tokens {
        if compat.max_tokens_field == crate::types::MaxTokensField::MaxTokens {
            params.insert("max_tokens".into(), json!(max_tokens));
        } else {
            params.insert("max_completion_tokens".into(), json!(max_tokens));
        }
    }

    if let Some(temperature) = options.base.temperature {
        params.insert("temperature".into(), json!(temperature));
    }

    let mut tools: Option<Vec<Value>> = None;
    if let Some(context_tools) = &context.tools {
        if !context_tools.is_empty() {
            tools = Some(convert_tools(context_tools, compat));
            if compat.zai_tool_stream {
                params.insert("tool_stream".into(), json!(true));
            }
        }
    }
    if tools.is_none() && has_tool_history(&context.messages) {
        // Anthropic (via LiteLLM/proxy) requires the tools param when the
        // conversation has tool_calls/tool_results.
        tools = Some(Vec::new());
    }
    if let Some(tools) = &tools {
        params.insert("tools".into(), json!(tools));
    }

    if let Some(cache_control) = cache_control {
        apply_anthropic_cache_control(&mut params, cache_control);
    }

    if let Some(tool_choice) = &options.tool_choice {
        params.insert("tool_choice".into(), tool_choice.to_json());
    }

    if model.reasoning {
        match compat.thinking_format {
            crate::types::ThinkingFormat::Zai | crate::types::ThinkingFormat::Qwen => {
                params.insert(
                    "enable_thinking".into(),
                    json!(options.reasoning_effort.is_some()),
                );
            }
            crate::types::ThinkingFormat::QwenChatTemplate => {
                params.insert(
                    "chat_template_kwargs".into(),
                    json!({
                        "enable_thinking": options.reasoning_effort.is_some(),
                        "preserve_thinking": true,
                    }),
                );
            }
            crate::types::ThinkingFormat::Deepseek => {
                params.insert(
                    "thinking".into(),
                    json!({
                        "type": if options.reasoning_effort.is_some() { "enabled" } else { "disabled" },
                    }),
                );
                if let Some(effort) = options.reasoning_effort {
                    let mapped = model
                        .thinking_level_map_value(effort)
                        .flatten()
                        .cloned()
                        .unwrap_or_else(|| effort.wire_name().to_string());
                    params.insert("reasoning_effort".into(), json!(mapped));
                }
            }
            crate::types::ThinkingFormat::Openrouter => {
                if let Some(effort) = options.reasoning_effort {
                    if compat.supports_reasoning_effort {
                        let mapped = model
                            .thinking_level_map_value(effort)
                            .flatten()
                            .cloned()
                            .unwrap_or_else(|| effort.wire_name().to_string());
                        params.insert("reasoning".into(), json!({ "effort": mapped }));
                    }
                } else if options.reasoning_enabled == Some(true) {
                    params.insert("reasoning".into(), json!({ "enabled": true }));
                } else if options.reasoning_enabled == Some(false) {
                    let off_null = model
                        .thinking_level_map_value(ModelThinkingLevel::Off)
                        .map(|value| value.is_none())
                        .unwrap_or(false);
                    if !off_null {
                        if compat.supports_reasoning_effort {
                            let off_value = model
                                .thinking_level_map_value(ModelThinkingLevel::Off)
                                .flatten()
                                .cloned()
                                .unwrap_or_else(|| "none".to_string());
                            params.insert("reasoning".into(), json!({ "effort": off_value }));
                        } else {
                            params.insert("reasoning".into(), json!({ "enabled": false }));
                        }
                    }
                }
            }
            _ => {
                if let Some(effort) = options.reasoning_effort {
                    if compat.supports_reasoning_effort {
                        let mapped = model
                            .thinking_level_map_value(effort)
                            .flatten()
                            .cloned()
                            .unwrap_or_else(|| effort.wire_name().to_string());
                        params.insert("reasoning_effort".into(), json!(mapped));
                    }
                } else if options.reasoning_enabled == Some(false)
                    && compat.supports_reasoning_effort
                {
                    let off = model.thinking_level_map_value(ModelThinkingLevel::Off);
                    let off_null = off.map(|value| value.is_none()).unwrap_or(false);
                    if !off_null {
                        let off_value =
                            off.flatten().cloned().unwrap_or_else(|| "none".to_string());
                        params.insert("reasoning_effort".into(), json!(off_value));
                    }
                }
            }
        }
    }

    if model.base_url.contains("openrouter.ai") {
        if let Some(crate::types::CompatKind::OpenAiCompletions(compat)) = model.compat_kind() {
            if let Some(routing) = &compat.as_ref().open_router_routing {
                params.insert(
                    "provider".into(),
                    serde_json::to_value(routing).unwrap_or(Value::Null),
                );
            }
        }
    }

    if model.base_url.contains("ai-gateway.vercel.sh") {
        if let Some(crate::types::CompatKind::OpenAiCompletions(compat)) = model.compat_kind() {
            if let Some(routing) = &compat.as_ref().vercel_gateway_routing {
                let mut gateway_options = Map::new();
                if let Some(only) = &routing.only {
                    gateway_options.insert("only".into(), json!(only));
                }
                if let Some(order) = &routing.order {
                    gateway_options.insert("order".into(), json!(order));
                }
                if !gateway_options.is_empty() {
                    params.insert(
                        "providerOptions".into(),
                        json!({ "gateway": Value::Object(gateway_options) }),
                    );
                }
            }
        }
    }

    Value::Object(params)
}

fn apply_anthropic_cache_control(
    params: &mut Map<String, Value>,
    cache_control: &OpenAICompatCacheControl,
) {
    // Last tool.
    if let Some(tools) = params
        .get_mut("tools")
        .and_then(|value| value.as_array_mut())
    {
        if let Some(last_tool) = tools.last_mut() {
            last_tool
                .as_object_mut()
                .expect("tools entries are objects")
                .insert("cache_control".into(), cache_control.to_json());
        }
    }
    let messages = match params
        .get_mut("messages")
        .and_then(|value| value.as_array_mut())
    {
        Some(messages) => messages,
        None => return,
    };
    // System prompt.
    for message in messages.iter_mut() {
        let role = message.get("role").and_then(|value| value.as_str());
        if role == Some("system") || role == Some("developer") {
            add_cache_control_to_message(message, cache_control);
            break;
        }
    }
    // Last conversation message (user/assistant/tool), from the end.
    for message in messages.iter_mut().rev() {
        let role = message
            .get("role")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if (role == "user" || role == "assistant" || role == "tool")
            && add_cache_control_to_message(message, cache_control)
        {
            break;
        }
    }
}

fn add_cache_control_to_message(
    message: &mut Value,
    cache_control: &OpenAICompatCacheControl,
) -> bool {
    let cache_json = cache_control.to_json();
    match message.get_mut("content") {
        Some(Value::String(content)) => {
            if content.is_empty() {
                return false;
            }
            let text = content.clone();
            message
                .as_object_mut()
                .expect("messages are objects")
                .insert(
                    "content".into(),
                    json!([{
                        "type": "text",
                        "text": text,
                        "cache_control": cache_json,
                    }]),
                );
            true
        }
        Some(Value::Array(content)) => {
            for part in content.iter_mut().rev() {
                if part.get("type").and_then(|value| value.as_str()) == Some("text") {
                    part.as_object_mut()
                        .expect("text parts are objects")
                        .insert("cache_control".into(), cache_json);
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Usage + stop reason
// ---------------------------------------------------------------------------

fn parse_chunk_usage(raw_usage: &Value, model: &Model, cache_write_cost: Option<f64>) -> Usage {
    let get_u64 = |value: &Value| value.as_u64().unwrap_or(0);
    let prompt_tokens = raw_usage.get("prompt_tokens").map(get_u64).unwrap_or(0);
    let reported_cached_tokens = raw_usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .map(get_u64)
        .or_else(|| raw_usage.get("prompt_cache_hit_tokens").map(get_u64))
        .unwrap_or(0);
    let cache_write_tokens = raw_usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cache_write_tokens"))
        .map(get_u64)
        .unwrap_or(0);

    // Normalize to pi-ai semantics:
    // - cacheRead: hits from cache created by previous requests only
    // - cacheWrite: tokens written to cache in this request
    // Some OpenAI-compatible providers (observed on OpenRouter) report
    // cached_tokens as (previous hits + current writes). Remove cacheWrite from
    // cacheRead in that case.
    let cache_read_tokens = if cache_write_tokens > 0 {
        reported_cached_tokens.saturating_sub(cache_write_tokens)
    } else {
        reported_cached_tokens
    };

    let input = prompt_tokens
        .saturating_sub(cache_read_tokens)
        .saturating_sub(cache_write_tokens);
    // OpenAI completion_tokens already includes reasoning_tokens.
    let output_tokens = raw_usage.get("completion_tokens").map(get_u64).unwrap_or(0);
    let mut usage = Usage {
        input,
        output: output_tokens,
        cache_read: cache_read_tokens,
        cache_write: cache_write_tokens,
        total_tokens: input + output_tokens + cache_read_tokens + cache_write_tokens,
        cost: Default::default(),
    };
    calculate_cost(
        model,
        &mut usage,
        cache_write_cost
            .map(|cache_write| CostOverrides {
                cache_write: Some(cache_write),
            })
            .as_ref(),
    );
    usage
}

fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "function_call" | "tool_calls" => (StopReason::ToolUse, None),
        "content_filter" => (
            StopReason::Error,
            Some("Provider finish_reason: content_filter".to_string()),
        ),
        "network_error" => (
            StopReason::Error,
            Some("Provider finish_reason: network_error".to_string()),
        ),
        other => (
            StopReason::Error,
            Some(format!("Provider finish_reason: {other}")),
        ),
    }
}

// ---------------------------------------------------------------------------
// HTTP request assembly
// ---------------------------------------------------------------------------

fn build_headers(
    model: &Model,
    api_key: &str,
    options_headers: Option<&HashMap<String, String>>,
    cache_session_id: Option<&str>,
    compat: &ResolvedCompat,
    conversation_id: Option<&str>,
) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in model.headers.iter().flatten() {
        headers.push((name.clone(), value.clone()));
    }

    if model.provider == "prime-inference" {
        if let Some(team_id) = get_prime_team_id() {
            headers.push(("X-Prime-Team-ID".into(), team_id));
        }
    }

    if let Some(session_id) = cache_session_id {
        if compat.send_session_affinity_headers {
            headers.push(("session_id".into(), session_id.to_string()));
            headers.push(("x-client-request-id".into(), session_id.to_string()));
            headers.push(("x-session-affinity".into(), session_id.to_string()));
        }
    }

    if let Some(options_headers) = options_headers {
        for (name, value) in options_headers {
            headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
            headers.push((name.clone(), value.clone()));
        }
    }

    headers.insert(0, ("Authorization".into(), format!("Bearer {api_key}")));
    let _ = conversation_id;
    headers
}

// ---------------------------------------------------------------------------
// Streaming core
// ---------------------------------------------------------------------------

struct StreamingState {
    output: AssistantMessage,
    blocks: Vec<AssistantContent>,
    text_block: Option<usize>,
    thinking_block: Option<usize>,
    tool_call_blocks_by_index: HashMap<u64, usize>,
    tool_call_blocks_by_id: HashMap<String, usize>,
    tool_call_partial_args: HashMap<usize, String>,
    reasoning_details_by_index: Vec<(u64, Value)>,
    next_reasoning_details_index: u64,
    reasoning_details_block: Option<usize>,
}

impl StreamingState {
    fn new(output: AssistantMessage) -> Self {
        Self {
            output,
            blocks: Vec::new(),
            text_block: None,
            thinking_block: None,
            tool_call_blocks_by_index: HashMap::new(),
            tool_call_blocks_by_id: HashMap::new(),
            tool_call_partial_args: HashMap::new(),
            reasoning_details_by_index: Vec::new(),
            next_reasoning_details_index: 0,
            reasoning_details_block: None,
        }
    }

    fn ensure_text_block(&mut self, writer: &AssistantMessageEventWriter) -> usize {
        if let Some(index) = self.text_block {
            return index;
        }
        self.blocks.push(AssistantContent::Text(TextContent {
            text: String::new(),
            text_signature: None,
            rest: Default::default(),
        }));
        let index = self.blocks.len() - 1;
        self.text_block = Some(index);
        self.sync_output();
        writer.push(AssistantMessageEvent::TextStart {
            content_index: index as u64,
            partial: self.output.clone(),
        });
        index
    }

    fn ensure_thinking_block(
        &mut self,
        thinking_signature: &str,
        writer: &AssistantMessageEventWriter,
    ) -> usize {
        if let Some(index) = self.thinking_block {
            return index;
        }
        self.blocks
            .push(AssistantContent::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: Some(thinking_signature.to_string()),
                redacted: None,
                rest: Default::default(),
            }));
        let index = self.blocks.len() - 1;
        self.thinking_block = Some(index);
        self.sync_output();
        writer.push(AssistantMessageEvent::ThinkingStart {
            content_index: index as u64,
            partial: self.output.clone(),
        });
        index
    }

    fn ensure_tool_call_block(
        &mut self,
        stream_index: Option<u64>,
        id: Option<&str>,
        writer: &AssistantMessageEventWriter,
    ) -> usize {
        let mut block =
            stream_index.and_then(|index| self.tool_call_blocks_by_index.get(&index).copied());
        if block.is_none() {
            if let Some(id) = id {
                block = self.tool_call_blocks_by_id.get(id).copied();
            }
        }
        if let Some(index) = block {
            if let Some(stream_index) = stream_index {
                self.tool_call_blocks_by_index.insert(stream_index, index);
            }
            if let Some(id) = id {
                self.tool_call_blocks_by_id.insert(id.to_string(), index);
            }
            return index;
        }
        self.blocks.push(AssistantContent::ToolCall(ToolCall {
            id: id.unwrap_or("").to_string(),
            name: String::new(),
            arguments: Default::default(),
            thought_signature: None,
            rest: Default::default(),
        }));
        let index = self.blocks.len() - 1;
        if let Some(stream_index) = stream_index {
            self.tool_call_blocks_by_index.insert(stream_index, index);
        }
        if let Some(id) = id {
            self.tool_call_blocks_by_id.insert(id.to_string(), index);
        }
        self.sync_output();
        writer.push(AssistantMessageEvent::ToolcallStart {
            content_index: index as u64,
            partial: self.output.clone(),
        });
        index
    }

    fn sync_output(&mut self) {
        self.output.content = self.blocks.clone();
    }
}

/// Finish all open blocks, emitting `*_end` events (port of `finishBlock`).
fn finish_blocks(state: &mut StreamingState, writer: &AssistantMessageEventWriter) {
    for index in 0..state.blocks.len() {
        match &state.blocks[index] {
            AssistantContent::Text(text) => writer.push(AssistantMessageEvent::TextEnd {
                content_index: index as u64,
                content: text.text.clone(),
                partial: state.output.clone(),
            }),
            AssistantContent::Thinking(thinking) => {
                writer.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: index as u64,
                    content: thinking.thinking.clone(),
                    partial: state.output.clone(),
                })
            }
            AssistantContent::ToolCall(_) => {
                let partial_args = state.tool_call_partial_args.remove(&index);
                let arguments = partial_args
                    .as_deref()
                    .map(|partial| parse_streaming_json(Some(partial)))
                    .unwrap_or_else(|| json!({}));
                let arguments = arguments.as_object().cloned().unwrap_or_default();
                if let AssistantContent::ToolCall(tool_call) = &mut state.blocks[index] {
                    tool_call.arguments = arguments;
                }
                state.sync_output();
                let tool_call = match &state.blocks[index] {
                    AssistantContent::ToolCall(tool_call) => tool_call.clone(),
                    _ => unreachable!("index points at a tool call"),
                };
                writer.push(AssistantMessageEvent::ToolcallEnd {
                    content_index: index as u64,
                    tool_call,
                    partial: state.output.clone(),
                });
            }
        }
    }
}

/// Handle one parsed SSE chunk. Returns the chunk value for testability.
fn handle_chunk(
    chunk: &Value,
    model: &Model,
    cache_write_cost: Option<f64>,
    state: &mut StreamingState,
    writer: &AssistantMessageEventWriter,
) {
    if !chunk.is_object() {
        return;
    }
    if let Some(id) = chunk.get("id").and_then(|value| value.as_str()) {
        if state.output.response_id.is_none() {
            state.output.response_id = Some(id.to_string());
        }
    }
    if let Some(chunk_model) = chunk.get("model").and_then(|value| value.as_str()) {
        if !chunk_model.is_empty()
            && chunk_model != model.id
            && state.output.response_model.is_none()
        {
            state.output.response_model = Some(chunk_model.to_string());
        }
    }
    if let Some(usage) = chunk.get("usage") {
        if usage.is_object() {
            state.output.usage = parse_chunk_usage(usage, model, cache_write_cost);
        }
    }

    let Some(choice) = chunk
        .get("choices")
        .and_then(|choices| choices.as_array())
        .and_then(|choices| choices.first())
    else {
        return;
    };

    // Fallback: some providers (e.g., Moonshot) return usage in choice.usage.
    if !chunk
        .get("usage")
        .map(|usage| usage.is_object())
        .unwrap_or(false)
    {
        if let Some(usage) = choice.get("usage") {
            if usage.is_object() {
                state.output.usage = parse_chunk_usage(usage, model, cache_write_cost);
            }
        }
    }

    if let Some(finish_reason) = choice.get("finish_reason").and_then(|value| value.as_str()) {
        let (stop_reason, error_message) = map_stop_reason(finish_reason);
        state.output.stop_reason = stop_reason;
        if error_message.is_some() {
            state.output.error_message = error_message;
        }
    }

    let Some(delta) = choice.get("delta").and_then(|value| value.as_object()) else {
        return;
    };

    // Text content.
    if let Some(content) = delta.get("content").and_then(|value| value.as_str()) {
        if !content.is_empty() {
            let index = state.ensure_text_block(writer);
            if let Some(AssistantContent::Text(text)) = state.blocks.get_mut(index) {
                text.text.push_str(content);
            }
            state.sync_output();
            writer.push(AssistantMessageEvent::TextDelta {
                content_index: index as u64,
                delta: content.to_string(),
                partial: state.output.clone(),
            });
        }
    }

    // Some endpoints return reasoning in reasoning_content (llama.cpp),
    // or reasoning (other openai compatible endpoints). Use the first
    // non-empty reasoning field to avoid duplication.
    let mut found_reasoning_field: Option<(&str, &str)> = None;
    for field in REASONING_FIELDS {
        if let Some(value) = delta.get(field).and_then(|value| value.as_str()) {
            if !value.is_empty() {
                found_reasoning_field = Some((field, value));
                break;
            }
        }
    }
    if let Some((field, reasoning_delta)) = found_reasoning_field {
        let index = state.ensure_thinking_block(field, writer);
        if let Some(AssistantContent::Thinking(thinking)) = state.blocks.get_mut(index) {
            thinking.thinking.push_str(reasoning_delta);
        }
        state.sync_output();
        writer.push(AssistantMessageEvent::ThinkingDelta {
            content_index: index as u64,
            delta: reasoning_delta.to_string(),
            partial: state.output.clone(),
        });
    }

    // Tool calls.
    if let Some(tool_calls) = delta.get("tool_calls").and_then(|value| value.as_array()) {
        for tool_call in tool_calls {
            let stream_index = tool_call.get("index").and_then(|value| value.as_u64());
            let id = tool_call.get("id").and_then(|value| value.as_str());
            let index = state.ensure_tool_call_block(stream_index, id, writer);
            if let Some(AssistantContent::ToolCall(block)) = state.blocks.get_mut(index) {
                if block.id.is_empty() {
                    if let Some(id) = id {
                        block.id = id.to_string();
                        state.tool_call_blocks_by_id.insert(id.to_string(), index);
                    }
                }
                if block.name.is_empty() {
                    if let Some(name) = tool_call
                        .get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(|value| value.as_str())
                    {
                        block.name = name.to_string();
                    }
                }
            }
            let mut delta_text = String::new();
            if let Some(arguments) = tool_call
                .get("function")
                .and_then(|function| function.get("arguments"))
                .and_then(|value| value.as_str())
            {
                delta_text = arguments.to_string();
                let entry = state.tool_call_partial_args.entry(index).or_default();
                entry.push_str(arguments);
                if let Some(AssistantContent::ToolCall(block)) = state.blocks.get_mut(index) {
                    block.arguments = parse_streaming_json(Some(entry))
                        .as_object()
                        .cloned()
                        .unwrap_or_default();
                }
            }
            state.sync_output();
            writer.push(AssistantMessageEvent::ToolcallDelta {
                content_index: index as u64,
                delta: delta_text,
                partial: state.output.clone(),
            });
        }
    }

    // Structured reasoning details (e.g., encrypted reasoning + tool binding).
    if let Some(reasoning_details) = delta
        .get("reasoning_details")
        .and_then(|value| value.as_array())
    {
        for detail in reasoning_details {
            if !detail.is_object() {
                continue;
            }
            let explicit_index = detail.get("index").and_then(|value| value.as_u64());
            let index = explicit_index.unwrap_or(state.next_reasoning_details_index);
            state.next_reasoning_details_index = state.next_reasoning_details_index.max(index + 1);
            let previous = state
                .reasoning_details_by_index
                .iter()
                .find(|(existing, _)| *existing == index)
                .map(|(_, value)| value.clone());
            let mut merged = previous.clone().unwrap_or_else(|| json!({}));
            if let (Some(previous), Some(merged_object)) = (previous, merged.as_object_mut()) {
                for (key, value) in detail.as_object().expect("detail is an object") {
                    merged_object.insert(key.clone(), value.clone());
                }
                for field in ["text", "summary"] {
                    let previous_fragment = previous.get(field).and_then(|value| value.as_str());
                    let fragment = detail.get(field).and_then(|value| value.as_str());
                    if let (Some(previous_fragment), Some(fragment)) = (previous_fragment, fragment)
                    {
                        merged_object.insert(
                            field.to_string(),
                            json!(format!("{previous_fragment}{fragment}")),
                        );
                    }
                }
            } else {
                merged = detail.clone();
            }
            state
                .reasoning_details_by_index
                .retain(|(existing, _)| *existing != index);
            state
                .reasoning_details_by_index
                .push((index, merged.clone()));

            if detail.get("type").and_then(|value| value.as_str()) == Some("reasoning.encrypted") {
                if let (Some(id), Some(data)) = (
                    detail.get("id").and_then(|value| value.as_str()),
                    detail.get("data").filter(|value| !value.is_null()),
                ) {
                    let _ = data;
                    for block in state.blocks.iter_mut() {
                        if let AssistantContent::ToolCall(tool_call) = block {
                            if tool_call.id == id {
                                tool_call.thought_signature = Some(detail.to_string());
                            }
                        }
                    }
                }
            }
        }
        if !state.reasoning_details_by_index.is_empty() {
            if state.reasoning_details_block.is_none() {
                state
                    .blocks
                    .push(AssistantContent::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: Some(true),
                        rest: Default::default(),
                    }));
                let index = state.blocks.len() - 1;
                state.reasoning_details_block = Some(index);
                state.sync_output();
                writer.push(AssistantMessageEvent::ThinkingStart {
                    content_index: index as u64,
                    partial: state.output.clone(),
                });
            }
            let mut sorted = state.reasoning_details_by_index.clone();
            sorted.sort_by_key(|(index, _)| *index);
            let details: Vec<Value> = sorted.into_iter().map(|(_, detail)| detail).collect();
            if let Some(index) = state.reasoning_details_block {
                if let Some(AssistantContent::Thinking(thinking)) = state.blocks.get_mut(index) {
                    thinking.thinking_signature = Some(encode_reasoning_details(&details));
                }
            }
        }
    }
}

fn error_to_message(error: &ProviderError) -> String {
    match error {
        ProviderError::StreamFailure(failure) => failure.message.clone(),
        other => other.to_string(),
    }
}

/// Port of `streamOpenAICompletions`.
pub fn stream_openai_completions(
    model: &Model,
    context: &Context,
    options: Option<&OpenAICompletionsOptions>,
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
            timestamp: crate::utils_inner::diagnostics::now_ms(),
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
                output.error_message = Some(error_to_message(&error));
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
    options: Option<&OpenAICompletionsOptions>,
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
    let compat = get_compat(model);
    let cache_retention = resolve_cache_retention(base_options.cache_retention);
    let cache_control = get_compat_cache_control(&compat, cache_retention);
    let cache_write_cost = if cache_control.is_some() && has_standard_anthropic_cache_pricing(model)
    {
        Some(get_anthropic_cache_write_cost(
            model.cost.input.as_f64(),
            if cache_control.as_ref().and_then(|control| control.ttl) == Some("1h") {
                "1h"
            } else {
                "5m"
            },
            None,
        ))
    } else {
        None
    };
    let cache_session_id = if cache_retention == CacheRetention::None {
        None
    } else {
        base_options.session_id.clone()
    };

    let mut params = build_params(
        model,
        context,
        options,
        &compat,
        cache_retention,
        cache_control.as_ref(),
    );
    if let Some(on_payload) = &base_options.on_payload {
        if let Some(next) = on_payload(params.clone(), model) {
            params = next;
        }
    }

    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
    let headers = build_headers(
        model,
        &api_key,
        base_options.headers.as_ref(),
        cache_session_id.as_deref(),
        &compat,
        base_options.session_id.as_deref(),
    );

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

    let mut state = StreamingState::new(output.clone());
    let mut decoder = SseDecoder::new();
    loop {
        let chunk = match response.next_text().await? {
            Some(chunk) => chunk,
            None => break,
        };
        let events = decoder.push_text(&chunk);
        for event in &events {
            if let Some(chunk) = parse_sse_event_data(event) {
                handle_chunk(&chunk, model, cache_write_cost, &mut state, writer);
                output.clone_from(&state.output);
            }
        }
    }
    for event in decoder.finish() {
        if let Some(chunk) = parse_sse_event_data(&event) {
            handle_chunk(&chunk, model, cache_write_cost, &mut state, writer);
        }
    }
    output.clone_from(&state.output);

    finish_blocks(&mut state, writer);
    output.clone_from(&state.output);

    if base_options
        .signal
        .as_ref()
        .map(|signal| signal.is_cancelled())
        .unwrap_or(false)
    {
        return Err(ProviderError::Aborted);
    }
    if output.stop_reason == StopReason::Aborted {
        return Err(ProviderError::Aborted);
    }
    if output.stop_reason == StopReason::Error {
        return Err(ProviderError::Message(
            output
                .error_message
                .clone()
                .unwrap_or_else(|| "Provider returned an error stop reason".to_string()),
        ));
    }

    Ok(())
}

/// Parse the JSON payload of an SSE event; `None` for `[DONE]` and comments.
fn parse_sse_event_data(event: &ServerSentEvent) -> Option<Value> {
    if event.data.trim() == "[DONE]" {
        return None;
    }
    match parse_json_with_repair(&event.data) {
        Ok(value) => Some(value),
        Err(_) => Some(parse_streaming_json(Some(&event.data))),
    }
}

/// Port of `streamSimpleOpenAICompletions`.
pub fn stream_simple_openai_completions(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    let api_key = options
        .and_then(|options| options.base.api_key.clone())
        .or_else(|| get_env_api_key(&model.provider));
    let Some(api_key) = api_key else {
        let (writer, reader) = create_assistant_message_event_stream();
        let mut message = AssistantMessage {
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
            timestamp: crate::utils_inner::diagnostics::now_ms(),
            rest: Default::default(),
        };
        message.usage.cost = Default::default();
        writer.push(AssistantMessageEvent::Error {
            reason: crate::types::ErrorStopReason::Error,
            error: message.clone(),
        });
        writer.end(Some(message));
        return reader;
    };

    let base = build_base_options(model, options, Some(&api_key));
    let requested_reasoning = options.and_then(|options| options.reasoning);
    let reasoning_specified = requested_reasoning.is_some();
    let clamped_reasoning =
        requested_reasoning.map(|reasoning| clamp_thinking_level(model, reasoning));
    let reasoning_effort = clamped_reasoning.filter(|level| *level != ModelThinkingLevel::Off);

    let stream_options = OpenAICompletionsOptions {
        base,
        tool_choice: None,
        reasoning_effort,
        reasoning_enabled: if reasoning_specified {
            Some(clamped_reasoning != Some(ModelThinkingLevel::Off))
        } else {
            None
        },
    };
    stream_openai_completions(model, context, Some(&stream_options))
}

/// Registry provider for the `openai-completions` API.
pub struct OpenAICompletionsProvider;

impl Provider for OpenAICompletionsProvider {
    fn api(&self) -> &str {
        API_OPENAI_COMPLETIONS
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let options = options.map(|base| OpenAICompletionsOptions::from_base(base.clone()));
        stream_openai_completions(model, context, options.as_ref())
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        stream_simple_openai_completions(model, context, options)
    }
}
