//! Post-turn status line for daemon-hosted sessions: after each completed
//! turn (and on a periodic sweep while working), the worker asks a small
//! dashboard model for a one-line recap plus a completed/needs-input
//! verdict, keeps it in memory, and broadcasts it to attached clients as
//! `session_status`. Port of `daemon-session-summarizer.ts`; the trigger is
//! the worker's `turn_end` event, debounced until the session settles.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

use crate::protocol::{create_daemon_event_meta, DaemonOutbound};
use crate::worker::OutboundFrame;
use pa_types::ai::Model;

/// Collapse a tool loop's rapid turn bursts into one summarization.
pub const SETTLE_DEBOUNCE_MS: u64 = 2_000;
/// Periodic refresh of working sessions (the TS sweep).
pub const SWEEP_INTERVAL_MS: u64 = 25_000;

const SUMMARY_CONTEXT_MESSAGES: usize = 8;
const SUMMARY_MAX_CHARS_PER_MESSAGE: usize = 600;
/// Generous so a chatty model still closes the tags before truncation.
const SUMMARY_MAX_TOKENS: u64 = 400;

const SUMMARY_MODEL_PROVIDER: &str = "prime-inference";
const SUMMARY_MODEL_ID: &str = "qwen/qwen3-30b-a3b-instruct-2507";

pub const AGENT_STATUS_SYSTEM_PROMPT: &str = "You generate a status line for an AI coding agent dashboard. You are given the recent conversation between a user and the agent, plus whether the agent is currently working or idle.

Output ONLY these two tags, nothing before, between, or after. Do not think out loud, explain, or count words.
<recap>a present-tense clause, at most 12 words, saying what the agent is doing or just did, no trailing period</recap>
<status>one of NEEDS_INPUT, COMPLETED</status>

STATUS meaning:
- COMPLETED: the agent finished its turn AND the user's request is fully done with nothing left.
- NEEDS_INPUT: the agent finished its turn but the task is not fully done — it asked a question, hit a blocker, or needs more prompting.
When you are unsure between COMPLETED and NEEDS_INPUT, choose NEEDS_INPUT.

Example:
<recap>Refactoring the auth middleware and updating its tests</recap>
<status>NEEDS_INPUT</status>";

/// Idle verdict: did the turn fully complete the request or is more input
/// needed (TS `AgentTaskState`, wire strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTaskState {
    NeedsInput,
    Completed,
}

impl AgentTaskState {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentTaskState::NeedsInput => "needs_input",
            AgentTaskState::Completed => "completed",
        }
    }
}

/// Read/write access to the live session state the runner summarizes. The
/// worker owns the real implementation; the split keeps the summarization
/// logic independent of the session store internals.
pub trait StatusSession: Send + Sync {
    /// The branch messages in wire shape (role/content).
    fn status_messages(&self) -> Vec<Value>;
    /// Whether a turn is running right now.
    fn status_busy(&self) -> bool;
    /// The session's active id (routing + event meta).
    fn status_active_session_id(&self) -> String;
    /// The session's event-stream generation (event meta).
    fn status_generation(&self) -> String;
    /// Allocate the next event sequence number.
    fn status_next_sequence(&mut self) -> u64;
}

/// The daemon worker's status-line runner: turn-end notifications (debounced)
/// and periodic sweeps drive one cheap model call per settled state. The
/// latest verdict stays in memory; `session_status` broadcasts go to the
/// session's attached clients.
pub struct StatusLineRunner<S: StatusSession> {
    core: Arc<std::sync::Mutex<S>>,
    agent_dir: PathBuf,
    events: tokio::sync::broadcast::Sender<Arc<OutboundFrame>>,
    state: std::sync::Mutex<Option<StatusState>>,
}

/// The in-memory status the worker keeps per session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusState {
    summary: String,
    task_state: Option<AgentTaskState>,
    based_on_message_count: usize,
}

/// What woke the runner: a settled turn or the periodic sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotifySource {
    TurnSettled,
    Sweep,
}

impl<S: StatusSession> StatusLineRunner<S> {
    pub(crate) fn new(
        core: Arc<std::sync::Mutex<S>>,
        agent_dir: PathBuf,
        events: tokio::sync::broadcast::Sender<Arc<OutboundFrame>>,
    ) -> Self {
        StatusLineRunner {
            core,
            agent_dir,
            events,
            state: std::sync::Mutex::new(None),
        }
    }

    /// Serve until the notify channel closes (worker shutdown). A settled
    /// turn debounces until the session stops bursting; the sweep runs
    /// directly (it does not consume turn notifications).
    pub async fn run(self: Arc<Self>, mut notify: tokio::sync::mpsc::UnboundedReceiver<()>) {
        let mut sweep = tokio::time::interval(Duration::from_millis(SWEEP_INTERVAL_MS));
        loop {
            let turn_settled = tokio::select! {
                maybe = notify.recv() => match maybe {
                    Some(()) => true,
                    None => return,
                },
                _ = sweep.tick() => {
                    self.summarize(NotifySource::Sweep).await;
                    continue;
                }
            };
            if !turn_settled {
                continue;
            }
            // Debounce: restart the settle window on every burst member.
            let mut deadline =
                tokio::time::Instant::now() + Duration::from_millis(SETTLE_DEBOUNCE_MS);
            loop {
                match tokio::time::timeout_at(deadline, notify.recv()).await {
                    Ok(Some(())) => {
                        deadline =
                            tokio::time::Instant::now() + Duration::from_millis(SETTLE_DEBOUNCE_MS);
                    }
                    Ok(None) => return,
                    Err(_) => break,
                }
            }
            self.summarize(NotifySource::TurnSettled).await;
        }
    }

    /// One summarization pass: read the session state, fast-return unchanged
    /// idle verdicts, call the small model, publish on change.
    async fn summarize(&self, source: NotifySource) {
        let (messages, is_working, active_session_id, generation) = {
            let core = self.core.lock().expect("session core lock");
            (
                core.status_messages(),
                core.status_busy(),
                core.status_active_session_id(),
                core.status_generation(),
            )
        };
        if messages.is_empty() {
            return;
        }
        // The sweep refreshes working sessions only; idle sessions are
        // driven by turn notifications.
        if matches!(source, NotifySource::Sweep) && !is_working {
            return;
        }
        let message_count = messages.len();
        let previous = self.state.lock().expect("status state lock").clone();
        // Idle sessions with a current verdict need no refresh; working
        // sessions always refresh so the recap keeps up.
        let content_unchanged = previous
            .as_ref()
            .is_some_and(|state| state.based_on_message_count == message_count);
        let owes_idle_verdict = !is_working
            && previous
                .as_ref()
                .is_none_or(|state| state.task_state.is_none());
        // A blank recap means the model call has not succeeded yet; keep
        // retrying so the recap is not left permanently empty.
        let owes_summary = !is_working
            && previous
                .as_ref()
                .is_none_or(|state| state.summary.is_empty());
        if content_unchanged && !is_working && !owes_idle_verdict && !owes_summary {
            return;
        }
        let generated = generate_agent_status(&self.agent_dir, &messages, is_working).await;
        // A failed classification on an idle session would sit unjudged
        // forever, so settle it to needs_input.
        let settled = match generated {
            Some(generated) => Some(StatusState {
                summary: generated.summary,
                task_state: generated.task_state,
                based_on_message_count: message_count,
            }),
            None if !is_working && (owes_idle_verdict || owes_summary) => Some(StatusState {
                summary: previous
                    .as_ref()
                    .map(|state| state.summary.clone())
                    .unwrap_or_default(),
                task_state: Some(AgentTaskState::NeedsInput),
                based_on_message_count: message_count,
            }),
            None => None,
        };
        let Some(mut status) = settled else {
            return;
        };
        // A working refresh carries no verdict; keep the prior one at the
        // same message count so a still-valid needs_input is not dropped.
        if status.task_state.is_none() {
            status.task_state = previous
                .as_ref()
                .filter(|state| state.based_on_message_count == message_count)
                .and_then(|state| state.task_state);
        }
        let changed = previous
            .as_ref()
            .map(|state| {
                state.summary != status.summary
                    || state.task_state != status.task_state
                    || (!is_working
                        && state.based_on_message_count != status.based_on_message_count)
            })
            .unwrap_or(true);
        *self.state.lock().expect("status state lock") = Some(status.clone());
        if changed {
            self.broadcast(&status.summary, &active_session_id, &generation);
        }
    }

    /// Sequence and broadcast one `session_status` outbound to the
    /// session's attached clients.
    fn broadcast(&self, recap: &str, active_session_id: &str, generation: &str) {
        let sequence = {
            let mut core = self.core.lock().expect("session core lock");
            core.status_next_sequence()
        };
        let meta = create_daemon_event_meta(active_session_id, sequence, None, Some(generation));
        let outbound = DaemonOutbound::SessionStatus {
            active_session_id: active_session_id.to_string(),
            recap: (!recap.is_empty()).then(|| recap.to_string()),
            meta: Some(meta),
            rest: Default::default(),
        };
        let payload = serde_json::to_vec(&outbound).unwrap_or_default();
        let _ = self
            .events
            .send(Arc::new(OutboundFrame::session_status(payload)));
    }
}

/// One generated status: the recap text plus the idle verdict (working
/// refreshes carry no verdict).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedStatus {
    pub summary: String,
    pub task_state: Option<AgentTaskState>,
}

/// Resolve the cheap summary model, or `None` when it has no configured
/// auth (the request is skipped, like the TS product).
pub fn resolve_summary_model(
    auth: pa_core::auth::AuthStorage,
    models_json: &Path,
) -> Option<Model> {
    let registry = pa_core::models::ModelRegistry::create(auth, models_json);
    registry
        .get_all()
        .iter()
        .find(|model| model.provider == SUMMARY_MODEL_PROVIDER && model.id == SUMMARY_MODEL_ID)
        .filter(|model| registry.has_configured_auth(model))
        .cloned()
}

/// One cheap model call for a fresh status, or `None` when unavailable or
/// failed. The request is the observable parity artifact: same model,
/// system prompt, conversation body, and token cap as the TS product.
pub async fn generate_agent_status(
    agent_dir: &Path,
    messages: &[Value],
    is_working: bool,
) -> Option<GeneratedStatus> {
    if messages.is_empty() {
        return None;
    }
    let auth = pa_core::auth::AuthStorage::create(agent_dir);
    let models_json = agent_dir.join("models.json");
    let model = resolve_summary_model(auth, &models_json)?;
    let request_auth = {
        let mut registry = pa_core::models::ModelRegistry::create(
            pa_core::auth::AuthStorage::create(agent_dir),
            &models_json,
        );
        registry.get_api_key_and_headers(&model, model.headers.as_ref())
    };
    if !request_auth.ok || request_auth.api_key.is_none() {
        return None;
    }
    let context = pa_types::ai::Context {
        system_prompt: Some(AGENT_STATUS_SYSTEM_PROMPT.to_string()),
        messages: vec![pa_types::ai::Message::User(pa_types::ai::UserMessage {
            content: pa_types::ai::UserContent::Text(build_status_context(messages, is_working)),
            timestamp: 0,
            rest: Default::default(),
        })],
        tools: None,
    };
    let stream_options =
        pa_ai::types::SimpleStreamOptions::from_base(pa_ai::types::StreamOptions {
            max_tokens: Some(SUMMARY_MAX_TOKENS),
            api_key: request_auth.api_key,
            headers: request_auth.headers,
            ..Default::default()
        });
    let response = pa_ai::complete_simple(&model, &context, Some(stream_options))
        .await
        .ok()?;
    if response.stop_reason == pa_types::ai::StopReason::Error {
        return None;
    }
    let text = response
        .content
        .iter()
        .filter_map(|block| match block {
            pa_types::ai::AssistantContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    parse_agent_status(&text, is_working).map(|status| GeneratedStatus {
        summary: status.summary,
        task_state: status.task_state,
    })
}

/// The text of one message: text blocks joined, tool calls by name only.
fn message_text(content: &Value) -> (String, Vec<String>) {
    match content {
        Value::String(text) => (text.clone(), Vec::new()),
        Value::Array(blocks) => {
            let mut parts: Vec<String> = Vec::new();
            let mut tools: Vec<String> = Vec::new();
            for block in blocks {
                let Some(object) = block.as_object() else {
                    continue;
                };
                match object.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = object.get("text").and_then(Value::as_str) {
                            parts.push(text.to_string());
                        }
                    }
                    Some("tool_use" | "toolUse") => {
                        if let Some(name) = object.get("name").and_then(Value::as_str) {
                            tools.push(name.to_string());
                        }
                    }
                    _ => {}
                }
            }
            (parts.join("\n"), tools)
        }
        _ => (String::new(), Vec::new()),
    }
}

/// Normalize whitespace and clamp with the TS ellipsis.
fn clamp(text: &str, max: usize) -> String {
    let normalized: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() > max {
        let cut: String = normalized.chars().take(max).collect();
        format!("{cut}\u{2026}")
    } else {
        normalized
    }
}

/// Serialize the trailing messages into a compact prompt body (tool calls by
/// name only).
pub fn build_status_context(messages: &[Value], is_working: bool) -> String {
    let recent = &messages[messages.len().saturating_sub(SUMMARY_CONTEXT_MESSAGES)..];
    let mut lines: Vec<String> = Vec::new();
    for message in recent {
        let Some(role) = message.get("role").and_then(Value::as_str) else {
            continue;
        };
        if !matches!(role, "user" | "assistant" | "toolResult" | "custom") {
            continue;
        }
        let (text, tools) = message.get("content").map(message_text).unwrap_or_default();
        let body = clamp(&text, SUMMARY_MAX_CHARS_PER_MESSAGE);
        let mut tools_seen: Vec<&str> = Vec::new();
        for tool in &tools {
            if !tools_seen.contains(&tool.as_str()) {
                tools_seen.push(tool.as_str());
            }
        }
        let tool_note = if tools_seen.is_empty() {
            String::new()
        } else {
            format!("[tools: {}]", tools_seen.join(", "))
        };
        let rendered = [body, tool_note]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !rendered.is_empty() {
            lines.push(format!("{role}: {rendered}"));
        }
    }
    let state = if is_working {
        "working"
    } else {
        "idle (finished its turn)"
    };
    format!(
        "<agent-state>{state}</agent-state>\n<conversation>\n{}\n</conversation>",
        lines.join("\n")
    )
}

const MAX_RECAP_WORDS: usize = 16;

/// Strip a word-counting trailer the model sometimes appends; kept to
/// structural counting markers so plain words survive.
fn reasoning_trailer() -> Regex {
    // TS REASONING_TRAILER:
    // \s*(?:["”]\s*)?(?:\bthat['’]?s\s+\d+\s+words?\b|\bcount\s*:|\(\d+\)|=\s*\d+\s+words?\b).*
    Regex::new(r#"(?i)\s*(?:["”]\s*)?(?:\bthat['’]?s\s+\d+\s+words?\b|\bcount\s*:|\(\d+\)|=\s*\d+\s+words?\b).*"#)
        .expect("valid recap trailer regex")
}

/// Counting artifact markers that reject a recap outright.
fn counting_artifact() -> Regex {
    // TS COUNTING_ARTIFACT: \(\d+\)|=\s*\d+\s+words?\b
    Regex::new(r#"(?i)\(\d+\)|=\s*\d+\s+words?\b"#).expect("valid counting regex")
}

/// Take the content of the last `<recap>` and `<status>` tags; idle verdicts
/// default to needs_input.
pub fn parse_agent_status(text: &str, is_working: bool) -> Option<GeneratedStatus> {
    // Normalize unicode angle-bracket lookalikes so a tag written with them
    // still parses.
    let cleaned = text
        .replace(['\u{2039}', '\u{FF1C}'], "<")
        .replace(['\u{203A}', '\u{FF1E}'], ">");
    let recap = last_tag_content(&cleaned, "recap")?;
    let summary = clean_recap(&recap)?;
    if is_working {
        return Some(GeneratedStatus {
            summary,
            task_state: None,
        });
    }
    let status = last_tag_content(&cleaned, "status");
    let task_state = match status.map(|status| status.trim().to_lowercase()).as_deref() {
        Some("completed") => Some(AgentTaskState::Completed),
        // Idle verdicts default to needs_input.
        _ => Some(AgentTaskState::NeedsInput),
    };
    Some(GeneratedStatus {
        summary,
        task_state,
    })
}

/// The content of the last `<tag>...</tag>` pair, case-insensitive.
fn last_tag_content(text: &str, tag: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut content = None;
    let mut search = 0usize;
    while let Some(start) = lower[search..].find(&open) {
        let content_start = search + start + open.len();
        let Some(end) = lower[content_start..].find(&close) else {
            break;
        };
        content = Some(text[content_start..content_start + end].to_string());
        search = content_start + end + close.len();
    }
    content
}

/// Strip quotes and trailing punctuation; reject prompt echoes and counting
/// artifacts (TS `cleanRecap`).
fn clean_recap(raw: &str) -> Option<String> {
    let value = reasoning_trailer().replace(raw.trim(), "");
    let value = value.trim();
    let value = value
        .trim_start_matches(['"', '\u{201C}', '\''])
        .trim_end_matches(['"', '\u{201D}', '\'']);
    let value = value.trim_end_matches(['.', ' ']).trim();
    if value.is_empty() || value.starts_with('<') {
        return None;
    }
    let lower = value.to_lowercase();
    if lower.contains("present-tense") || lower.contains("12 words") {
        return None;
    }
    if counting_artifact().is_match(value) {
        return None;
    }
    if value.split_whitespace().count() > MAX_RECAP_WORDS {
        return None;
    }
    Some(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages() -> Vec<Value> {
        serde_json::json!([
            { "role": "user", "content": [{ "type": "text", "text": "please add tests" }] },
            { "role": "assistant", "content": [
                { "type": "text", "text": "running the suite now" },
                { "type": "tool_use", "name": "ipython", "id": "t1" },
            ] },
            { "role": "toolResult", "content": [{ "type": "text", "text": "all green" }] },
        ])
        .as_array()
        .cloned()
        .unwrap_or_default()
    }

    #[test]
    fn context_body_matches_the_ts_shape() {
        let body = build_status_context(&messages(), false);
        assert_eq!(
            body,
            "<agent-state>idle (finished its turn)</agent-state>\n<conversation>\n\
             user: please add tests\n\
             assistant: running the suite now [tools: ipython]\n\
             toolResult: all green\n</conversation>"
        );
        assert!(build_status_context(&messages(), true)
            .starts_with("<agent-state>working</agent-state>"));
    }

    #[test]
    fn context_takes_the_trailing_messages_only() {
        let mut many = Vec::new();
        for i in 0..12 {
            many.push(serde_json::json!({
                "role": "user",
                "content": [{ "type": "text", "text": format!("message {i}") }],
            }));
        }
        let body = build_status_context(&many, false);
        assert!(body.contains("message 4"));
        assert!(body.contains("message 11"));
        assert!(!body.contains("message 3\n"));
    }

    #[test]
    fn parse_takes_the_last_tags_and_defaults_idle_to_needs_input() {
        let parsed = parse_agent_status(
            "<recap>First</recap><status>COMPLETED</status><recap>Fixing the leak</recap>",
            false,
        )
        .expect("parsed");
        // The last recap wins; the only status tag wins.
        assert_eq!(parsed.summary, "Fixing the leak");
        assert_eq!(parsed.task_state, Some(AgentTaskState::Completed));
        let parsed = parse_agent_status(
            "<recap>queued</recap><status>COMPLETED</status><status>NEEDS_INPUT</status>",
            false,
        )
        .expect("parsed");
        assert_eq!(parsed.task_state, Some(AgentTaskState::NeedsInput));
        let parsed = parse_agent_status(
            "<recap>Refactoring the auth middleware</recap><status>COMPLETED</status>",
            false,
        )
        .expect("parsed");
        assert_eq!(parsed.task_state, Some(AgentTaskState::Completed));
        // A working refresh carries no verdict.
        let parsed = parse_agent_status("<recap>running tests</recap>", true).expect("parsed");
        assert_eq!(parsed.task_state, None);
        // Normalized angle-bracket lookalikes still parse.
        let parsed = parse_agent_status(
            "\u{FF1C}recap\u{FF1E}queued the build\u{FF1C}/recap\u{FF1E}",
            true,
        )
        .expect("parsed");
        assert_eq!(parsed.summary, "queued the build");
    }

    #[test]
    fn parse_rejects_prompt_echoes_and_counting_artifacts() {
        assert!(parse_agent_status("<recap></recap>", false).is_none());
        assert!(parse_agent_status("<recap>a present-tense clause</recap>", false).is_none());
        // The counting trailer is cut, and the surviving recap is kept.
        let parsed = parse_agent_status("<recap>Waiting for CI (3) = 4 words?</recap>", false)
            .expect("parsed");
        assert_eq!(parsed.summary, "Waiting for CI");
        assert!(
            parse_agent_status("<recap>Sending the patch. That's 5 words</recap>", false,)
                .is_some()
        );
        let long: Vec<String> = (0..20).map(|i| format!("word{i}")).collect();
        assert!(parse_agent_status(&format!("<recap>{}</recap>", long.join(" ")), false).is_none());
    }
}
