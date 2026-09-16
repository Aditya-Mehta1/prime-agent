//! Agent messaging and observation host requests: validation helpers, message
//! ids and prompts, controller traits, and kernel host-handler registration.
//! Port of core/agent-messages.ts (validation/prompt half) and
//! core/agent-observe.ts.

use std::future::Future;

use serde_json::{json, Value};

use crate::kernel::shared::{host_handler, HostRequestHandlers};

pub const AGENT_MESSAGE_CUSTOM_TYPE: &str = "agent_message";
pub const AGENT_MESSAGE_SOURCE: &str = "agent_message";
pub const AGENT_MESSAGE_ID_PREFIX: &str = "agentmsg_";
pub const DEFAULT_AGENT_MESSAGE_MAX_CHARS: usize = 16_384;
pub const DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION: usize = 20;

pub const AGENT_OBSERVE_PREVIEW_MAX_CHARS: usize = 240;
pub const AGENT_OBSERVE_IMPORT_NAME: &str = "agent_observe";

/// Family relationships between agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFamilyRelationship {
    Parent,
    Sibling,
    Child,
}

impl AgentFamilyRelationship {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentFamilyRelationship::Parent => "parent",
            AgentFamilyRelationship::Sibling => "sibling",
            AgentFamilyRelationship::Child => "child",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "parent" => Some(AgentFamilyRelationship::Parent),
            "sibling" => Some(AgentFamilyRelationship::Sibling),
            "child" => Some(AgentFamilyRelationship::Child),
            _ => None,
        }
    }
}

/// Delivery status for a sent agent message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMessageDeliveryStatus {
    Delivered,
    Queued,
}

impl AgentMessageDeliveryStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentMessageDeliveryStatus::Delivered => "delivered",
            AgentMessageDeliveryStatus::Queued => "queued",
        }
    }
}

/// `agent_message.send` input.
#[derive(Debug, Clone)]
pub struct AgentMessageSendInput {
    pub target: String,
    pub message: String,
    pub receiver_role: Option<AgentFamilyRelationship>,
}

/// The receipt returned after sending an agent message.
#[derive(Debug, Clone)]
pub struct AgentMessageReceipt {
    pub id: String,
    pub target: String,
    pub message: String,
    pub delivery_status: AgentMessageDeliveryStatus,
    pub delivery_mode: Option<&'static str>,
    pub receiver_role: Option<AgentFamilyRelationship>,
    pub delivered_at: Option<String>,
    pub queued_at: Option<String>,
}

/// The controller the daemon supplies for `agent_message.*` requests.
pub trait AgentMessageController: Send + Sync {
    fn send_agent_message(
        &self,
        input: AgentMessageSendInput,
    ) -> impl Future<Output = anyhow::Result<AgentMessageReceipt>> + Send;
}

/// Message payload for the rendered `[agent-message from ...]` prompt.
#[derive(Debug, Clone, Default)]
pub struct AgentMessagePromptPayload {
    pub message: String,
    pub sender_name: String,
    pub from_relationship: Option<AgentFamilyRelationship>,
}

pub fn create_agent_session_message_id() -> String {
    format!("{AGENT_MESSAGE_ID_PREFIX}{}", uuid::Uuid::new_v4())
}

/// Distinguishes agent-to-agent ids from synthetic prompt ids.
pub fn is_agent_session_message_id(id: Option<&str>) -> bool {
    id.is_some_and(|id| id.starts_with(AGENT_MESSAGE_ID_PREFIX))
}

/// Normalize and validate an outgoing message body.
pub fn normalize_agent_session_message(message: &str) -> anyhow::Result<String> {
    normalize_agent_session_message_limited(message, DEFAULT_AGENT_MESSAGE_MAX_CHARS)
}

pub fn normalize_agent_session_message_limited(
    message: &str,
    max_chars: usize,
) -> anyhow::Result<String> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Agent session message cannot be empty");
    }
    if trimmed.chars().count() > max_chars {
        anyhow::bail!(
            "Agent session message is too long: {} chars exceeds {max_chars}",
            trimmed.chars().count()
        );
    }
    Ok(trimmed.to_string())
}

/// Reject broadcast targets: only direct messaging is supported.
pub fn assert_direct_agent_message_target(target: &str) -> anyhow::Result<String> {
    let normalized = target.trim();
    if normalized.is_empty() {
        anyhow::bail!("Agent message target cannot be empty");
    }
    if normalized == "*"
        || normalized.eq_ignore_ascii_case("all")
        || normalized.eq_ignore_ascii_case("broadcast")
    {
        anyhow::bail!("Broadcast agent messaging is not supported");
    }
    Ok(normalized.to_string())
}

/// Guard the target session's pending-work capacity.
pub fn assert_agent_message_queue_capacity(
    unfinished_action_count: usize,
    max_pending: usize,
) -> anyhow::Result<()> {
    if unfinished_action_count >= max_pending {
        anyhow::bail!(
            "Target session has too many pending messages: {unfinished_action_count} unfinished, limit is {max_pending}"
        );
    }
    Ok(())
}

fn sanitize_message_header_value(value: &str) -> String {
    value
        .chars()
        .map(|char| {
            if char.is_alphanumeric() || char == '-' || char == '_' {
                char
            } else {
                '_'
            }
        })
        .collect()
}

/// The rendered prompt a receiving context sees.
pub fn create_agent_session_message_prompt(payload: &AgentMessagePromptPayload) -> String {
    let sender = sanitize_message_header_value(&payload.sender_name);
    let sender = if sender.is_empty() {
        "unknown".to_string()
    } else {
        sender
    };
    let sender = match payload.from_relationship {
        Some(relationship) => format!("{}:{sender}", relationship.as_str()),
        None => sender,
    };
    format!("[agent-message from {sender}]\n\n{}", payload.message)
}

/// Parse the message id out of the pre-bracket-grammar transcript header.
pub fn parse_agent_session_message_prompt_id(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let offset = lines.first().is_some_and(|line| line.starts_with("[from ")) as usize;
    if lines.get(offset).copied() != Some("Agent-to-agent message received.")
        || lines.get(offset + 1).copied()
            != Some(format!("Source: {AGENT_MESSAGE_SOURCE}").as_str())
    {
        return None;
    }
    let to_line_index = if lines
        .get(offset + 2)
        .is_some_and(|line| line.starts_with("From: "))
    {
        offset + 3
    } else {
        offset + 2
    };
    if !lines
        .get(to_line_index)
        .is_some_and(|line| line.starts_with("To: "))
    {
        return None;
    }
    let id_line = lines.get(to_line_index + 1)?;
    let id = id_line.strip_prefix("Message id: ")?;
    (!id.is_empty() && id.starts_with(AGENT_MESSAGE_ID_PREFIX)).then(|| id.to_string())
}

pub fn is_agent_session_message_prompt(text: &str) -> bool {
    parse_agent_session_message_prompt_id(text).is_some()
}

fn receipt_value(receipt: &AgentMessageReceipt) -> Value {
    json!({
        "id": receipt.id,
        "source": AGENT_MESSAGE_SOURCE,
        "target": receipt.target,
        "message": receipt.message,
        "deliveryStatus": receipt.delivery_status.as_str(),
        "deliveredAt": receipt.delivered_at,
        "queuedAt": receipt.queued_at,
        "deliveryMode": receipt.delivery_mode,
        "receiverRole": receipt.receiver_role.map(|role| role.as_str()),
    })
}

/// Register `agent_message.*` handlers onto a handler map.
pub fn register_agent_message_host_handlers<C: AgentMessageController + 'static>(
    controller: std::sync::Arc<C>,
    handlers: &mut HostRequestHandlers,
) {
    handlers.register(
        "agent_message.send",
        host_handler(move |payload| {
            let controller = controller.clone();
            Box::pin(async move {
                let data = payload.data;
                let Some(target) = data.get("target").and_then(Value::as_str) else {
                    return Err(anyhow::anyhow!(
                        "agent_message.send target must be a string"
                    ));
                };
                let Some(message) = data.get("message").and_then(Value::as_str) else {
                    return Err(anyhow::anyhow!(
                        "agent_message.send message must be a string"
                    ));
                };
                let target = assert_direct_agent_message_target(target)?;
                let message = normalize_agent_session_message(message)?;
                let receiver_role = match data.get("receiver_role") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(role)) => AgentFamilyRelationship::parse(role),
                    _ => None,
                };
                let receipt = controller
                    .send_agent_message(AgentMessageSendInput {
                        target,
                        message,
                        receiver_role,
                    })
                    .await?;
                Ok(receipt_value(&receipt))
            })
        }),
    );
}

// ---------------------------------------------------------------------------
// Agent observation
// ---------------------------------------------------------------------------

/// One roster row / agent summary.
#[derive(Debug, Clone, Default)]
pub struct AgentObserveSummary {
    pub active_session_id: Option<String>,
    pub session_id: String,
    pub session_name: Option<String>,
    pub relationship: Option<AgentFamilyRelationship>,
    pub runtime_kind: Option<String>,
    pub status: String,
    pub is_current: bool,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub attached_clients: usize,
    pub queued_count: usize,
    pub is_session_active: bool,
}

impl AgentObserveSummary {
    fn to_value(&self) -> Value {
        json!({
            "activeSessionId": self.active_session_id,
            "sessionId": self.session_id,
            "sessionName": self.session_name,
            "relationship": self.relationship.map(|r| r.as_str()),
            "runtimeKind": self.runtime_kind,
            "status": self.status,
            "isCurrent": self.is_current,
            "isStreaming": self.is_streaming,
            "isCompacting": self.is_compacting,
            "attachedClients": self.attached_clients,
            "queuedCount": self.queued_count,
            "isSessionActive": self.is_session_active,
        })
    }
}

/// One bounded message preview.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AgentObserveMessagePreview {
    pub index: usize,
    pub role: String,
    pub timestamp: Option<u64>,
    pub text: String,
    pub truncated: bool,
    pub tool_calls: Vec<String>,
    pub custom_type: Option<String>,
}

/// The controller the daemon supplies for `agent_observe.*` requests.
pub trait AgentObserveController: Send + Sync {
    fn list_agents(&self) -> impl Future<Output = anyhow::Result<Vec<AgentObserveSummary>>> + Send;
    fn get_agent(
        &self,
        target: &str,
    ) -> impl Future<Output = anyhow::Result<Option<AgentObserveSummary>>> + Send;
    fn recent_messages(
        &self,
        target: &str,
        limit: usize,
        max_chars: usize,
    ) -> impl Future<Output = anyhow::Result<Vec<AgentObserveMessagePreview>>> + Send;
}

/// Clamp an observe limit (default 8, range 1..=50).
pub fn normalize_observe_limit(limit: Option<u64>) -> anyhow::Result<usize> {
    clamp_integer(limit.unwrap_or(8), 1, 50, "agent_observe limit")
}

/// Clamp an observe preview width (default 800, range 80..=2000).
pub fn normalize_observe_max_chars(max_chars: Option<u64>) -> anyhow::Result<usize> {
    clamp_integer(
        max_chars.unwrap_or(800),
        80,
        2_000,
        "agent_observe max_chars",
    )
}

fn clamp_integer(value: u64, min: u64, max: u64, label: &str) -> anyhow::Result<usize> {
    if value < min || value > max {
        anyhow::bail!("{label} must be between {min} and {max}");
    }
    Ok(value as usize)
}

fn optional_integer(value: Option<&Value>, label: &str) -> anyhow::Result<Option<u64>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => {
            if let Some(integer) = number.as_u64() {
                Ok(Some(integer))
            } else {
                anyhow::bail!("{label} must be an integer when provided")
            }
        }
        Some(_) => anyhow::bail!("{label} must be an integer when provided"),
    }
}

/// Build one preview from a session message.
pub fn create_agent_observe_message_preview(
    message: &pa_types::session::AgentMessage,
    index: usize,
    max_chars: usize,
) -> AgentObserveMessagePreview {
    use pa_types::session::AgentMessage;
    let text = observe_message_text(message);
    let (text, truncated) = if text.chars().count() <= max_chars {
        (text, false)
    } else {
        (text.chars().take(max_chars).collect(), true)
    };
    let (role, timestamp, custom_type, tool_calls) = match message {
        AgentMessage::User(message) => ("user", Some(message.timestamp), None, Vec::new()),
        AgentMessage::Assistant(message) => (
            "assistant",
            Some(message.timestamp),
            None,
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    pa_types::ai::AssistantContentBlock::ToolCall(call) => Some(call.name.clone()),
                    _ => None,
                })
                .collect(),
        ),
        AgentMessage::ToolResult(message) => {
            ("toolResult", Some(message.timestamp), None, Vec::new())
        }
        AgentMessage::BashExecution(message) => {
            ("bashExecution", Some(message.timestamp), None, Vec::new())
        }
        AgentMessage::Custom(message) => (
            "custom",
            Some(message.timestamp),
            Some(message.custom_type.clone()),
            Vec::new(),
        ),
        AgentMessage::BranchSummary(message) => {
            ("branchSummary", Some(message.timestamp), None, Vec::new())
        }
        AgentMessage::CompactionSummary(message) => (
            "compactionSummary",
            Some(message.timestamp),
            None,
            Vec::new(),
        ),
    };
    AgentObserveMessagePreview {
        index,
        role: role.to_string(),
        timestamp,
        text,
        truncated,
        tool_calls,
        custom_type,
    }
}

fn user_content_text(content: &pa_types::ai::UserContent) -> String {
    match content {
        pa_types::ai::UserContent::Text(text) => text.clone(),
        pa_types::ai::UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                pa_types::ai::UserContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn observe_message_text(message: &pa_types::session::AgentMessage) -> String {
    use pa_types::session::AgentMessage;
    match message {
        AgentMessage::User(message) => user_content_text(&message.content),
        AgentMessage::Assistant(message) => message
            .content
            .iter()
            .filter_map(|block| match block {
                pa_types::ai::AssistantContentBlock::Text(text) => Some(text.text.clone()),
                pa_types::ai::AssistantContentBlock::Thinking(thinking) => {
                    Some(thinking.thinking.clone())
                }
                pa_types::ai::AssistantContentBlock::ToolCall(_) => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        AgentMessage::ToolResult(message) => {
            user_content_text(&pa_types::ai::UserContent::Blocks(message.content.clone()))
        }
        AgentMessage::BashExecution(message) => [message.command.clone(), message.output.clone()]
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        AgentMessage::Custom(message) => user_content_text(&message.content),
        AgentMessage::BranchSummary(message) => message.summary.clone(),
        AgentMessage::CompactionSummary(message) => message.summary.clone(),
    }
}

fn preview_value(preview: &AgentObserveMessagePreview) -> Value {
    json!({
        "index": preview.index,
        "role": preview.role,
        "timestamp": preview.timestamp,
        "text": preview.text,
        "truncated": preview.truncated,
        "toolCalls": if preview.tool_calls.is_empty() { Value::Null } else { json!(preview.tool_calls) },
        "customType": preview.custom_type,
    })
}

/// Register `agent_observe.*` handlers onto a handler map.
pub fn register_agent_observe_host_handlers<C: AgentObserveController + 'static>(
    controller: std::sync::Arc<C>,
    handlers: &mut HostRequestHandlers,
) {
    handlers.register(
        "agent_observe.list",
        host_handler({
            let controller = controller.clone();
            move |_payload| {
                let controller = controller.clone();
                Box::pin(async move {
                    let agents = controller.list_agents().await?;
                    Ok(json!({
                        "agents": agents.iter().map(AgentObserveSummary::to_value).collect::<Vec<_>>(),
                    }))
                })
            }
        }),
    );
    handlers.register(
        "agent_observe.get",
        host_handler({
            let controller = controller.clone();
            move |payload| {
                let controller = controller.clone();
                Box::pin(async move {
                    let Some(target) = payload.data.get("target").and_then(Value::as_str) else {
                        return Err(anyhow::anyhow!("agent_observe.get target must be a string"));
                    };
                    let Some(agent) = controller.get_agent(target).await? else {
                        anyhow::bail!("agent {target} is not reachable");
                    };
                    Ok(json!({ "agent": agent.to_value() }))
                })
            }
        }),
    );
    handlers.register(
        "agent_observe.recent",
        host_handler(move |payload| {
            let controller = controller.clone();
            Box::pin(async move {
                let Some(target) = payload.data.get("target").and_then(Value::as_str) else {
                    return Err(anyhow::anyhow!(
                        "agent_observe.recent target must be a string"
                    ));
                };
                let limit =
                    optional_integer(payload.data.get("limit"), "agent_observe.recent limit")?;
                let max_chars = optional_integer(
                    payload
                        .data
                        .get("max_chars")
                        .or_else(|| payload.data.get("maxChars")),
                    "agent_observe.recent max_chars",
                )?;
                let messages = controller
                    .recent_messages(
                        target,
                        normalize_observe_limit(limit)?,
                        normalize_observe_max_chars(max_chars)?,
                    )
                    .await?;
                Ok(json!({
                    "messages": messages.iter().map(preview_value).collect::<Vec<_>>(),
                }))
            })
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_ids_and_validation() {
        let id = create_agent_session_message_id();
        assert!(id.starts_with("agentmsg_"));
        assert!(is_agent_session_message_id(Some(&id)));
        assert!(!is_agent_session_message_id(Some("prompt123")));
        assert!(!is_agent_session_message_id(None));
        assert_eq!(
            normalize_agent_session_message("  hello  ").unwrap(),
            "hello"
        );
        assert!(normalize_agent_session_message("   ").is_err());
        assert!(normalize_agent_session_message("").is_err());
        let long = "x".repeat(DEFAULT_AGENT_MESSAGE_MAX_CHARS + 1);
        assert!(normalize_agent_session_message(&long).is_err());
        let at_limit = "x".repeat(DEFAULT_AGENT_MESSAGE_MAX_CHARS);
        assert!(normalize_agent_session_message(&at_limit).is_ok());
    }

    #[test]
    fn target_and_capacity_guards() {
        assert_eq!(
            assert_direct_agent_message_target(" worker ").unwrap(),
            "worker"
        );
        assert!(assert_direct_agent_message_target("").is_err());
        for broadcast in ["*", "all", "All", "BROADCAST"] {
            let error = assert_direct_agent_message_target(broadcast).unwrap_err();
            assert_eq!(
                error.to_string(),
                "Broadcast agent messaging is not supported"
            );
        }
        assert_agent_message_queue_capacity(3, 20).unwrap();
        let error = assert_agent_message_queue_capacity(20, 20).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Target session has too many pending messages: 20 unfinished, limit is 20"
        );
    }

    #[test]
    fn message_prompts_and_id_parsing() {
        let payload = AgentMessagePromptPayload {
            message: "keep going".to_string(),
            sender_name: "worker-1".to_string(),
            from_relationship: Some(AgentFamilyRelationship::Child),
        };
        let prompt = create_agent_session_message_prompt(&payload);
        assert_eq!(prompt, "[agent-message from child:worker-1]\n\nkeep going");
        // Header values are sanitized.
        let evil = AgentMessagePromptPayload {
            message: "m".to_string(),
            sender_name: "bad name!".to_string(),
            from_relationship: None,
        };
        assert_eq!(
            create_agent_session_message_prompt(&evil),
            "[agent-message from bad_name_]\n\nm"
        );
        // Legacy transcript header parsing.
        let header = format!(
            "Agent-to-agent message received.\nSource: {AGENT_MESSAGE_SOURCE}\nTo: worker\nMessage id: {}",
            create_agent_session_message_id()
        );
        let parsed = parse_agent_session_message_prompt_id(&header).unwrap();
        assert!(parsed.starts_with("agentmsg_"));
        assert!(is_agent_session_message_prompt(&header));
        assert!(!is_agent_session_message_prompt("plain text"));
    }

    #[test]
    fn observe_limits_clamp() {
        assert_eq!(normalize_observe_limit(None).unwrap(), 8);
        assert_eq!(normalize_observe_limit(Some(50)).unwrap(), 50);
        assert!(normalize_observe_limit(Some(51)).is_err());
        assert!(normalize_observe_limit(Some(0)).is_err());
        assert_eq!(normalize_observe_max_chars(None).unwrap(), 800);
        assert_eq!(normalize_observe_max_chars(Some(80)).unwrap(), 80);
        assert!(normalize_observe_max_chars(Some(79)).is_err());
        assert!(normalize_observe_max_chars(Some(2_001)).is_err());
    }

    #[test]
    fn observe_previews_truncate() {
        let user = pa_types::session::AgentMessage::User(pa_types::ai::UserMessage {
            content: pa_types::ai::UserContent::Text("short".to_string()),
            timestamp: 42,
            rest: Default::default(),
        });
        let preview = create_agent_observe_message_preview(&user, 3, 800);
        assert_eq!(preview.index, 3);
        assert_eq!(preview.role, "user");
        assert_eq!(preview.timestamp, Some(42));
        assert_eq!(preview.text, "short");
        assert!(!preview.truncated);
        let long = pa_types::session::AgentMessage::User(pa_types::ai::UserMessage {
            content: pa_types::ai::UserContent::Text("x".repeat(100)),
            timestamp: 0,
            rest: Default::default(),
        });
        let clipped = create_agent_observe_message_preview(&long, 0, 10);
        assert!(clipped.truncated);
        assert_eq!(clipped.text.chars().count(), 10);
    }

    struct RecordingMessageController;

    impl AgentMessageController for RecordingMessageController {
        async fn send_agent_message(
            &self,
            input: AgentMessageSendInput,
        ) -> anyhow::Result<AgentMessageReceipt> {
            Ok(AgentMessageReceipt {
                id: create_agent_session_message_id(),
                target: input.target,
                message: input.message,
                delivery_status: AgentMessageDeliveryStatus::Delivered,
                delivery_mode: Some("steer"),
                receiver_role: input.receiver_role,
                delivered_at: Some("2024-01-01T00:00:00.000Z".to_string()),
                queued_at: None,
            })
        }
    }

    #[tokio::test]
    async fn message_host_handler_round_trip() {
        let mut handlers = HostRequestHandlers::default();
        register_agent_message_host_handlers(
            std::sync::Arc::new(RecordingMessageController),
            &mut handlers,
        );
        let send = handlers.get("agent_message.send").unwrap().clone();
        let receipt = send(crate::kernel::shared::HostRequestPayload {
            data: json!({
                "target": "worker",
                "message": "  proceed  ",
                "receiver_role": "child"
            }),
            cell_source_code: None,
        })
        .await
        .unwrap();
        assert_eq!(receipt["target"], "worker");
        assert_eq!(receipt["message"], "proceed");
        assert_eq!(receipt["deliveryStatus"], "delivered");
        assert_eq!(receipt["receiverRole"], "child");
        assert!(receipt["id"].as_str().unwrap().starts_with("agentmsg_"));
        // Payload validation.
        let error = send(crate::kernel::shared::HostRequestPayload {
            data: json!({ "message": "hi" }),
            cell_source_code: None,
        })
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "agent_message.send target must be a string"
        );
        let error = send(crate::kernel::shared::HostRequestPayload {
            data: json!({ "target": "*", "message": "hi" }),
            cell_source_code: None,
        })
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Broadcast agent messaging is not supported"
        );
    }
}
