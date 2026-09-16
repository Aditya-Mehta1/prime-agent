//! Shared classification and reporting for provider stream failures, so no
//! provider collapses a specific cause (refusal, safety filter, overload, ...)
//! into a generic string before it is logged and persisted.
//! Ported from `packages/ai/src/utils/stream-failure.ts`.

use serde::Serialize;

use crate::types::AssistantMessage;
use crate::utils::diagnostics::{
    append_assistant_message_diagnostic, create_assistant_message_diagnostic, now_ms,
    DiagnosticErrorInfo,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamFailureKind {
    Refusal,
    Safety,
    Overloaded,
    RateLimit,
    ServerError,
    Auth,
    Permission,
    InvalidRequest,
    MalformedResponse,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamFailureInfo {
    pub kind: StreamFailureKind,
    /// Provider's own error/stop identifier, e.g. "overloaded_error" or "SAFETY".
    #[serde(
        rename = "providerErrorType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub provider_error_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(rename = "requestId", default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Server-requested wait before retrying (Retry-After header or reset info), in milliseconds.
    #[serde(
        rename = "retryAfterMs",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub retry_after_ms: Option<u64>,
    /// Truncated raw provider payload for post-mortems.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

impl StreamFailureInfo {
    pub fn unknown() -> Self {
        Self {
            kind: StreamFailureKind::Unknown,
            provider_error_type: None,
            status: None,
            request_id: None,
            retry_after_ms: None,
            raw: None,
        }
    }
}

/// A stream failure carrying structured classification info.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamFailureError {
    pub message: String,
    pub info: StreamFailureInfo,
}

impl std::fmt::Display for StreamFailureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StreamFailureError {}

/// Errors raised by provider HTTP/SSE plumbing, carrying the raw pieces the TS
/// reference extracts from provider SDK exceptions.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderHttpError {
    pub message: String,
    pub status: Option<u16>,
    pub body: Option<String>,
    pub headers: std::collections::HashMap<String, String>,
    pub request_id: Option<String>,
}

impl std::fmt::Display for ProviderHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProviderHttpError {}

/// Unified provider error used across the crate.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderError {
    StreamFailure(StreamFailureError),
    Http(ProviderHttpError),
    /// Plain error message; classified from its text like unrecognized TS errors.
    Message(String),
    Aborted,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::StreamFailure(e) => write!(f, "{}", e.message),
            ProviderError::Http(e) => write!(f, "{}", e.message),
            ProviderError::Message(m) => f.write_str(m),
            ProviderError::Aborted => f.write_str("Request was aborted"),
        }
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    /// Build an HTTP error from a status code and response body, classifying
    /// the failure from the body text like the TS `stream-failure.ts` parser.
    pub fn from_http_status_body(
        status: u16,
        body: &str,
        headers: std::collections::HashMap<String, String>,
    ) -> Self {
        // The OpenAI SDK surfaces `400 <body>` as the error message; the
        // classification pipeline reads the structured `body` field, and the
        // user-facing message is rebuilt from the classified parts.
        let message = if body.trim().is_empty() {
            format!("{status}")
        } else {
            format!("{status} {body}")
        };
        let http_error = ProviderHttpError {
            message,
            status: Some(status),
            body: Some(body.to_string()),
            headers,
            request_id: None,
        };
        let parts = extract_parts_from_http(&http_error);
        if parts.info.kind == StreamFailureKind::Unknown {
            ProviderError::Http(http_error)
        } else {
            ProviderError::Http(ProviderHttpError {
                message: stream_failure_message(&parts.info, parts.detail.as_deref()),
                ..http_error
            })
        }
    }
}

impl From<StreamFailureError> for ProviderError {
    fn from(value: StreamFailureError) -> Self {
        ProviderError::StreamFailure(value)
    }
}

impl From<ProviderHttpError> for ProviderError {
    fn from(value: ProviderHttpError) -> Self {
        ProviderError::Http(value)
    }
}

const KIND_MESSAGES: &[(StreamFailureKind, &str)] = &[
    (StreamFailureKind::Refusal, "Model refused to respond"),
    (
        StreamFailureKind::Safety,
        "Response blocked by provider safety filters",
    ),
    (StreamFailureKind::Overloaded, "Provider overloaded"),
    (StreamFailureKind::RateLimit, "Provider rate limit exceeded"),
    (StreamFailureKind::ServerError, "Provider server error"),
    (StreamFailureKind::Auth, "Provider authentication failed"),
    (
        StreamFailureKind::Permission,
        "Provider denied access to the requested resource",
    ),
    (
        StreamFailureKind::InvalidRequest,
        "Provider rejected the request",
    ),
    (
        StreamFailureKind::MalformedResponse,
        "Provider returned a malformed response",
    ),
    (StreamFailureKind::Unknown, "Provider stream failed"),
];

fn kind_message(kind: StreamFailureKind) -> &'static str {
    KIND_MESSAGES
        .iter()
        .find(|(candidate, _)| *candidate == kind)
        .map(|(_, message)| *message)
        .unwrap_or("Provider stream failed")
}

/// Build a user-facing message like "Provider overloaded (overloaded_error, 529) [request_id: req_abc]".
pub fn stream_failure_message(info: &StreamFailureInfo, detail: Option<&str>) -> String {
    let mut qualifiers: Vec<String> = Vec::new();
    if let Some(provider_error_type) = &info.provider_error_type {
        qualifiers.push(provider_error_type.clone());
    }
    if let Some(status) = info.status {
        qualifiers.push(status.to_string());
    }
    let mut message = kind_message(info.kind).to_string();
    if !qualifiers.is_empty() {
        message += &format!(" ({})", qualifiers.join(", "));
    }
    if let Some(detail) = detail {
        message += &format!(": {detail}");
    }
    if let Some(request_id) = &info.request_id {
        message += &format!(" [request_id: {request_id}]");
    }
    message
}

pub fn classify_stream_failure(
    provider_error_type: Option<&str>,
    status: Option<u16>,
) -> StreamFailureKind {
    let type_lower = provider_error_type.unwrap_or("").to_lowercase();
    if type_lower == "refusal" {
        return StreamFailureKind::Refusal;
    }
    let safety = regex::Regex::new(
        r"sensitive|safety|prohibited_content|blocklist|spii|recitation|content.?filter|guardrail|flagged",
    )
    .expect("static regex");
    if safety.is_match(&type_lower) {
        return StreamFailureKind::Safety;
    }
    if type_lower.contains("overloaded") || status == Some(529) {
        return StreamFailureKind::Overloaded;
    }
    // usage_not_included is Codex's plan-entitlement rejection, not bad credentials.
    let rate_limit = regex::Regex::new(r"rate_limit|usage_limit|usage_not_included|throttl")
        .expect("static regex");
    if rate_limit.is_match(&type_lower) || status == Some(429) {
        return StreamFailureKind::RateLimit;
    }
    // Permission/403 shapes are entitlement or policy denials, not bad credentials: never auth-stale.
    if type_lower.contains("authentication")
        || type_lower.contains("unauthorized")
        || status == Some(401)
    {
        return StreamFailureKind::Auth;
    }
    let permission =
        regex::Regex::new(r"permission|forbidden|access.?denied").expect("static regex");
    if permission.is_match(&type_lower) || status == Some(403) {
        return StreamFailureKind::Permission;
    }
    if type_lower.contains("invalid_request")
        || type_lower.contains("not_found_error")
        || status == Some(400)
        || status == Some(404)
    {
        return StreamFailureKind::InvalidRequest;
    }
    if type_lower.contains("malformed") {
        return StreamFailureKind::MalformedResponse;
    }
    if type_lower.contains("api_error")
        || type_lower.contains("server_error")
        || type_lower.contains("unavailable")
        || status.is_some_and(|status| status >= 500)
    {
        return StreamFailureKind::ServerError;
    }
    StreamFailureKind::Unknown
}

/// Failure for a stream that terminated with a provider stop/finish reason that
/// maps to "error" (e.g. Anthropic "refusal", Gemini "SAFETY").
pub fn stream_failure_from_stop_reason(
    raw_stop_reason: Option<&str>,
    request_id: Option<&str>,
) -> StreamFailureError {
    let mut info = StreamFailureInfo {
        kind: match raw_stop_reason {
            Some(reason) => classify_stream_failure(Some(reason), None),
            None => StreamFailureKind::Unknown,
        },
        provider_error_type: raw_stop_reason.map(|s| s.to_string()),
        request_id: request_id.map(|s| s.to_string()),
        status: None,
        retry_after_ms: None,
        raw: None,
    };
    if info.kind == StreamFailureKind::Unknown
        && raw_stop_reason
            .map(|reason| reason.to_lowercase().contains("malformed"))
            .unwrap_or(false)
    {
        info.kind = StreamFailureKind::MalformedResponse;
    }
    let message = match raw_stop_reason {
        Some(_) => stream_failure_message(&info, None),
        None => {
            stream_failure_message(&info, Some("stream ended with an error and no stop reason"))
        }
    };
    StreamFailureError { message, info }
}

const MAX_RAW_LENGTH: usize = 2000;

pub fn truncate_raw_payload(raw: &str) -> String {
    if raw.len() > MAX_RAW_LENGTH {
        // Match the TS slice-by-16-bit-code-unit behavior closely enough for
        // post-mortem truncation while staying on char boundaries in Rust.
        let mut end = MAX_RAW_LENGTH;
        while end > 0 && !raw.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\u{2026}", &raw[..end])
    } else {
        raw.to_string()
    }
}

fn header_value(headers: &std::collections::HashMap<String, String>, name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

/// Parse Retry-After / Retry-After-Ms headers into a millisecond wait.
pub fn parse_retry_after_ms(headers: &std::collections::HashMap<String, String>) -> Option<u64> {
    if let Some(value) = header_value(headers, "retry-after-ms") {
        if let Ok(ms) = value.parse::<f64>() {
            if ms.is_finite() && ms >= 0.0 {
                return Some(ms as u64);
            }
        }
    }
    let raw = header_value(headers, "retry-after")?;
    if let Ok(seconds) = raw.parse::<f64>() {
        if seconds.is_finite() && seconds >= 0.0 {
            return Some((seconds * 1000.0) as u64);
        }
    }
    // HTTP-date form: compute the delta against now.
    match parse_http_date(&raw) {
        Some(date_ms) => {
            let now = now_ms() as i64;
            Some((date_ms - now).max(0) as u64)
        }
        None => None,
    }
}

fn parse_http_date(raw: &str) -> Option<i64> {
    // Minimal IMF-fixdate parser: "Sun, 06 Nov 1994 08:49:37 GMT".
    let parts: Vec<&str> = raw.split_whitespace().collect();
    if parts.len() < 6 {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = match parts[2].to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let year: i64 = parts[3].parse().ok()?;
    let time: Vec<&str> = parts[4].split(':').collect();
    if time.len() < 3 {
        return None;
    }
    let (h, m, s): (i64, i64, i64) = (
        time[0].parse().ok()?,
        time[1].parse().ok()?,
        time[2].parse().ok()?,
    );
    let days = days_from_civil(year, month, day);
    Some((days * 86_400 + h * 3600 + m * 60 + s) * 1000)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

struct ExtractedParts {
    info: StreamFailureInfo,
    detail: Option<String>,
}

fn extract_parts_from_http(error: &ProviderHttpError) -> ExtractedParts {
    let mut body_type: Option<String> = None;
    let mut body_message: Option<String> = None;
    if let Some(body) = &error.body {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body) {
            // Error bodies come nested differently per SDK: Anthropic/OpenAI expose
            // `error.error = {type|code, message}` (sometimes doubly nested).
            let mut node = &parsed;
            if let Some(nested) = node.get("error") {
                if nested.is_object() && nested.get("error").is_some() {
                    node = nested.get("error").unwrap();
                }
                if nested.is_object() {
                    node = nested;
                }
            }
            if let Some(value) = node.get("type").or_else(|| node.get("code")) {
                if let Some(text) = value.as_str() {
                    body_type = Some(text.to_string());
                }
            }
            if let Some(value) = node.get("message") {
                if let Some(text) = value.as_str() {
                    body_message = Some(text.to_string());
                }
            }
        }
    }

    let header_request_id = header_value(&error.headers, "request-id")
        .or_else(|| header_value(&error.headers, "x-request-id"));
    let request_id = error.request_id.clone().or(header_request_id);
    let retry_after_ms = parse_retry_after_ms(&error.headers);

    let provider_error_type = body_type.clone();
    let mut kind = classify_stream_failure(
        provider_error_type
            .as_deref()
            .or(Some(error.message.as_str())),
        error.status,
    );
    // Message text is too weak for these verdicts: without a structured type, only the status decides.
    if (kind == StreamFailureKind::Auth || kind == StreamFailureKind::Permission)
        && provider_error_type.is_none()
    {
        kind = classify_stream_failure(None, error.status);
    }

    ExtractedParts {
        info: StreamFailureInfo {
            kind,
            provider_error_type,
            status: error.status,
            request_id,
            retry_after_ms,
            raw: None,
        },
        detail: body_message,
    }
}

/// Best-effort extraction of structured failure info from any provider error.
pub fn extract_stream_failure_info(error: &ProviderError) -> StreamFailureInfo {
    match error {
        ProviderError::StreamFailure(failure) => failure.info.clone(),
        ProviderError::Http(http) => extract_parts_from_http(http).info,
        ProviderError::Message(message) => StreamFailureInfo {
            kind: classify_stream_failure(Some(message), None),
            ..StreamFailureInfo::unknown()
        },
        ProviderError::Aborted => StreamFailureInfo::unknown(),
    }
}

/// User-facing message for a thrown stream error: a classified one-liner with
/// the provider's own short message, never the raw payload/trace. Unrecognized
/// errors pass through verbatim so their text (which downstream retry matching
/// may depend on) is preserved.
pub fn format_stream_failure_message(error: &ProviderError) -> String {
    match error {
        ProviderError::StreamFailure(failure) => failure.message.clone(),
        ProviderError::Aborted => "Request was aborted".to_string(),
        ProviderError::Http(http) => {
            let parts = extract_parts_from_http(http);
            if parts.info.kind == StreamFailureKind::Unknown {
                http.message.clone()
            } else {
                stream_failure_message(&parts.info, parts.detail.as_deref())
            }
        }
        ProviderError::Message(message) => message.clone(),
    }
}

fn diagnostic_error_info(error: &ProviderError) -> DiagnosticErrorInfo {
    let (name, message, code) = match error {
        ProviderError::StreamFailure(_) => (
            Some("StreamFailureError".to_string()),
            error.to_string(),
            None,
        ),
        ProviderError::Http(_) => (
            Some("ProviderHttpError".to_string()),
            error.to_string(),
            None,
        ),
        ProviderError::Message(_) => (None, error.to_string(), None),
        ProviderError::Aborted => (
            Some("AbortError".to_string()),
            "Request was aborted".to_string(),
            None,
        ),
    };
    DiagnosticErrorInfo {
        name,
        message,
        stack: None,
        code,
        rest: Default::default(),
    }
}

/// Record a terminal stream failure on the message (structured diagnostic that
/// persists to session JSONL) and emit one structured log line. Call from the
/// provider's terminal catch after stop_reason/error_message are set; no-op for
/// user-initiated aborts.
pub fn record_stream_failure(
    model: (&str, &str, &str),
    output: &mut AssistantMessage,
    error: &ProviderError,
) {
    if output.stop_reason != crate::types::StopReason::Error {
        return;
    }
    let info = extract_stream_failure_info(error);
    let info_json = serde_json::to_value(&info).unwrap_or(serde_json::Value::Null);
    append_assistant_message_diagnostic(
        output,
        create_assistant_message_diagnostic(
            "provider_stream_failure",
            Some(diagnostic_error_info(error)),
            Some(info_json),
        ),
    );
    let raw_message = error.to_string();
    let error_message = output.error_message.clone().unwrap_or_default();
    crate::utils_inner::log::get_logger("ai.provider").error(
        "provider stream failure",
        serde_json::json!({
            "provider": model.0,
            "model": model.1,
            "api": model.2,
            "kind": info.kind,
            "providerErrorType": info.provider_error_type,
            "status": info.status,
            "requestId": info.request_id,
            "message": output.error_message,
            "cause": if raw_message == error_message {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(truncate_raw_payload(&raw_message))
            },
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_provider_error_types() {
        assert_eq!(
            classify_stream_failure(Some("refusal"), None),
            StreamFailureKind::Refusal
        );
        assert_eq!(
            classify_stream_failure(Some("SAFETY"), None),
            StreamFailureKind::Safety
        );
        assert_eq!(
            classify_stream_failure(Some("overloaded_error"), None),
            StreamFailureKind::Overloaded
        );
        assert_eq!(
            classify_stream_failure(Some("other"), Some(529)),
            StreamFailureKind::Overloaded
        );
        assert_eq!(
            classify_stream_failure(Some("rate_limit_error"), None),
            StreamFailureKind::RateLimit
        );
        assert_eq!(
            classify_stream_failure(Some("usage_not_included"), None),
            StreamFailureKind::RateLimit
        );
        assert_eq!(
            classify_stream_failure(Some("other"), Some(429)),
            StreamFailureKind::RateLimit
        );
        assert_eq!(
            classify_stream_failure(Some("authentication_error"), None),
            StreamFailureKind::Auth
        );
        assert_eq!(
            classify_stream_failure(Some("forbidden"), None),
            StreamFailureKind::Permission
        );
        assert_eq!(
            classify_stream_failure(Some("invalid_request_error"), None),
            StreamFailureKind::InvalidRequest
        );
        assert_eq!(
            classify_stream_failure(Some("api_error"), None),
            StreamFailureKind::ServerError
        );
        assert_eq!(
            classify_stream_failure(Some("other"), Some(503)),
            StreamFailureKind::ServerError
        );
        assert_eq!(
            classify_stream_failure(Some("weird"), None),
            StreamFailureKind::Unknown
        );
    }

    #[test]
    fn builds_user_facing_messages() {
        let info = StreamFailureInfo {
            kind: StreamFailureKind::Overloaded,
            provider_error_type: Some("overloaded_error".into()),
            status: Some(529),
            request_id: Some("req_abc".into()),
            retry_after_ms: None,
            raw: None,
        };
        assert_eq!(
            stream_failure_message(&info, Some("slow down")),
            "Provider overloaded (overloaded_error, 529): slow down [request_id: req_abc]"
        );
    }

    #[test]
    fn stop_reason_failures() {
        let err = stream_failure_from_stop_reason(Some("refusal"), None);
        assert_eq!(err.info.kind, StreamFailureKind::Refusal);
        assert_eq!(err.message, "Model refused to respond (refusal)");

        let missing = stream_failure_from_stop_reason(None, None);
        assert_eq!(
            missing.message,
            "Provider stream failed: stream ended with an error and no stop reason"
        );
    }

    #[test]
    fn truncates_raw_payload() {
        let long = "x".repeat(2500);
        let truncated = truncate_raw_payload(&long);
        assert!(truncated.chars().count() == 2001);
        assert!(truncated.ends_with('\u{2026}'));
    }

    #[test]
    fn parses_retry_after_headers() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("retry-after-ms".to_string(), "250".to_string());
        assert_eq!(parse_retry_after_ms(&headers), Some(250));
        headers.clear();
        headers.insert("retry-after".to_string(), "3".to_string());
        assert_eq!(parse_retry_after_ms(&headers), Some(3000));
    }
}
