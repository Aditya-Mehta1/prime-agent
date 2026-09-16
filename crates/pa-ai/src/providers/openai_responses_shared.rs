//! OpenAI Responses API message and tool conversion.
//!
//! Port of the conversion half of
//! `packages/ai/src/providers/openai-responses-shared.ts`: reasoning item
//! replay via thinkingSignature, text signatures, foreign tool-call id
//! normalization. The stream event processor lives in
//! [`crate::providers::openai_responses_stream`].

use serde_json::{json, Map, Value};

use crate::providers::transform_messages::transform_messages_with_normalizer;
use crate::types::{
    AssistantContent, AssistantMessage, Context, Model, ModelExt, TextSignaturePhase, Tool,
};
use crate::utils_inner::hash::short_hash;
use crate::utils_inner::sanitize_unicode::sanitize_surrogates;

pub use crate::providers::openai_responses_hooks::ReasoningSummary;
pub use crate::providers::openai_responses_stream::{
    apply_service_tier_pricing, ResponsesStreamHooks, ResponsesStreamProcessor,
};

pub(crate) fn encode_text_signature_v1(id: &str, phase: Option<TextSignaturePhase>) -> String {
    let mut payload = Map::new();
    payload.insert("v".into(), json!(1));
    payload.insert("id".into(), json!(id));
    if let Some(phase) = phase {
        payload.insert("phase".into(), json!(phase));
    }
    Value::Object(payload).to_string()
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParsedTextSignature {
    pub id: String,
    pub phase: Option<TextSignaturePhase>,
}

fn parse_text_signature(signature: Option<&str>) -> Option<ParsedTextSignature> {
    let signature = signature?;
    if signature.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<Value>(signature) {
            if parsed.get("v") == Some(&json!(1)) {
                if let Some(id) = parsed.get("id").and_then(|value| value.as_str()) {
                    let phase = match parsed.get("phase").and_then(|value| value.as_str()) {
                        Some("commentary") => Some(TextSignaturePhase::Commentary),
                        Some("final_answer") => Some(TextSignaturePhase::FinalAnswer),
                        _ => None,
                    };
                    return Some(ParsedTextSignature {
                        id: id.to_string(),
                        phase,
                    });
                }
            }
        }
        // Fall through to legacy plain-string handling.
    }
    Some(ParsedTextSignature {
        id: signature.to_string(),
        phase: None,
    })
}

// ---------------------------------------------------------------------------
// Message conversion
// ---------------------------------------------------------------------------

/// Providers whose tool-call IDs may carry the `call_id|item_id` Responses
/// encoding natively.
pub const OPENAI_TOOL_CALL_PROVIDERS: [&str; 3] = ["openai", "openai-codex", "opencode"];
pub const AZURE_TOOL_CALL_PROVIDERS: [&str; 4] = [
    "openai",
    "openai-codex",
    "opencode",
    "azure-openai-responses",
];

#[derive(Clone, Copy, Debug, Default)]
pub struct ConvertResponsesMessagesOptions {
    pub include_system_prompt: bool,
}

impl ConvertResponsesMessagesOptions {
    pub fn include_system_prompt() -> Self {
        Self {
            include_system_prompt: true,
        }
    }
}

fn normalize_id_part(part: &str) -> String {
    let sanitized: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut normalized: String = sanitized.chars().take(64).collect();
    while normalized.ends_with('_') {
        normalized.pop();
    }
    normalized
}

fn build_foreign_responses_item_id(item_id: &str) -> String {
    let normalized = format!("fc_{}", short_hash(item_id));
    if normalized.len() > 64 {
        normalized.chars().take(64).collect()
    } else {
        normalized
    }
}

/// Convert a conversation to Responses API `input` items.
pub fn convert_responses_messages(
    model: &Model,
    context: &Context,
    allowed_tool_call_providers: &[&str],
    options: ConvertResponsesMessagesOptions,
) -> Vec<Value> {
    use crate::types::{Message, UserMessageContent, UserOrToolContent};
    let mut messages: Vec<Value> = Vec::new();

    let normalize_tool_call_id = |id: &str, source: &AssistantMessage| -> String {
        if !allowed_tool_call_providers.contains(&model.provider.as_str()) {
            return normalize_id_part(id);
        }
        if !id.contains('|') {
            return normalize_id_part(id);
        }
        let (call_id, item_id) = id.split_once('|').unwrap_or((id, ""));
        let normalized_call_id = normalize_id_part(call_id);
        let is_foreign_tool_call = source.provider != model.provider || source.api != model.api;
        let mut normalized_item_id = if is_foreign_tool_call {
            build_foreign_responses_item_id(item_id)
        } else {
            normalize_id_part(item_id)
        };
        // OpenAI Responses API requires item id to start with "fc".
        if !normalized_item_id.starts_with("fc_") {
            normalized_item_id = normalize_id_part(&format!("fc_{normalized_item_id}"));
        }
        format!("{normalized_call_id}|{normalized_item_id}")
    };

    let transformed =
        transform_messages_with_normalizer(&context.messages, model, &|id, _model, source| {
            Some(normalize_tool_call_id(id, source))
        });

    let include_system_prompt = options.include_system_prompt;
    if include_system_prompt {
        if let Some(system_prompt) = &context.system_prompt {
            let role = if model.reasoning {
                "developer"
            } else {
                "system"
            };
            messages.push(json!({
                "role": role,
                "content": sanitize_surrogates(system_prompt),
            }));
        }
    }

    for msg in &transformed {
        match msg {
            Message::User(user) => match &user.content {
                UserMessageContent::Text(text) => messages.push(json!({
                    "role": "user",
                    "content": [{ "type": "input_text", "text": sanitize_surrogates(text) }],
                })),
                UserMessageContent::Blocks(blocks) => {
                    let content: Vec<Value> = blocks
                        .iter()
                        .map(|item| match crate::types::user_block_payload(item) {
                            crate::types::UserBlockPayload::Text(text) => json!({
                                "type": "input_text",
                                "text": sanitize_surrogates(text),
                            }),
                            crate::types::UserBlockPayload::Image { data, mime_type } => json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!("data:{};base64,{}", mime_type, data),
                            }),
                            crate::types::UserBlockPayload::Opaque(json) => json!({
                                "type": "input_text",
                                "text": sanitize_surrogates(&json),
                            }),
                        })
                        .collect();
                    if content.is_empty() {
                        continue;
                    }
                    messages.push(json!({
                        "role": "user",
                        "content": content,
                    }));
                }
            },
            Message::Assistant(assistant) => {
                let mut output: Vec<Value> = Vec::new();
                let is_different_model = assistant.model != model.id
                    && assistant.provider == model.provider
                    && assistant.api == model.api;

                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(thinking) => {
                            if let Some(signature) = &thinking.thinking_signature {
                                if let Ok(mut reasoning_item) =
                                    serde_json::from_str::<Value>(signature)
                                {
                                    if model.provider == "xai" {
                                        if let Some(item) = reasoning_item.as_object_mut() {
                                            item.remove("status");
                                        }
                                    }
                                    output.push(reasoning_item);
                                }
                            }
                        }
                        AssistantContent::Text(text) => {
                            let parsed_signature =
                                parse_text_signature(text.text_signature.as_deref());
                            // OpenAI requires id to be max 64 characters.
                            let msg_id = match parsed_signature.as_ref() {
                                None => String::new(),
                                Some(signature) => {
                                    if signature.id.len() > 64 {
                                        format!("msg_{}", short_hash(&signature.id))
                                    } else {
                                        signature.id.clone()
                                    }
                                }
                            };
                            let mut entry = Map::new();
                            entry.insert("type".into(), json!("message"));
                            entry.insert("role".into(), json!("assistant"));
                            entry.insert(
                                "content".into(),
                                json!([{
                                    "type": "output_text",
                                    "text": sanitize_surrogates(&text.text),
                                    "annotations": [],
                                }]),
                            );
                            entry.insert("status".into(), json!("completed"));
                            entry.insert("id".into(), json!(msg_id));
                            if let Some(signature) = &parsed_signature {
                                if let Some(phase) = signature.phase {
                                    entry.insert("phase".into(), json!(phase));
                                }
                            }
                            output.push(Value::Object(entry));
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            let (call_id, item_id_raw) = tool_call
                                .id
                                .split_once('|')
                                .unwrap_or((tool_call.id.as_str(), ""));
                            let mut entry = Map::new();
                            entry.insert("type".into(), json!("function_call"));
                            // For different-model messages, set id to null to
                            // avoid pairing validation against rs_ reasoning
                            // items tracked by the provider.
                            let item_id = if is_different_model && item_id_raw.starts_with("fc_") {
                                Value::Null
                            } else {
                                json!(item_id_raw)
                            };
                            entry.insert("id".into(), item_id);
                            entry.insert("call_id".into(), json!(call_id));
                            entry.insert("name".into(), json!(tool_call.name));
                            entry.insert(
                                "arguments".into(),
                                json!(Value::Object(tool_call.arguments.clone()).to_string()),
                            );
                            output.push(Value::Object(entry));
                        }
                    }
                }
                if output.is_empty() {
                    continue;
                }
                messages.extend(output);
            }
            Message::ToolResult(tool_result) => {
                let text_result = tool_result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        UserOrToolContent::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let has_images = tool_result
                    .content
                    .iter()
                    .any(|block| matches!(block, UserOrToolContent::Image(_)));
                let has_text = !text_result.is_empty();
                let call_id = tool_result
                    .tool_call_id
                    .split('|')
                    .next()
                    .unwrap_or_default();

                let output_value = if has_images && model.supports_image_input() {
                    let mut content_parts: Vec<Value> = Vec::new();
                    if has_text {
                        content_parts.push(json!({
                            "type": "input_text",
                            "text": sanitize_surrogates(&text_result),
                        }));
                    }
                    for block in &tool_result.content {
                        if let UserOrToolContent::Image(image) = block {
                            content_parts.push(json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!("data:{};base64,{}", image.mime_type, image.data),
                            }));
                        }
                    }
                    json!(content_parts)
                } else {
                    json!(sanitize_surrogates(if has_text {
                        text_result.as_str()
                    } else if has_images {
                        "(see attached image)"
                    } else {
                        ""
                    }))
                };

                messages.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output_value,
                }));
            }
        }
    }

    messages
}

#[derive(Clone, Copy, Debug)]
pub struct ConvertResponsesToolsOptions {
    pub strict: Option<bool>,
}

/// Convert tool definitions to Responses API tools.
pub fn convert_responses_tools(
    tools: &[Tool],
    options: ConvertResponsesToolsOptions,
) -> Vec<Value> {
    let strict = options.strict.unwrap_or(false);
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
                "strict": strict,
            })
        })
        .collect()
}
