//! Amazon Bedrock Converse Stream provider.
//!
//! Port of `packages/ai/src/providers/amazon-bedrock.ts` as a raw-HTTP
//! implementation: SigV4-signed `POST /model/{modelId}/converse-stream`,
//! binary `vnd.amazon.eventstream` response decoding (see [`eventstream`]),
//! message conversion and cache points (see [`convert`]), and credential /
//! region resolution (see [`auth`]). Supports bearer-token auth, SigV4 skip
//! for local gateways, Claude adaptive vs budget-based thinking, and
//! GovCloud-safe request fields.

use std::collections::HashMap;

use serde_json::{json, Value};

mod auth;
mod convert;
mod events;
mod eventstream;

use crate::env_api_keys::get_env_api_key;
use crate::event_stream::{
    create_assistant_message_event_stream, AssistantMessageEvent, AssistantMessageEventStream,
    AssistantMessageEventWriter,
};
use crate::providers::bedrock::auth::{resolve_credentials, resolve_endpoint, sigv4_headers};
use crate::providers::bedrock::convert::{
    convert_messages, convert_tool_config, is_anthropic_claude_model, map_stop_reason,
    map_thinking_level_to_effort, supports_adaptive_thinking, supports_always_on_adaptive_thinking,
    BedrockToolChoice,
};
use crate::providers::bedrock::events::{handle_event, BedrockStreamState};
use crate::providers::bedrock::eventstream::EventStreamDecoder;
use crate::providers::simple_options::{build_base_options, clamp_reasoning};
use crate::registry::Provider;
use crate::types::{
    done_reason, error_reason, AssistantMessage, CacheRetention, Context, Model,
    ModelThinkingLevel, SimpleStreamOptions, StopReason, StreamOptions, ThinkingBudgets, Usage,
};
use crate::utils_inner::diagnostics::now_ms;
use crate::utils_inner::http::{send, HttpResponse, RequestOptions};
use crate::utils_inner::stream_failure::{
    format_stream_failure_message, record_stream_failure, stream_failure_from_stop_reason,
    ProviderError,
};

pub const API_BEDROCK_CONVERSE_STREAM: &str = "bedrock-converse-stream";

/// How Claude's thinking content is returned (`thinkingDisplay` in the TS).
#[allow(dead_code)] // full TS option surface; variants set by callers
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BedrockThinkingDisplay {
    Summarized,
    Omitted,
}

impl BedrockThinkingDisplay {
    fn as_str(self) -> &'static str {
        match self {
            BedrockThinkingDisplay::Summarized => "summarized",
            BedrockThinkingDisplay::Omitted => "omitted",
        }
    }
}

/// Provider-specific request options (`BedrockOptions` in the TS reference).
#[derive(Clone, Default)]
pub struct BedrockOptions {
    pub base: StreamOptions,
    pub region: Option<String>,
    pub profile: Option<String>,
    pub tool_choice: Option<BedrockToolChoice>,
    pub reasoning: Option<ModelThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub interleaved_thinking: Option<bool>,
    pub thinking_display: Option<BedrockThinkingDisplay>,
    pub request_metadata: Option<HashMap<String, String>>,
    pub bearer_token: Option<String>,
}

impl BedrockOptions {
    pub fn from_base(base: StreamOptions) -> Self {
        Self {
            base,
            ..Default::default()
        }
    }
}

/// Human-readable prefixes for Bedrock SDK exception names (see the TS
/// comment: the downstream retry logic in agent-session matches patterns like
/// `server.?error`, so the legacy prefix format is preserved).
const BEDROCK_ERROR_PREFIXES: [(&str, &str); 5] = [
    ("InternalServerException", "Internal server error"),
    ("ModelStreamErrorException", "Model stream error"),
    ("ValidationException", "Validation error"),
    ("ThrottlingException", "Throttling error"),
    ("ServiceUnavailableException", "Service unavailable"),
];

pub(crate) fn bedrock_error_prefix(exception_name: &str) -> String {
    for (name, prefix) in BEDROCK_ERROR_PREFIXES {
        if name == exception_name {
            return prefix.to_string();
        }
    }
    exception_name.to_string()
}

/// Port of `streamBedrock`.
pub fn stream_bedrock(
    model: &Model,
    context: &Context,
    options: Option<&BedrockOptions>,
) -> AssistantMessageEventStream {
    let options = options.cloned();
    let model = model.clone();
    let context = context.clone();
    let (writer, reader) = create_assistant_message_event_stream();

    tokio::spawn(async move {
        let mut output = AssistantMessage {
            content: Vec::new(),
            api: API_BEDROCK_CONVERSE_STREAM.to_string(),
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

/// Port of `resolveCacheRetention`.
fn resolve_cache_retention(cache_retention: Option<CacheRetention>) -> CacheRetention {
    if let Some(retention) = cache_retention {
        return retention;
    }
    if std::env::var("PI_CACHE_RETENTION").as_deref() == Ok("long") {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

/// Port of `isGovCloudBedrockTarget`.
fn is_gov_cloud_bedrock_target(model: &Model, options: &BedrockOptions) -> bool {
    if options
        .region
        .as_deref()
        .map(|region| region.to_lowercase().starts_with("us-gov-"))
        .unwrap_or(false)
    {
        return true;
    }
    let model_id = model.id.to_lowercase();
    model_id.starts_with("us-gov.") || model_id.starts_with("arn:aws-us-gov:")
}

/// Port of `buildAdditionalModelRequestFields`.
fn build_additional_model_request_fields(model: &Model, options: &BedrockOptions) -> Option<Value> {
    let reasoning = options.reasoning?;
    if !model.reasoning {
        return None;
    }

    if is_anthropic_claude_model(model) {
        // GovCloud Bedrock currently rejects the Claude thinking.display field.
        // Omit it there until the GovCloud Converse schema catches up.
        let display = if is_gov_cloud_bedrock_target(model, options) {
            None
        } else {
            Some(
                options
                    .thinking_display
                    .unwrap_or(BedrockThinkingDisplay::Summarized)
                    .as_str(),
            )
        };
        let mut result = if supports_adaptive_thinking(&model.id, Some(&model.name)) {
            let mut thinking = Map::new();
            thinking.insert("type".into(), json!("adaptive"));
            if let Some(display) = display {
                thinking.insert("display".into(), json!(display));
            }
            json!({
                "thinking": Value::Object(thinking),
                "output_config": {
                    "effort": map_thinking_level_to_effort(model, reasoning),
                }
            })
        } else {
            const DEFAULT_BUDGETS: [(ModelThinkingLevel, u64); 6] = [
                (ModelThinkingLevel::Minimal, 1024),
                (ModelThinkingLevel::Low, 2048),
                (ModelThinkingLevel::Medium, 8192),
                (ModelThinkingLevel::High, 16384),
                // Budget-based Claude has no xhigh tier, clamp to high
                (ModelThinkingLevel::Xhigh, 16384),
                // Budget-based Claude has no max tier, clamp to high
                (ModelThinkingLevel::Max, 16384),
            ];
            // Custom budgets are keyed by the clamped level; xhigh/max
            // resolve through the `high` entry, matching the TS.
            let clamped_level = clamp_reasoning(reasoning);
            let custom_budget =
                options
                    .thinking_budgets
                    .as_ref()
                    .and_then(|budgets| match clamped_level {
                        ModelThinkingLevel::Minimal => budgets.minimal,
                        ModelThinkingLevel::Low => budgets.low,
                        ModelThinkingLevel::Medium => budgets.medium,
                        _ => budgets.high,
                    });
            let budget = custom_budget.or_else(|| {
                DEFAULT_BUDGETS
                    .iter()
                    .find(|(level, _)| *level == reasoning)
                    .map(|(_, budget)| *budget)
            });
            let mut thinking = Map::new();
            thinking.insert("type".into(), json!("enabled"));
            thinking.insert("budget_tokens".into(), json!(budget));
            if let Some(display) = display {
                thinking.insert("display".into(), json!(display));
            }
            json!({ "thinking": Value::Object(thinking) })
        };

        if !supports_adaptive_thinking(&model.id, Some(&model.name))
            && options.interleaved_thinking.unwrap_or(true)
        {
            result["anthropic_beta"] = json!(["interleaved-thinking-2025-05-14"]);
        }

        return Some(result);
    }

    None
}

use serde_json::Map;

/// Percent-encode the model id for the `/model/{modelId}/converse-stream`
/// path, matching the SDK's URI-component encoding of path labels.
fn encode_model_id(model_id: &str) -> String {
    let mut encoded = String::new();
    for byte in model_id.bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            encoded.push(c);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

async fn run_stream(
    model: &Model,
    context: &Context,
    options: Option<&BedrockOptions>,
    output: &mut AssistantMessage,
    writer: &AssistantMessageEventWriter,
) -> Result<(), ProviderError> {
    let options = options.cloned().unwrap_or_default();
    let _ = get_env_api_key(&model.provider); // Bedrock auth never uses provider env keys

    if options
        .base
        .signal
        .as_ref()
        .map(|signal| signal.is_cancelled())
        .unwrap_or(false)
    {
        return Err(ProviderError::Aborted);
    }

    let cache_retention = resolve_cache_retention(options.base.cache_retention);

    let mut inference_config = Map::new();
    if let Some(max_tokens) = options.base.max_tokens {
        inference_config.insert("maxTokens".into(), json!(max_tokens));
    }
    if let Some(temperature) = options.base.temperature {
        if !supports_always_on_adaptive_thinking(&model.id, Some(&model.name)) {
            inference_config.insert("temperature".into(), json!(temperature));
        }
    }

    let mut command_input = Map::new();
    command_input.insert("modelId".into(), json!(model.id));
    command_input.insert(
        "messages".into(),
        json!(convert_messages(context, model, cache_retention)),
    );
    if let Some(system) =
        build_system_prompt_blocks(context.system_prompt.as_deref(), model, cache_retention)
    {
        command_input.insert("system".into(), json!(system));
    }
    if !inference_config.is_empty() {
        command_input.insert("inferenceConfig".into(), Value::Object(inference_config));
    }
    if let Some(tool_config) =
        convert_tool_config(context.tools.as_deref(), options.tool_choice.as_ref())
    {
        command_input.insert("toolConfig".into(), tool_config);
    }
    if let Some(fields) = build_additional_model_request_fields(model, &options) {
        command_input.insert("additionalModelRequestFields".into(), fields);
    }
    if let Some(metadata) = &options.request_metadata {
        command_input.insert("requestMetadata".into(), json!(metadata));
    }

    let mut payload = Value::Object(command_input);
    if let Some(on_payload) = &options.base.on_payload {
        if let Some(next) = on_payload(payload.clone(), model) {
            payload = next;
        }
    }

    let (endpoint, region) = resolve_endpoint(model, &options);
    let path_and_query = format!("/model/{}/converse-stream", encode_model_id(&model.id));
    let host = url::Url::parse(&endpoint)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .ok_or_else(|| ProviderError::Message(format!("Invalid Bedrock endpoint: {endpoint}")))?;
    let url = format!("{endpoint}{path_and_query}");

    let mut headers: Vec<(String, String)> = vec![
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "application/json".into()),
    ];
    if let Some((name, value)) = model.headers.iter().flatten().next() {
        let _ = (name, value);
    }

    // requestMetadata also travels as the X-Amzn-Bedrock-Request-Metadata header.
    let mut extra_signed_headers: Vec<(String, String)> = Vec::new();
    if let Some(metadata) = &options.request_metadata {
        extra_signed_headers.push((
            "x-amzn-bedrock-request-metadata".into(),
            serde_json::to_string(metadata).unwrap_or_default(),
        ));
    }

    // Bearer-token auth bypasses SigV4 entirely.
    let bearer_token = options
        .bearer_token
        .clone()
        .or_else(|| std::env::var("AWS_BEARER_TOKEN_BEDROCK").ok())
        .filter(|token| !token.is_empty());
    let use_bearer =
        bearer_token.is_some() && std::env::var("AWS_BEDROCK_SKIP_AUTH").as_deref() != Ok("1");

    if use_bearer {
        headers.push((
            "authorization".into(),
            format!("Bearer {}", bearer_token.expect("checked above")),
        ));
        for (name, value) in &extra_signed_headers {
            headers.push((name.clone(), value.clone()));
        }
    } else {
        let credentials = resolve_credentials(options.profile.as_deref()).ok_or_else(|| {
            ProviderError::Message("No AWS credentials available for Bedrock".to_string())
        })?;
        let (amz_date, authorization, security_token) = sigv4_headers(
            &crate::providers::bedrock::auth::SigV4Params {
                method: "POST",
                path_and_query: &path_and_query,
                host: &host,
                region: &region,
                service: "bedrock",
                body: payload.to_string().as_bytes(),
                extra_signed_headers: &extra_signed_headers,
            },
            &credentials,
        );
        headers.push(("x-amz-date".into(), amz_date));
        headers.push(("authorization".into(), authorization));
        if let Some(security_token) = security_token {
            headers.push(("x-amz-security-token".into(), security_token));
        }
        for (name, value) in &extra_signed_headers {
            headers.push((name.clone(), value.clone()));
        }
    }

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
        return Err(ProviderError::from_http_status_body(
            response.status,
            &body,
            response.headers.clone(),
        ));
    }

    let request_id = response
        .headers
        .get("x-amzn-requestid")
        .or_else(|| response.headers.get("x-amzn-request-id"))
        .cloned();

    let mut state = BedrockStreamState::new();
    let mut decoder = EventStreamDecoder::new();
    let mut stream_error: Option<ProviderError> = None;
    loop {
        let chunk = match response.next_bytes().await? {
            Some(chunk) => chunk,
            None => break,
        };
        for message in decoder.push(&chunk) {
            match handle_event(&message, model, output, writer, &mut state, &request_id) {
                Ok(()) => {}
                Err(error) => {
                    stream_error = Some(error);
                    break;
                }
            }
        }
        if stream_error.is_some() {
            break;
        }
    }
    if let Some(error) = stream_error {
        return Err(error);
    }

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
            stream_failure_from_stop_reason(
                output.stop_reason_raw.as_deref(),
                request_id.as_deref(),
            ),
        ));
    }

    Ok(())
}

use crate::providers::bedrock::convert::build_system_prompt as build_system_prompt_blocks;

/// Port of `streamSimpleBedrock`.
pub fn stream_simple_bedrock(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    let base = build_base_options(model, options, None);
    let reasoning = options.and_then(|options| options.reasoning);
    let thinking_budgets = options.and_then(|options| options.thinking_budgets.clone());

    if reasoning.is_none() || reasoning == Some(ModelThinkingLevel::Off) {
        return stream_bedrock(
            model,
            context,
            Some(&BedrockOptions {
                base,
                reasoning: None,
                ..Default::default()
            }),
        );
    }

    if is_anthropic_claude_model(model) {
        if supports_adaptive_thinking(&model.id, Some(&model.name)) {
            return stream_bedrock(
                model,
                context,
                Some(&BedrockOptions {
                    base,
                    reasoning,
                    thinking_budgets,
                    ..Default::default()
                }),
            );
        }

        let adjusted = match crate::providers::simple_options::adjust_max_tokens_for_thinking(
            base.max_tokens.unwrap_or(0),
            model.max_tokens,
            reasoning.expect("checked above"),
            thinking_budgets.as_ref(),
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
                    reason: error_reason(StopReason::Error),
                    error: message.clone(),
                });
                writer.end(Some(message));
                return reader;
            }
        };

        let clamped_level = clamp_reasoning(reasoning.expect("checked above"));
        let mut budgets = thinking_budgets.clone().unwrap_or_default();
        match clamped_level {
            ModelThinkingLevel::Minimal => budgets.minimal = Some(adjusted.1),
            ModelThinkingLevel::Low => budgets.low = Some(adjusted.1),
            ModelThinkingLevel::Medium => budgets.medium = Some(adjusted.1),
            _ => budgets.high = Some(adjusted.1),
        }

        return stream_bedrock(
            model,
            context,
            Some(&BedrockOptions {
                base: StreamOptions {
                    max_tokens: Some(adjusted.0),
                    ..base
                },
                reasoning,
                thinking_budgets: Some(budgets),
                ..Default::default()
            }),
        );
    }

    stream_bedrock(
        model,
        context,
        Some(&BedrockOptions {
            base,
            reasoning,
            thinking_budgets,
            ..Default::default()
        }),
    )
}

/// Registry provider for the `bedrock-converse-stream` API.
pub struct BedrockConverseStreamProvider;

impl Provider for BedrockConverseStreamProvider {
    fn api(&self) -> &str {
        API_BEDROCK_CONVERSE_STREAM
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let options = options.map(|base| BedrockOptions::from_base(base.clone()));
        stream_bedrock(model, context, options.as_ref())
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        stream_simple_bedrock(model, context, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::bedrock::auth::{
        get_standard_bedrock_endpoint_region, should_use_explicit_bedrock_endpoint,
    };

    #[test]
    fn parses_standard_endpoint_regions() {
        assert_eq!(
            get_standard_bedrock_endpoint_region("https://bedrock-runtime.us-west-2.amazonaws.com"),
            Some("us-west-2".to_string())
        );
        assert_eq!(
            get_standard_bedrock_endpoint_region(
                "https://bedrock-runtime-fips.us-gov-west-1.amazonaws.com"
            ),
            Some("us-gov-west-1".to_string())
        );
        assert_eq!(
            get_standard_bedrock_endpoint_region("https://example.com"),
            None
        );
    }

    #[test]
    fn explicit_endpoint_rules() {
        assert!(should_use_explicit_bedrock_endpoint(
            "http://localhost:8000",
            None,
            false
        ));
        assert!(!should_use_explicit_bedrock_endpoint(
            "https://bedrock-runtime.us-west-2.amazonaws.com",
            Some("us-west-2"),
            false
        ));
    }

    #[test]
    fn normalizes_tool_call_ids() {
        use crate::providers::bedrock::convert::normalize_tool_call_id;
        assert_eq!(normalize_tool_call_id("toolu_01ABCdef"), "toolu_01ABCdef");
        assert_eq!(normalize_tool_call_id(&"a".repeat(80)).len(), 64);
    }

    #[test]
    fn maps_stop_reasons() {
        use crate::types::StopReason;
        assert_eq!(map_stop_reason(Some("end_turn")), StopReason::Stop);
        assert_eq!(map_stop_reason(Some("stop_sequence")), StopReason::Stop);
        assert_eq!(map_stop_reason(Some("max_tokens")), StopReason::Length);
        assert_eq!(map_stop_reason(Some("tool_use")), StopReason::ToolUse);
        assert_eq!(map_stop_reason(Some("other")), StopReason::Error);
        assert_eq!(map_stop_reason(None), StopReason::Error);
    }

    #[test]
    fn error_prefixes_match_ts() {
        assert_eq!(
            bedrock_error_prefix("ThrottlingException"),
            "Throttling error"
        );
        assert_eq!(bedrock_error_prefix("Unknown"), "Unknown");
    }

    #[test]
    fn gov_cloud_detection() {
        let model = test_model("arn:aws-us-gov:bedrock:us-gov-west-1::foundation-model/test");
        let options = BedrockOptions::default();
        assert!(is_gov_cloud_bedrock_target(&model, &options));

        let options = BedrockOptions {
            region: Some("us-gov-east-1".into()),
            ..Default::default()
        };
        assert!(is_gov_cloud_bedrock_target(
            &test_model("us.anthropic.claude"),
            &options
        ));
    }

    fn test_model(id: &str) -> Model {
        Model {
            id: id.into(),
            name: "claude".into(),
            api: API_BEDROCK_CONVERSE_STREAM.into(),
            provider: "amazon-bedrock".into(),
            base_url: String::new(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![crate::types::ModelInput::Text],
            cost: crate::types::zero_model_cost(),
            context_window: 200_000,
            max_tokens: 8192,
            featured: None,
            headers: None,
            compat: None,
        }
    }
}
