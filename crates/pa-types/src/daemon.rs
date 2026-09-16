//! Daemon wire protocol, ported from
//! `packages/coding-agent/src/modes/daemon/daemon-protocol.ts` and
//! `daemon-worker-protocol.ts`.
//!
//! This is the local JSONL transport between clients (TUI/CLI), the supervisor,
//! and per-session worker processes. Frame and command shapes match the TS
//! wire format exactly. Payloads owned by other subsystems (session summaries,
//! agent-connection state objects, session events) are carried as opaque
//! [`Value`]s and will gain typed shapes in their owning crates.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::AgentMessage;
use crate::JsonMap;

pub const DAEMON_PROTOCOL_NAME: &str = "prime-agent.daemon";
pub const DAEMON_PROTOCOL_VERSION: u64 = 7;
pub const DAEMON_SCHEMA_REVISION: u64 = 28;
pub const DAEMON_SCHEMA_ID: &str = "protocol-7-schema-28-92bc5368a082";

pub type DaemonClientId = String;
pub type DaemonCommandId = String;
pub type DaemonEventId = String;
pub type DaemonEventSequence = u64;
/// Client/server capability wire strings (closed TS unions, open on the wire
/// for older/newer builds, so carried as raw strings).
pub type DaemonClientCapability = String;
pub type DaemonServerCapability = String;

// ---------------------------------------------------------------------------
// Common frames
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonProtocolInfo {
    pub name: String,
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonEventCursor {
    pub generation: String,
    pub sequence: DaemonEventSequence,
}

/// Resume cursor accepted on attach. The TS wire shape is a union: either a
/// `DaemonEventCursor` (`generation` + `sequence`, optionally with
/// `activeSessionId`) or a bare `eventSequence` with optional
/// `activeSessionId`. A single optional-field struct accepts both forms and
/// serializes each back to its original shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonResumeCursor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<DaemonEventSequence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_sequence: Option<DaemonEventSequence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonReplayStatus {
    Complete,
    Partial,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonReplayInfo {
    pub status: DaemonReplayStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_sequence: Option<DaemonEventSequence>,
    pub to_sequence: DaemonEventSequence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_cursor: Option<DaemonEventCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_cursor: Option<DaemonEventCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonEventMeta {
    pub id: DaemonEventId,
    pub protocol: DaemonProtocolInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<DaemonEventSequence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<DaemonEventCursor>,
    pub emitted_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replayed: Option<bool>,
}

// ---------------------------------------------------------------------------
// Commands (client/worker -> supervisor/worker)
// ---------------------------------------------------------------------------

/// `type: "command"` envelope wrapping a [`DaemonCommand`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonCommandEnvelope {
    /// Fixed `"command"` frame tag.
    #[serde(rename = "type")]
    pub frame_type: DaemonCommandFrameType,
    pub id: DaemonCommandId,
    pub protocol: DaemonProtocolInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<DaemonClientId>,
    pub command: DaemonCommand,
}

/// Frame tag of [`DaemonCommandEnvelope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonCommandFrameType {
    #[serde(rename = "command")]
    Command,
}

/// A bare command or a command envelope, both accepted on one socket line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DaemonCommandWire {
    Command(DaemonCommand),
    Envelope(DaemonCommandEnvelope),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamingBehavior {
    Steer,
    FollowUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonSessionLifecycle {
    Resident,
    ClientOwned,
}

/// `prompt`/`steer`/`follow_up`-family input payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming_behavior: Option<StreamingBehavior>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_if_busy: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expand_prompt_templates: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_message: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix_messages: Option<Value>,
    /// Unique only when the caller needs cancellable pre-ownership admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_id: Option<String>,
}

/// Client commands, tagged by `type`. Every variant also carries `id` (when
/// sent as a bare command) and a catch-all for unknown fields, so wire
/// round-trips are lossless across schema revisions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DaemonCommand {
    List {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        all: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_dir: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        include_client_owned: Option<bool>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    /// `list_saved_sessions` (session-addressed or cwd-addressed forms share
    /// this shape; unaddressed fields stay absent).
    ListSavedSessions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_dir: Option<String>,
        #[serde(default)]
        scope: Value,
        #[serde(flatten)]
        rest: JsonMap,
    },
    ListAgentPeers {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        worker_token: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetDirectWorkerTransport {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    RosterSubscribe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    RosterUnsubscribe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    Create {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        continue_recent: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        no_session: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_metadata: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lifecycle: Option<DaemonSessionLifecycle>,
        /// Allowlisted client env vars (`env`), carried on create only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<std::collections::BTreeMap<String, String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch_env: Option<std::collections::BTreeMap<String, String>>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    Attach {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supports_extension_ui: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<DaemonClientId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capabilities: Option<Vec<DaemonClientCapability>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_cursor: Option<DaemonResumeCursor>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        telemetry_disabled: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery_config: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<std::collections::BTreeMap<String, String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch_env: Option<std::collections::BTreeMap<String, String>>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    Reattach {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        target_active_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supports_extension_ui: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<DaemonClientId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capabilities: Option<Vec<DaemonClientCapability>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_cursor: Option<DaemonResumeCursor>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        telemetry_disabled: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery_config: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<std::collections::BTreeMap<String, String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch_env: Option<std::collections::BTreeMap<String, String>>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    Detach {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_session_id: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    CompleteOwnedSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    PromoteOwnedSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    Kill {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    Rename {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        name: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    Prompt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        message: String,
        #[serde(flatten)]
        input: PromptInput,
        #[serde(flatten)]
        rest: JsonMap,
    },
    CancelPromptAdmission {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        admission_id: String,
        /// Cancel session-owned work too when it has not started delivery.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cancel_owned: Option<bool>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    PromptAndWait {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        message: String,
        #[serde(flatten)]
        input: PromptInput,
        #[serde(flatten)]
        rest: JsonMap,
    },
    Steer {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        message: String,
        #[serde(flatten)]
        input: PromptInput,
        #[serde(flatten)]
        rest: JsonMap,
    },
    FollowUp {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        message: String,
        #[serde(flatten)]
        input: PromptInput,
        #[serde(flatten)]
        rest: JsonMap,
    },
    RestoreNextTurn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        messages: Vec<AgentMessage>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    RestoreActions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        snapshot: Value,
        #[serde(flatten)]
        rest: JsonMap,
    },
    AppendCustomMessage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        message: Value,
        #[serde(flatten)]
        rest: JsonMap,
    },
    ResumeQueue {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SendMessage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        target_active_session_id: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_active_session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_origin: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delivery_mode: Option<Value>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    AgentMessagesStatus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_session_id: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    AgentMessagesPause {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_session_id: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    AgentMessagesResume {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_session_id: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    AgentMessagesClear {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    Abort {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    StartSideQuestion {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        side_question_id: String,
        question: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        previous_turns: Option<Value>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    AbortSideQuestion {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        side_question_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    ExecuteBash {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exclude_from_context: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transient: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    AbortBash {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    CancelRlmChild {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        child_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    DeleteRlmSubagent {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        child_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    WaitForIdle {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    WaitForHeadlessCompletion {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait_for_rlm_quiescence: Option<bool>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetSessionHeader {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetState {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetConnectionState {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetMessages {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetRlmChildren {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetSessionStats {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetContextTree {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetCommands {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetResourceSnapshot {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    ReplaceAcpMcpServers {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        owner_id: String,
        servers: Value,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetModelCatalog {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetAvailableModels {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetQueue {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    MutateQueuedMessage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        lane: Value,
        index: u64,
        expected_text: String,
        mutation: Value,
        #[serde(flatten)]
        rest: JsonMap,
    },
    ClearQueue {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    AbortAndClearQueue {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    AcquireSessionInputPause {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        lease_key: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    ReleaseSessionInputPause {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        pause_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    CronList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        include_inactive: Option<bool>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    HeartbeatsList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_session_id: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    HeartbeatManage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        job_id: String,
        action: Value,
        #[serde(flatten)]
        rest: JsonMap,
    },
    CronAdd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        schedule: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        promote_owned_session: Option<bool>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    CronCancel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_session_id: Option<String>,
        job_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    HeartbeatGet {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    HeartbeatSet {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        schedule: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delivery_mode: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        promote_owned_session: Option<bool>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    HeartbeatUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        action: Value,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SetModel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        provider: String,
        model_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    CycleModel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        direction: Option<CycleDirection>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SetScopedModels {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        scoped_models: Value,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SetThinkingLevel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        level: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SetServiceTier {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        service_tier: Option<crate::ai::ServiceTier>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    CycleThinkingLevel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SetTransport {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        transport: crate::ai::Transport,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SetSteeringMode {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        mode: Value,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SetFollowUpMode {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        mode: Value,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SetAutoCompaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        enabled: bool,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SetAutoRetry {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        enabled: bool,
        #[serde(flatten)]
        rest: JsonMap,
    },
    Compact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    Refine {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rollback_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        global: Option<bool>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    AbortCompaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    AbortBranchSummary {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    AbortRetry {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    ExecuteBashAndWait {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        command: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    Reload {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    NewSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_session: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SwitchSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        session_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd_override: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    Fork {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        entry_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        position: Option<ForkPosition>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    NavigateTree {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        target_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summarize: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replace_instructions: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    ImportJsonl {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        input_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd_override: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    ExportHtml {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_path: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    ExportJsonl {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_path: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SetSessionName {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worker_token: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetRlmMaxDepthStatus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SetRlmMaxDepth {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        max_depth: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        global: Option<bool>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    RenameSavedSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_session_id: Option<String>,
        session_path: String,
        name: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    DeleteSavedSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_session_id: Option<String>,
        session_path: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetSessionContext {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetSessionTree {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetUserMessagesForForking {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetLastAssistantText {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetSystemPrompt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    GetToolDefinition {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        name: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SetSessionEntryLabel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        entry_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    ExtensionUiResponse {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        request_id: String,
        response: DaemonExtensionUiResponse,
        #[serde(flatten)]
        rest: JsonMap,
    },
    AckResult {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        command_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    PrepareUpdateRestart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    RetryWorker {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    Restart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    Shutdown {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        force: Option<bool>,
        #[serde(flatten)]
        rest: JsonMap,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CycleDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForkPosition {
    Before,
    At,
}

/// Response payload of an `extension_ui_request` dialog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DaemonExtensionUiResponse {
    Value { value: String },
    Confirmed { confirmed: bool },
    Cancelled { cancelled: bool },
}

// ---------------------------------------------------------------------------
// Responses and outbound events
// ---------------------------------------------------------------------------

/// `type: "response"`: success or failure outcome of one command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub command: String,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_info: Option<DaemonErrorInfo>,
}

/// Structured failure info carried on error responses, tagged by `code`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "code",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DaemonErrorInfo {
    MissingSessionCwd {
        issue: Value,
    },
    SessionImportFileNotFound {
        file_path: String,
    },
    SessionAlreadyActive {
        session_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_session_id: Option<String>,
    },
    SessionRecovering {
        active_session_id: String,
    },
    CommandResultUncertain {
        client_id: DaemonClientId,
        command_id: DaemonCommandId,
    },
}

/// Saved-session row pushed by `session_list_item` progress events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonSavedSessionInfo {
    pub path: String,
    pub id: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rlm_depth: Option<u64>,
    pub created: String,
    pub modified: String,
    pub message_count: u64,
    pub first_message: String,
    pub all_messages_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Value>,
    #[serde(flatten)]
    pub rest: JsonMap,
}

/// Full session snapshot (attach, replacement, resync).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonSessionSnapshot {
    pub active_session_id: String,
    pub summary: Value,
    pub state: Value,
    pub messages: Vec<AgentMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_context: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_tree: Option<Value>,
    pub last_event_sequence: DaemonEventSequence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_cursor: Option<DaemonEventCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children: Option<Value>,
    #[serde(flatten)]
    pub rest: JsonMap,
}

/// Process identity of the daemon build, published in `daemon_hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonRuntimeIdentity {
    pub build_id: String,
    pub executable_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launcher_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonClosingReason {
    Shutdown,
    Update,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonSessionClosedReason {
    Killed,
    Shutdown,
    Completed,
    Replaced,
    Update,
}

/// Single-use credential for one direct TUI-to-worker connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonPeerTransportTicket {
    pub purpose: String,
    pub socket_path: String,
    pub socket_identity: SocketIdentity,
    pub worker_instance_id: String,
    pub active_session_id: String,
    pub grant_id: String,
    pub token: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SocketIdentity {
    pub dev: u64,
    pub ino: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotPurpose {
    Attach,
    Replacement,
    Catchup,
}

/// `type: "event"` envelope wrapping a [`DaemonOutbound`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonEventEnvelope {
    pub id: DaemonEventId,
    pub protocol: DaemonProtocolInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<DaemonEventSequence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<DaemonEventCursor>,
    pub emitted_at: String,
    pub event: DaemonOutbound,
}

/// Supervisor -> client frames, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DaemonOutbound {
    Response {
        #[serde(flatten)]
        response: DaemonResponse,
    },
    SessionListProgress {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_session_id: Option<String>,
        loaded: u64,
        total: u64,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SessionListItem {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_session_id: Option<String>,
        session: DaemonSavedSessionInfo,
        #[serde(flatten)]
        rest: JsonMap,
    },
    DaemonHello {
        socket_path: String,
        protocol: DaemonProtocolInfo,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema_revision: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app_version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime: Option<DaemonRuntimeIdentity>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supervisor_generation: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supervisor_pid: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supervisor_owner_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supervisor_process_start_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supervisor_socket_path: Option<String>,
        client_id: DaemonClientId,
        server_capabilities: Vec<DaemonServerCapability>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    DaemonClosing {
        reason: DaemonClosingReason,
        #[serde(flatten)]
        rest: JsonMap,
    },
    HeartbeatsChanged {
        #[serde(flatten)]
        rest: JsonMap,
    },
    RosterUpdate {
        changed: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        removed: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resync: Option<bool>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SessionEvent {
        active_session_id: String,
        event: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<DaemonEventMeta>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SideQuestionEvent {
        active_session_id: String,
        event: Value,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SessionStatus {
        active_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recap: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<DaemonEventMeta>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SessionReplaced {
        active_session_id: String,
        state: Value,
        messages: Vec<AgentMessage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot_follows: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<DaemonEventMeta>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SessionResynced {
        active_session_id: String,
        snapshot: DaemonSessionSnapshot,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<DaemonEventMeta>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SessionAttached {
        active_session_id: String,
        state: Value,
        messages: Vec<AgentMessage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot: Option<DaemonSessionSnapshot>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay: Option<DaemonReplayInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_event_sequence: Option<DaemonEventSequence>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SessionSnapshotBegin {
        active_session_id: String,
        snapshot_id: String,
        #[serde(flatten)]
        snapshot: Value,
        message_count: u64,
        target_chunk_bytes: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        purpose: Option<SnapshotPurpose>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SessionSnapshotChunk {
        active_session_id: String,
        snapshot_id: String,
        index: u64,
        messages: Vec<AgentMessage>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SessionSnapshotEnd {
        active_session_id: String,
        snapshot_id: String,
        chunk_count: u64,
        last_event_sequence: DaemonEventSequence,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_event_cursor: Option<DaemonEventCursor>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SessionSnapshotFailed {
        active_session_id: String,
        snapshot_id: String,
        error: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SessionDetached {
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    SessionClosed {
        active_session_id: String,
        reason: DaemonSessionClosedReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<DaemonEventMeta>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    ExtensionUiRequest {
        active_session_id: String,
        id: String,
        method: String,
        payload: JsonMap,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<DaemonEventMeta>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    ExtensionError {
        active_session_id: String,
        extension_path: String,
        event: String,
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<DaemonEventMeta>,
        #[serde(flatten)]
        rest: JsonMap,
    },
}

// ---------------------------------------------------------------------------
// Worker protocol (supervisor <-> worker)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonWorkerLifecycle {
    Starting,
    Ready,
    Recovering,
    Stopping,
    Failed,
}

/// Worker -> supervisor roster frames, outside the client-facing schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DaemonWorkerRosterOutbound {
    RosterDelta {
        entries: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        removed_agent_ids: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot: Option<bool>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    RosterHeartbeat {
        #[serde(flatten)]
        rest: JsonMap,
    },
}

/// Frame header the worker writes to the supervisor pipe, tagging the payload
/// that follows in the same frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DaemonWorkerFrameHeader {
    Command {
        request_id: String,
        command_type: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    Outbound {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        outbound_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_event_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload_encoding: Option<PayloadEncoding>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot_purpose: Option<SnapshotPurpose>,
        #[serde(flatten)]
        rest: JsonMap,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PayloadEncoding {
    Jsonl,
    AssistantDelta,
}

/// A single-use, worker-memory-only admission for one direct peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonWorkerPeerGrant {
    pub grant_id: String,
    pub token: String,
    pub expires_at: String,
    pub purpose: String,
    pub worker_instance_id: String,
    pub active_session_id: String,
    pub issuer_generation: String,
}

/// Commands a direct peer may send before it holds an authenticated role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DaemonPeerCommand {
    PeerAuth {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        grant_id: String,
        token: String,
        worker_instance_id: String,
        purpose: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
}

/// Worker lifecycle commands on the supervisor -> worker channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DaemonWorkerCommand {
    WorkerAuth {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worker_instance_id: Option<String>,
        supervisor_generation: String,
        supervisor_pid: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supervisor_process_start_id: Option<String>,
        supervisor_socket_path: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    WorkerSubscribe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capabilities: Option<Vec<DaemonClientCapability>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supports_extension_ui: Option<bool>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    WorkerUnsubscribe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        active_session_id: String,
        #[serde(flatten)]
        rest: JsonMap,
    },
    WorkerRegisterPeerTransport {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        grant: DaemonWorkerPeerGrant,
        #[serde(flatten)]
        rest: JsonMap,
    },
    WorkerArchiveAndShutdown {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    WorkerPassivateIdleChildren {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        idle_eviction_minutes: Value,
        now: u64,
        limit: u64,
        #[serde(flatten)]
        rest: JsonMap,
    },
    WorkerDeliverMessage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        target_active_session_id: String,
        message: String,
        sender: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delivery_mode: Option<Value>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    WorkerPrepareUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    WorkerCommitUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
    WorkerCancelUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(flatten)]
        rest: JsonMap,
    },
}

/// The subset of `create` persisted in the durable worker descriptor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DurableDaemonCreateCommand {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_session: Option<bool>,
    #[serde(flatten)]
    pub rest: JsonMap,
}

/// Durable supervisor-side worker record (recovery journal), version 1 or 2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonWorkerDescriptor {
    pub version: u32,
    pub worker_id: String,
    pub pid: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_start_id: Option<String>,
    pub socket_path: String,
    pub recovery_journal_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphan_process_journal_path: Option<String>,
    pub supervisor_socket_path: String,
    pub authentication_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_instance_id: Option<String>,
    pub root_active_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_disabled: Option<bool>,
    pub created_at: String,
    pub updated_at: String,
    pub lifecycle: DaemonWorkerLifecycle,
    pub create_command: DurableDaemonCreateCommand,
    pub consecutive_failures: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_requested_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_on_stop: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(flatten)]
    pub rest: JsonMap,
}

// ---------------------------------------------------------------------------
// Update-restart manifest
// ---------------------------------------------------------------------------

pub const DAEMON_UPDATE_RESTART_FORMAT_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonUpdateRestartQueue {
    pub actions: Value,
    pub next_turn: Vec<AgentMessage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonUpdateRestartSession {
    pub active_session_id: String,
    pub session_id: String,
    pub session_file: String,
    pub cwd: String,
    pub config: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_env: Option<std::collections::BTreeMap<String, String>>,
    pub queue: DaemonUpdateRestartQueue,
    pub should_resume: bool,
    pub was_streaming: bool,
    pub was_compacting: bool,
    pub was_bash_running: bool,
    pub had_running_rlm_children: bool,
    pub was_retrying: bool,
    pub had_accepted_prompt_in_flight: bool,
    #[serde(flatten)]
    pub rest: JsonMap,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonUpdateRestartManifest {
    pub format_version: u64,
    pub created_at: String,
    pub sessions: Vec<DaemonUpdateRestartSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discarded_active_session_ids: Option<Vec<String>>,
    #[serde(flatten)]
    pub rest: JsonMap,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt<T: serde::Serialize + for<'de> Deserialize<'de>>(json: &str) {
        let original: serde_json::Value = serde_json::from_str(json).unwrap();
        let parsed: T = serde_json::from_str(json).expect("deserialize");
        let out = serde_json::to_string(&parsed).expect("serialize");
        let reparsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(original, reparsed, "round trip changed the value: {out}");
    }

    #[test]
    fn command_wire_bare_and_envelope() {
        rt::<DaemonCommandWire>(r#"{"type":"list","id":"c1","cwd":"/w","future":1}"#);
        rt::<DaemonCommandWire>(
            r#"{"type":"command","id":"e1","protocol":{"name":"prime-agent.daemon","version":7},"clientId":"cl","command":{"type":"prompt","activeSessionId":"s1","message":"hi","queueIfBusy":true,"extra":"kept"}}"#,
        );
    }

    #[test]
    fn prompt_family_roundtrip() {
        rt::<DaemonCommand>(
            r#"{"type":"steer","activeSessionId":"s1","message":"m","content":[{"type":"text","text":"x"}],"streamingBehavior":"steer","queueKey":"q","admissionId":"a1"}"#,
        );
        rt::<DaemonCommand>(
            r#"{"type":"cancel_prompt_admission","activeSessionId":"s1","admissionId":"a1","cancelOwned":true}"#,
        );
    }

    #[test]
    fn response_and_error_info_roundtrip() {
        rt::<DaemonOutbound>(
            r#"{"type":"response","id":"r1","command":"prompt","success":true,"data":{"x":1}}"#,
        );
        rt::<DaemonOutbound>(
            r#"{"type":"response","id":"r2","command":"attach","success":false,"error":"boom","errorInfo":{"code":"session_already_active","sessionPath":"/s.jsonl","activeSessionId":"a"}}"#,
        );
        rt::<DaemonOutbound>(
            r#"{"type":"response","command":"import_jsonl","success":false,"error":"e","errorInfo":{"code":"session_import_file_not_found","filePath":"/x"}}"#,
        );
    }

    #[test]
    fn hello_and_snapshot_roundtrip() {
        rt::<DaemonOutbound>(
            r#"{"type":"daemon_hello","socketPath":"/sock","protocol":{"name":"prime-agent.daemon","version":7},"schemaId":"protocol-7-schema-28-92bc5368a082","schemaRevision":28,"appVersion":"1.0","supervisorGeneration":"g","supervisorPid":42,"clientId":"c","serverCapabilities":["attach_snapshot","event_sequence"]}"#,
        );
        let msg = r#"{"role":"user","content":"hi","timestamp":1}"#;
        rt::<DaemonOutbound>(&format!(
            r#"{{"type":"session_attached","activeSessionId":"s","state":{{"a":1}},"messages":[{msg}],"replay":{{"status":"complete","toSequence":5}},"lastEventSequence":5}}"#
        ));
        rt::<DaemonOutbound>(
            r#"{"type":"session_snapshot_chunk","activeSessionId":"s","snapshotId":"sn","index":0,"messages":[{"role":"assistant","content":[{"type":"text","text":"t"}],"api":"a","provider":"p","model":"m","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":1}]}"#,
        );
    }

    #[test]
    fn worker_frames_roundtrip() {
        rt::<DaemonWorkerFrameHeader>(
            r#"{"kind":"outbound","requestId":"r","outboundType":"session_event","activeSessionId":"s","payloadEncoding":"jsonl","snapshotPurpose":"attach"}"#,
        );
        rt::<DaemonWorkerCommand>(
            r#"{"type":"worker_auth","token":"t","supervisorGeneration":"g","supervisorPid":1,"supervisorSocketPath":"/s"}"#,
        );
        rt::<DaemonWorkerDescriptor>(
            r#"{"version":2,"workerId":"w","pid":9,"socketPath":"/w.sock","recoveryJournalPath":"/j","supervisorSocketPath":"/s","authenticationToken":"t","rootActiveSessionId":"a","createdAt":"c","updatedAt":"u","lifecycle":"ready","createCommand":{"sessionPath":"/p"},"consecutiveFailures":0}"#,
        );
        rt::<DaemonPeerCommand>(
            r#"{"type":"peer_auth","grantId":"g","token":"t","workerInstanceId":"w","purpose":"session_client"}"#,
        );
    }

    #[test]
    fn resume_cursor_and_replay_roundtrip() {
        rt::<DaemonResumeCursor>(r#"{"generation":"g","sequence":3,"activeSessionId":"s"}"#);
        rt::<DaemonResumeCursor>(r#"{"activeSessionId":"s","eventSequence":3}"#);
        rt::<DaemonReplayInfo>(
            r#"{"status":"unavailable","fromSequence":1,"toSequence":5,"fromCursor":{"generation":"g","sequence":1},"toCursor":{"generation":"g","sequence":5},"reason":"event_replay_not_available"}"#,
        );
    }

    #[test]
    fn extension_ui_response_variants() {
        rt::<DaemonExtensionUiResponse>(r#"{"value":"pick"}"#);
        rt::<DaemonExtensionUiResponse>(r#"{"confirmed":true}"#);
        rt::<DaemonExtensionUiResponse>(r#"{"cancelled":true}"#);
    }

    #[test]
    fn update_restart_manifest_roundtrip() {
        rt::<DaemonUpdateRestartManifest>(
            r#"{"formatVersion":1,"createdAt":"t","sessions":[{"activeSessionId":"a","sessionId":"s","sessionFile":"/f","cwd":"/w","config":{"x":1},"queue":{"actions":{"a":[]},"nextTurn":[]},"shouldResume":true,"wasStreaming":false,"wasCompacting":false,"wasBashRunning":false,"hadRunningRlmChildren":false,"wasRetrying":false,"hadAcceptedPromptInFlight":false}],"discardedActiveSessionIds":["z"]}"#,
        );
    }

    #[test]
    fn protocol_constants_match_ts() {
        assert_eq!(DAEMON_PROTOCOL_NAME, "prime-agent.daemon");
        assert_eq!(DAEMON_PROTOCOL_VERSION, 7);
        assert_eq!(DAEMON_SCHEMA_REVISION, 28);
        assert_eq!(DAEMON_SCHEMA_ID, "protocol-7-schema-28-92bc5368a082");
        assert_eq!(DAEMON_UPDATE_RESTART_FORMAT_VERSION, 1);
    }
}
