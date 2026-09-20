//! Harness digest delivery: compose the continual-harness state into the
//! model-facing `[harness-digest]` context message and deliver it at cold
//! context boundaries (session start, resume, compaction head). Port of the
//! `_harnessDigest` half of core/agent-session.ts over
//! `format_harness_state_for_prompt`.

use std::path::PathBuf;

use pa_agent::types::{AgentMessage, Message, TextContent, UserContent, UserMessage, UserPart};
use pa_types::session::{AgentMessage as SessionAgentMessage, FileEntry};

use crate::refinement::ranking::{
    format_harness_state_for_prompt, harness_query_terms, HarnessQueryTerms,
    HarnessStatePromptOptions,
};
use crate::refinement::{load_harness_state, merge_harness_states, HarnessScope};

use super::messages::{HARNESS_DIGEST_PREFIX, HARNESS_DIGEST_SUFFIX};

/// Session-scoped digest inputs: where harness state lives and which
/// interfaces the digest may reference.
#[derive(Debug, Clone)]
pub struct HarnessDigestContext {
    /// Global harness state directory (`<agent dir>/harness`).
    pub global_dir: PathBuf,
    /// Session-local harness state directory (session artifact dir), when the
    /// session persists artifacts.
    pub local_dir: Option<PathBuf>,
    /// The session exposes the Python REPL (`ipython` tool active).
    pub include_ipython: bool,
    /// The session exposes `bash` as a model tool.
    pub include_shell_examples: bool,
    /// The `refine` skill is visible to the model.
    pub include_refine: bool,
}

/// Relevance terms for digest entry ranking: the active goal objective
/// (strongest) plus the last few user/assistant texts, newest first.
pub fn digest_query_terms(
    goal_objective: Option<&str>,
    recent_texts_newest_first: &[String],
) -> HarnessQueryTerms {
    let mut terms: HarnessQueryTerms = std::collections::HashMap::new();
    fn add_text(terms: &mut HarnessQueryTerms, text: &str, weight: f64) {
        for raw in harness_query_terms(text) {
            if terms.len() >= 48 && !terms.contains_key(&raw) {
                return;
            }
            terms.entry(raw).or_insert(weight);
        }
    }
    add_text(&mut terms, goal_objective.unwrap_or_default(), 3.0);
    let mut recency_weight = 2.0;
    for text in recent_texts_newest_first.iter().take(4) {
        add_text(&mut terms, text, recency_weight);
        recency_weight = (recency_weight - 0.5).max(1.0);
    }
    terms
}

/// The rendered digest body (the `<harness_state>` content): merged global +
/// local harness state, ranked by the query terms.
pub fn harness_digest_text(
    context: &HarnessDigestContext,
    query_terms: HarnessQueryTerms,
) -> String {
    let global = load_harness_state(&context.global_dir, HarnessScope::Global);
    let local = context
        .local_dir
        .as_ref()
        .map(|dir| load_harness_state(dir, HarnessScope::Local));
    let merged = merge_harness_states(&global, local.as_ref());
    format_harness_state_for_prompt(
        &merged,
        &HarnessStatePromptOptions {
            include_ipython_examples: Some(context.include_ipython),
            include_shell_examples: context.include_shell_examples,
            // TS: includeRefineExamples = hasIpython && hasRefineSkill.
            include_refine_examples: Some(context.include_ipython && context.include_refine),
            query_terms: Some(query_terms),
            ..Default::default()
        },
    )
}

/// Digest inputs captured from the live session (interface flags plus
/// relevance terms); the merged harness-state disk read happens when the
/// digest is rendered, so a render at the compaction commit is a fresh
/// read of harness state written mid-run (TS `_harnessDigest` at the
/// `appendCompaction` call site).
#[derive(Debug, Clone)]
pub struct HarnessDigestInputs {
    pub context: HarnessDigestContext,
    pub terms: HarnessQueryTerms,
}

impl HarnessDigestInputs {
    /// Render the digest body (the `<harness_state>` content).
    pub fn render(&self) -> String {
        harness_digest_text(&self.context, self.terms.clone())
    }
}

/// Full digest message text (prefix + state + suffix).
pub fn harness_digest_message_text(digest: &str) -> String {
    format!("{HARNESS_DIGEST_PREFIX}{digest}{HARNESS_DIGEST_SUFFIX}")
}

/// The digest as a loop message: a user turn whose text is the framed digest
/// (session custom messages enter LLM context as user turns).
pub fn harness_digest_loop_message(digest: &str, timestamp: i64) -> AgentMessage {
    AgentMessage::Standard(Message::User(digest_user_message(digest, timestamp)))
}

/// The digest as a session message payload for persistence
/// (`append_custom_message` with `display: false` and the digest in details).
pub fn persist_digest(
    session: &mut crate::session::manager::SessionManager,
    digest: &str,
) -> String {
    session.append_custom_message(
        super::headless::HARNESS_DIGEST_CUSTOM_TYPE,
        pa_types::ai::UserContent::Text(harness_digest_message_text(digest)),
        false,
        Some(serde_json::json!({ "digest": digest })),
    )
}

fn digest_user_message(digest: &str, timestamp: i64) -> UserMessage {
    UserMessage {
        content: UserContent::Parts(vec![UserPart::Text(TextContent {
            text: harness_digest_message_text(digest),
            text_signature: None,
        })]),
        timestamp,
    }
}

/// The raw digest carried by one loop-context user row, when it carries the
/// digest frame (standalone digest rows and the digest block that leads a
/// compaction-summary row).
fn digest_from_frame(text: &str) -> Option<&str> {
    let after_prefix = text
        .strip_prefix(super::messages::HARNESS_DIGEST_PREFIX)
        .or_else(|| {
            text.find(super::messages::HARNESS_DIGEST_PREFIX)
                .map(|at| &text[at + super::messages::HARNESS_DIGEST_PREFIX.len()..])
        })?;
    let end = after_prefix
        .find(super::messages::HARNESS_DIGEST_SUFFIX)
        .map(|at| &after_prefix[..at])?;
    Some(end.trim_end_matches('\n'))
}

/// The newest digest recorded in the live loop context (TS
/// `_latestContextHarnessDigest`): digest rows and compaction-summary digest
/// blocks, both of which are user turns after context conversion. Recency is
/// by timestamp, not position - retained pre-compaction rows follow the
/// compaction head, and out-of-context file entries must never suppress a
/// cold-boundary delivery.
pub fn latest_context_digest(messages: &[AgentMessage]) -> Option<String> {
    let mut latest: Option<(i64, &str)> = None;
    for message in messages {
        let AgentMessage::Standard(Message::User(user)) = message else {
            continue;
        };
        let text = match &user.content {
            UserContent::Text(text) => text.as_str(),
            UserContent::Parts(parts) => parts
                .iter()
                .find_map(|part| match part {
                    UserPart::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .unwrap_or(""),
        };
        let Some(digest) = digest_from_frame(text) else {
            continue;
        };
        if latest.is_none_or(|(timestamp, _)| user.timestamp > timestamp) {
            latest = Some((user.timestamp, digest));
        }
    }
    latest.map(|(_, digest)| digest.to_string())
}

/// Session-local harness state directory implied by a conversation-log path
/// (`dirname(dirname(file))/session-artifacts/<id>`, TS
/// `getSessionArtifactPathForFile`); used when the caller owns persistence.
pub fn local_harness_dir_for_log(log: &std::path::Path) -> Option<PathBuf> {
    let id = log.file_stem()?.to_string_lossy().to_string();
    let artifacts_root = log.parent()?.parent()?.join("session-artifacts");
    Some(
        artifacts_root
            .join(id)
            .join(crate::refinement::HARNESS_STATE_DIR_NAME),
    )
}

/// Session message view of the digest entries (resume context rebuild).
pub fn digest_session_message(entry: &FileEntry) -> Option<SessionAgentMessage> {
    let FileEntry::CustomMessage { payload, .. } = entry else {
        return None;
    };
    if payload.custom_type != super::headless::HARNESS_DIGEST_CUSTOM_TYPE {
        return None;
    }
    Some(SessionAgentMessage::Custom(
        pa_types::session::CustomMessage {
            custom_type: payload.custom_type.clone(),
            content: payload.content.clone(),
            display: payload.display,
            details: payload.details.clone(),
            timestamp: crate::session::timestamp_to_millis(entry.timestamp()),
            rest: Default::default(),
        },
    ))
}

// Delivery mechanics live with the digest composition (cold-boundary
// delivery is one invariant, TS `_ensureHarnessDigestContext` /
// `_appendHarnessDigestIfStale`): the placement enum plus the
// `AgentSession` methods that drive it. Private `AgentSession` fields are
// reachable from this child module.
/// Where a delivered digest message lands in the loop context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DigestPlacement {
    /// Resume/context-rebuild boundaries: the digest follows the context.
    Append,
    /// The deferred first-turn digest leads the (empty or rolled-back)
    /// context so it precedes the first user prompt in the request.
    PrependToEmptyContext,
}

impl super::AgentSession {
    /// Cold-boundary digest delivery (session start / resume): empty contexts
    /// defer to the first committed turn; non-empty contexts append only when
    /// the newest in-context digest is stale against disk.
    pub(crate) async fn ensure_harness_digest_context(&self) -> anyhow::Result<()> {
        let state = self.agent.state().await;
        let empty = state.messages.is_empty();
        drop(state);
        if empty {
            self.digest_pending
                .store(true, std::sync::atomic::Ordering::SeqCst);
        } else {
            self.append_stale_harness_digest(DigestPlacement::Append)
                .await?;
        }
        Ok(())
    }

    /// Deliver the pending first-turn digest before the prompt rides the turn.
    pub(crate) async fn deliver_pending_harness_digest(&self) -> anyhow::Result<()> {
        if !self
            .digest_pending
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(());
        }
        self.append_stale_harness_digest(DigestPlacement::PrependToEmptyContext)
            .await
    }

    /// The digest inputs captured from the live session (TS `_harnessDigest`
    /// sources): the interface flags plus relevance terms from the goal
    /// objective and the last few user/assistant texts. `None` when the
    /// session carries no harness state. The harness-state disk read is
    /// deferred to render time so a snapshot taken before a long-running
    /// operation (the compaction summarizer) still reads fresh state at
    /// its commit.
    pub(crate) async fn harness_digest_inputs(&self) -> Option<HarnessDigestInputs> {
        let context = self.harness_digest.clone()?;
        let recent_texts = self.recent_message_texts_newest_first().await;
        let goal = {
            let session = self.session.lock().await;
            super::goal_driver::GoalDriver::load_persisted(&session)
                .state()
                .objective
                .clone()
        };
        let terms = digest_query_terms(goal.as_deref(), &recent_texts);
        Some(HarnessDigestInputs { context, terms })
    }

    /// Compute the digest and, when it differs from the newest in-context
    /// digest, persist it and place the message in the loop context.
    async fn append_stale_harness_digest(&self, placement: DigestPlacement) -> anyhow::Result<()> {
        let Some(inputs) = self.harness_digest_inputs().await else {
            return Ok(());
        };
        let digest = inputs.render();
        // Staleness is against the live loop context only (TS
        // `_latestContextHarnessDigest`): pruned file entries are not
        // in-context digests and must not suppress delivery.
        let latest = latest_context_digest(&self.agent.state().await.messages);
        if latest.as_deref() == Some(digest.as_str()) {
            return Ok(());
        }
        let message = harness_digest_loop_message(&digest, super::now_millis() as i64);
        let mut messages = self.agent.state().await.messages;
        match placement {
            DigestPlacement::Append => messages.push(message),
            // A cancelled first turn can leave non-empty context; the digest
            // still leads whatever is present.
            DigestPlacement::PrependToEmptyContext => messages.insert(0, message),
        }
        self.agent.set_messages(messages).await;
        let mut session = self.session.lock().await;
        persist_digest(&mut session, &digest);
        Ok(())
    }

    /// The last four user/assistant texts, newest first (digest ranking).
    async fn recent_message_texts_newest_first(&self) -> Vec<String> {
        use pa_agent::types::{AgentMessage, AssistantContent, Message};
        let state = self.agent.state().await;
        let mut texts: Vec<String> = state
            .messages
            .iter()
            .filter_map(|message| match message {
                AgentMessage::Standard(Message::User(user)) => Some(loop_user_text(&user.content)),
                AgentMessage::Standard(Message::Assistant(assistant)) => {
                    let text = assistant
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            AssistantContent::Text(text) => Some(text.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    (!text.is_empty()).then_some(text)
                }
                _ => None,
            })
            .collect();
        texts.truncate(4);
        texts.reverse();
        texts
    }
}

/// The text of a loop user message (string content or joined text parts).
fn loop_user_text(content: &pa_agent::types::UserContent) -> String {
    match content {
        pa_agent::types::UserContent::Text(text) => text.clone(),
        pa_agent::types::UserContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                pa_agent::types::UserPart::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::manager::SessionManager;

    #[test]
    fn empty_state_digest_renders_placeholder() {
        let tmp = tempfile::tempdir().unwrap();
        let context = HarnessDigestContext {
            global_dir: tmp.path().join("harness"),
            local_dir: None,
            include_ipython: true,
            include_shell_examples: false,
            include_refine: true,
        };
        let digest = harness_digest_text(&context, HarnessQueryTerms::default());
        assert!(digest.starts_with("# Continual Harness State"));
        assert!(digest.contains("No saved harness entries yet."));
        assert!(digest.contains("recent refinements: 0"));
        assert!(digest.contains("When to call `await refine.run()`"));
        // The framed message text wraps the state block.
        let message = harness_digest_message_text(&digest);
        assert!(message.starts_with("[harness-digest]"));
        assert!(message.ends_with("</harness_state>"));
    }

    #[test]
    fn query_terms_weight_goal_and_recency() {
        let terms = digest_query_terms(
            Some("fix the worktree parity battery"),
            &["continue the parity work".to_string()],
        );
        assert_eq!(terms.get("worktree"), Some(&3.0));
        assert_eq!(terms.get("parity"), Some(&3.0));
        assert_eq!(terms.get("continue"), Some(&2.0));
    }

    #[test]
    fn persisted_digest_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = SessionManager::persisted(tmp.path(), &tmp.path().join("sessions"));
        let id = persist_digest(&mut session, "digest body");
        let _ = id;
        let entries = session.get_all_entries().to_vec();
        let FileEntry::CustomMessage { payload, .. } = &entries[1] else {
            panic!("expected digest entry");
        };
        assert_eq!(
            payload
                .details
                .as_ref()
                .and_then(|details| details.get("digest"))
                .and_then(serde_json::Value::as_str),
            Some("digest body")
        );
        let message = digest_session_message(&entries[1]).expect("digest entry");
        let SessionAgentMessage::Custom(custom) = &message else {
            panic!("expected custom message");
        };
        assert_eq!(custom.custom_type, "harness_digest");
        assert!(!custom.display);
        assert!(matches!(custom.content, pa_types::ai::UserContent::Text(_)));
        // The loop message is a user turn carrying the framed text.
        let loop_message = harness_digest_loop_message("digest body", 0);
        let AgentMessage::Standard(Message::User(user)) = &loop_message else {
            panic!("expected user message");
        };
        match &user.content {
            UserContent::Text(text) => assert!(text.contains("[harness-digest]")),
            UserContent::Parts(parts) => assert!(parts.iter().any(
                |part| matches!(part, UserPart::Text(text) if text.text.contains("[harness-digest]"))
            )),
        }
        // The loop-context staleness view reads the digest out of the frame,
        // so a delivered digest row suppresses re-delivery while it is the
        // newest (TS `_latestContextHarnessDigest`).
        let mut context = vec![loop_message.clone()];
        assert_eq!(
            latest_context_digest(&context).as_deref(),
            Some("digest body")
        );
        let mut later = harness_digest_loop_message("newer digest", 2);
        if let AgentMessage::Standard(Message::User(user)) = &mut later {
            user.timestamp = 2;
        }
        context.push(later);
        assert_eq!(
            latest_context_digest(&context).as_deref(),
            Some("newer digest")
        );
        // A context with no digest rows never suppresses delivery.
        assert_eq!(latest_context_digest(&[]), None);
    }

    #[test]
    fn out_of_context_file_entries_never_count_as_context_digests() {
        // A compaction-summary user row carries its digest block first; the
        // frame reader extracts that digest, and a compaction row without a
        // digest block contributes nothing.
        let summary = harness_digest_loop_message("compaction head digest", 5);
        let mut wrapped = match summary {
            AgentMessage::Standard(Message::User(mut user)) => {
                user.content = UserContent::Text(format!(
                    "{HARNESS_DIGEST_PREFIX}compaction head digest{HARNESS_DIGEST_SUFFIX}\n\n[compaction] summary text"
                ));
                AgentMessage::Standard(Message::User(user))
            }
            other => other,
        };
        if let AgentMessage::Standard(Message::User(user)) = &mut wrapped {
            user.timestamp = 5;
        }
        assert_eq!(
            latest_context_digest(&[wrapped]).as_deref(),
            Some("compaction head digest")
        );
    }
}
