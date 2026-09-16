//! AI surface types for providers.
//!
//! The wire/domain types (messages, content blocks, usage, stream events,
//! models) live in [`pa_types::ai`] and are re-exported here for provider
//! code. This module adds the provider-internal option structs
//! ([`StreamOptions`], [`SimpleStreamOptions`]) and hooks that never cross
//! the wire, plus small extension helpers over the shared types.

use tokio_util::sync::CancellationToken;

pub use pa_types::ai::{
    AnthropicMessagesCompat, Api, AssistantContentBlock as AssistantContent, AssistantMessage,
    AssistantMessageDiagnostic, AssistantMessageEvent, CacheControlFormat, CacheRetention,
    CompatKind, Context, DataCollection, DiagnosticCode, DiagnosticErrorInfo, DoneStopReason,
    ErrorStopReason, ImageContent, KnownApi, KnownProvider, MaxTokensField, Message, Model,
    ModelCompat, ModelCost, ModelInput, ModelThinkingLevel, NumOrString, OpenAiCompletionsCompat,
    OpenAiResponsesCompat, OpenRouterMaxPrice, OpenRouterRouting, OpenRouterSort,
    OpenRouterThreshold, Provider, ProviderResponse, ServiceTier, StopReason, TextContent,
    TextSignaturePhase, TextSignatureV1, ThinkingBudgets, ThinkingContent, ThinkingFormat,
    ThinkingLevel, ThinkingLevelMap, Tool, ToolCall, ToolResultMessage, Transport, Usage,
    UsageCost, UserContent as UserMessageContent, UserContentBlock as UserOrToolContent,
    UserMessage,
};

/// JSON tool parameter schema. In the TS reference this is a TypeBox schema;
/// here it is stored as the raw JSON value.
pub type JsonSchema = serde_json::Value;

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// Hook invoked with the outbound provider payload before sending. Return None
/// to keep the payload unchanged.
pub type OnPayloadHook =
    std::sync::Arc<dyn Fn(serde_json::Value, &Model) -> Option<serde_json::Value> + Send + Sync>;

/// Hook invoked after the HTTP response is received and before the body is read.
pub type OnResponseHook = std::sync::Arc<dyn Fn(ProviderResponse, &Model) + Send + Sync>;

/// Options shared by all providers (`StreamOptions` in the TS reference).
#[derive(Clone, Default)]
pub struct StreamOptions {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub signal: Option<CancellationToken>,
    pub api_key: Option<String>,
    pub transport: Option<Transport>,
    pub service_tier: Option<ServiceTier>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub on_payload: Option<OnPayloadHook>,
    pub on_response: Option<OnResponseHook>,
    pub headers: Option<std::collections::HashMap<String, String>>,
    pub timeout_ms: Option<u64>,
    pub metadata: Option<std::collections::HashMap<String, serde_json::Value>>,
}

impl std::fmt::Debug for StreamOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamOptions")
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("signal", &self.signal.is_some())
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .field("transport", &self.transport)
            .field("service_tier", &self.service_tier)
            .field("cache_retention", &self.cache_retention)
            .field("session_id", &self.session_id)
            .field("on_payload", &self.on_payload.is_some())
            .field("on_response", &self.on_response.is_some())
            .field("headers", &self.headers)
            .field("timeout_ms", &self.timeout_ms)
            .field("metadata", &self.metadata)
            .finish()
    }
}

/// Unified options with reasoning passed to `stream_simple()`/`complete_simple()`.
#[derive(Clone, Default, Debug)]
pub struct SimpleStreamOptions {
    pub base: StreamOptions,
    /// Explicit model reasoning selection. Omit to preserve the provider default.
    pub reasoning: Option<ModelThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
}

impl SimpleStreamOptions {
    pub fn from_base(base: StreamOptions) -> Self {
        Self {
            base,
            reasoning: None,
            thinking_budgets: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Extension helpers over the shared wire types
// ---------------------------------------------------------------------------

/// Provider-side helpers over [`Model`] (the TS `Model` methods live with the
/// type definition; here they are an extension trait because `Model` itself
/// is owned by `pa-types`).
pub trait ModelExt {
    /// Parsed compat overrides for this model, when present.
    fn compat_kind(&self) -> Option<CompatKind>;
    /// Value mapped for a thinking level (provider-specific string); the outer
    /// Option is None when the map is absent, the inner when the level is
    /// unsupported/absent.
    fn thinking_level_map_value(&self, level: ModelThinkingLevel) -> Option<Option<&String>>;
    /// Whether the model accepts image input blocks.
    fn supports_image_input(&self) -> bool;
}

impl ModelExt for Model {
    fn compat_kind(&self) -> Option<CompatKind> {
        self.compat.as_ref().and_then(|compat| compat.kind().ok())
    }

    fn thinking_level_map_value(&self, level: ModelThinkingLevel) -> Option<Option<&String>> {
        self.thinking_level_map
            .as_ref()
            .map(|map| map.get(&level).and_then(|value| value.as_ref()))
    }

    fn supports_image_input(&self) -> bool {
        self.input
            .iter()
            .any(|mode| matches!(mode, ModelInput::Image))
    }
}

// ---------------------------------------------------------------------------
// Stop-reason mapping helpers
// ---------------------------------------------------------------------------

/// Map a [`StopReason`] to the terminal reason of a `done` event. Panics for
/// `error`/`aborted`, which only terminate streams through `error` events.
pub fn done_reason(reason: StopReason) -> DoneStopReason {
    match reason {
        StopReason::Stop => DoneStopReason::Stop,
        StopReason::Length => DoneStopReason::Length,
        StopReason::ToolUse => DoneStopReason::ToolUse,
        reason => panic!("stop reason {reason:?} cannot terminate a done event"),
    }
}

/// Map a [`StopReason`] to the terminal reason of an `error` event.
pub fn error_reason(reason: StopReason) -> ErrorStopReason {
    match reason {
        StopReason::Aborted => ErrorStopReason::Aborted,
        _ => ErrorStopReason::Error,
    }
}

/// Role tag of a message on the wire (`user`, `assistant`, `toolResult`).
pub trait MessageExt {
    fn role(&self) -> &'static str;
}

impl MessageExt for Message {
    fn role(&self) -> &'static str {
        match self {
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::ToolResult(_) => "toolResult",
        }
    }
}

/// All-zero model cost ($0 per million tokens).
pub fn zero_model_cost() -> ModelCost {
    ModelCost {
        input: 0.0.into(),
        output: 0.0.into(),
        cache_read: 0.0.into(),
        cache_write: 0.0.into(),
    }
}

/// Zeroed usage (the TS `Usage` type is initialized with all-zero fields).
pub fn zeroed_usage() -> Usage {
    Usage::default()
}

/// Total billed tokens including cache traffic (`totalTokens`).
pub fn usage_total_tokens(usage: &Usage) -> u64 {
    usage.input + usage.output + usage.cache_read + usage.cache_write
}
