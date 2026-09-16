//! Session-level /refine: message builders, history merge, and the
//! plan -> re-read -> apply -> persist flow. Port of the refine plumbing in
//! core/agent-session.ts (_planRefine/_applyRefine) plus the message builders
//! in core/messages.ts.

use std::path::{Path, PathBuf};

use pa_types::ai::{UserContent, UserMessage};
use pa_types::session::{AgentMessage, CustomMessage, FileEntry};
use serde_json::json;

use crate::refinement::executor::{
    apply_refinement_plan, plan_refinement, RefineOptions as CoreRefineOptions, RefinementPlan,
};
use crate::refinement::{
    append_global_refinement, format_refinement_notice_body, load_global_refinement_history,
    load_harness_state, merge_harness_states, save_harness_state, HarnessScope, RefinementResult,
};
use crate::session::manager::SessionManager;

/// Audit entry type recording each applied refinement in the session JSONL.
pub const REFINEMENT_AUDIT_CUSTOM_TYPE: &str = "prime-agent.refinement";
/// TUI-rendered outcome message custom type.
pub const REFINEMENT_OUTCOME_CUSTOM_TYPE: &str = "refinement_outcome";
/// Model-facing notice custom type (display=false; passes convertToLlm).
pub const REFINEMENT_NOTICE_CUSTOM_TYPE: &str = "refinement_notice";

/// Who triggered a refinement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefinementSource {
    Auto,
    User,
    SelfRefine,
}

impl RefinementSource {
    fn as_str(&self) -> &'static str {
        match self {
            RefinementSource::Auto => "auto",
            RefinementSource::User => "user",
            RefinementSource::SelfRefine => "self",
        }
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// TUI-rendered outcome message (`refinement_outcome`).
pub fn create_refinement_outcome_message(result: &RefinementResult) -> CustomMessage {
    let mut details = json!({
        "refinementId": result.id,
        "summary": result.summary,
        "scope": result.scope.unwrap_or(HarnessScope::Local),
        "edits": result.applied_edits,
    });
    if let (Some(rollback), Some(map)) = (result.rollback_of.clone(), details.as_object_mut()) {
        map.insert("rollbackOf".to_string(), json!(rollback));
    }
    CustomMessage {
        custom_type: REFINEMENT_OUTCOME_CUSTOM_TYPE.to_string(),
        content: UserContent::Text(format!("Refinement complete: {}", result.summary)),
        display: true,
        details: Some(details),
        timestamp: now_millis(),
        rest: Default::default(),
    }
}

/// Model-facing notice (`refinement_notice`, display=false).
pub fn create_refinement_notice_message(
    result: &RefinementResult,
    source: RefinementSource,
) -> CustomMessage {
    let mut details = json!({
        "refinementId": result.id,
        "summary": result.summary,
        "scope": result.scope.unwrap_or(HarnessScope::Local),
        "edits": result.applied_edits,
        "source": source.as_str(),
    });
    if let (Some(rollback), Some(map)) = (result.rollback_of.clone(), details.as_object_mut()) {
        map.insert("rollbackOf".to_string(), json!(rollback));
    }
    CustomMessage {
        custom_type: REFINEMENT_NOTICE_CUSTOM_TYPE.to_string(),
        content: UserContent::Text(format!(
            "[{}-refinement]\n\n{}",
            source.as_str(),
            format_refinement_notice_body(result)
        )),
        display: false,
        details: Some(details),
        timestamp: now_millis(),
        rest: Default::default(),
    }
}

/// Refinement history recorded in this session's JSONL entries.
pub fn session_refinement_history(entries: &[FileEntry]) -> Vec<RefinementResult> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::Custom { payload, .. }
                if payload.custom_type == REFINEMENT_AUDIT_CUSTOM_TYPE =>
            {
                payload
                    .data
                    .as_ref()
                    .and_then(|data| serde_json::from_value::<RefinementResult>(data.clone()).ok())
            }
            _ => None,
        })
        .collect()
}

/// Merged cross-session + in-session refinement history.
pub fn load_refinement_history(
    session: &SessionManager,
    global_harness_dir: &Path,
) -> Vec<RefinementResult> {
    let global = load_global_refinement_history(global_harness_dir);
    let session_entries = session_refinement_history(session.get_all_entries());
    crate::refinement::merge_refinement_history(&global, &session_entries)
}

/// The session's local harness state directory (under the session dir).
pub fn local_harness_state_dir(session: &SessionManager) -> PathBuf {
    let session_dir = session.get_session_dir().to_path_buf();
    crate::refinement::get_local_harness_state_dir(Some(&session_dir))
        .expect("session dir always yields a local harness dir")
}

/// Strip display-only `local:`/`global:` prefixes from proposal edit ids.
fn strip_display_prefixes(plan: RefinementPlan) -> RefinementPlan {
    let mut plan = plan;
    for edit in &mut plan.proposal.edits {
        if let Some(id) = &edit.id {
            if let Some(stripped) = id
                .strip_prefix("local:")
                .or_else(|| id.strip_prefix("global:"))
            {
                edit.id = Some(stripped.to_string());
            }
        }
    }
    plan
}

/// Run the full refinement flow: plan (LLM or rollback), re-read the target
/// store, apply, persist state + history, and append the audit, outcome, and
/// notice entries to the session. `refine_call` performs the model request.
pub async fn execute_refinement(
    session: &mut SessionManager,
    messages: &[AgentMessage],
    global_harness_dir: &Path,
    model: &pa_types::ai::Model,
    options: &RefineOptions,
    source: RefinementSource,
    refine_call: crate::refinement::executor::RefinerFn,
) -> anyhow::Result<RefinementResult> {
    let local_harness_dir = local_harness_state_dir(session);
    let core_options = CoreRefineOptions {
        global: options.global,
        instructions: options.instructions.clone(),
        rollback_id: options.rollback_id.clone(),
    };
    let requested_scope = if options.global {
        HarnessScope::Global
    } else {
        HarnessScope::Local
    };
    if options.rollback_id.is_none()
        && requested_scope == HarnessScope::Local
        && !session.is_persisted()
    {
        anyhow::bail!(
            "Local harness refinement requires a persisted session; use global refinement instead."
        );
    }
    // Planning state: global, or merged global+local for local refinements.
    let global_state = load_harness_state(global_harness_dir, HarnessScope::Global);
    let planning_state = if requested_scope == HarnessScope::Global {
        global_state.clone()
    } else {
        let local_state = load_harness_state(&local_harness_dir, HarnessScope::Local);
        merge_harness_states(&global_state, Some(&local_state))
    };
    let history = load_refinement_history(session, global_harness_dir);
    // Baseline captured before the (slow) LLM pass, so concurrent kernel
    // writes are rejected instead of clobbered.
    let baseline_scope = options
        .rollback_id
        .as_ref()
        .and_then(|id| history.iter().find(|item| &item.id == id))
        .and_then(crate::refinement::infer_refinement_result_scope)
        .unwrap_or(requested_scope);
    let baseline_dir = match baseline_scope {
        HarnessScope::Global => global_harness_dir.to_path_buf(),
        HarnessScope::Local => local_harness_dir.clone(),
    };
    let baseline_state = load_harness_state(&baseline_dir, baseline_scope);

    let mut plan = plan_refinement(
        messages,
        &planning_state,
        &history,
        model,
        &core_options,
        refine_call,
    )
    .await?;
    plan = strip_display_prefixes(plan);

    // Synchronous application phase: re-read the target store, apply, persist.
    let target_scope = plan.rollback_scope.unwrap_or(requested_scope);
    let target_dir = match target_scope {
        HarnessScope::Global => global_harness_dir.to_path_buf(),
        HarnessScope::Local => local_harness_dir.clone(),
    };
    let mut state = load_harness_state(&target_dir, target_scope);
    let mut result = apply_refinement_plan(&mut state, plan, &core_options, Some(baseline_state));
    result.harness_state_path = save_harness_state(&target_dir, &state)?
        .to_string_lossy()
        .to_string();
    if target_scope == HarnessScope::Global {
        append_global_refinement(global_harness_dir, &result)?;
    }
    // Audit entry first; failures there abort the whole refinement.
    session.append_custom_entry(
        REFINEMENT_AUDIT_CUSTOM_TYPE,
        Some(serde_json::to_value(&result)?),
    );
    // Outcome for the TUI; notice for the model (only when edits applied).
    let outcome = create_refinement_outcome_message(&result);
    session.append_custom_message(
        &outcome.custom_type,
        outcome.content.clone(),
        outcome.display,
        outcome.details.clone(),
    );
    if result.applied_edits.iter().any(|edit| edit.applied) {
        let notice = create_refinement_notice_message(&result, source);
        session.append_custom_message(
            &notice.custom_type,
            notice.content.clone(),
            notice.display,
            notice.details.clone(),
        );
    }
    Ok(result)
}

/// `/refine` request options (session layer).
#[derive(Debug, Default, Clone)]
pub struct RefineOptions {
    pub global: bool,
    pub instructions: Option<String>,
    pub rollback_id: Option<String>,
}

/// The default model seam over pa-ai completion.
pub fn default_refiner_call(api_key: Option<String>) -> crate::refinement::executor::RefinerFn {
    Box::new(move |model, prompt| {
        let api_key = api_key.clone();
        Box::pin(async move {
            let context = pa_types::ai::Context {
                system_prompt: Some(
                    crate::refinement::planner::REFINEMENT_SYSTEM_PROMPT.to_string(),
                ),
                messages: vec![pa_types::ai::Message::User(UserMessage {
                    content: UserContent::Text(prompt),
                    timestamp: 0,
                    rest: Default::default(),
                })],
                tools: None,
            };
            let stream_options =
                pa_ai::types::SimpleStreamOptions::from_base(pa_ai::types::StreamOptions {
                    api_key,
                    ..Default::default()
                });
            Ok(pa_ai::complete_simple(&model, &context, Some(stream_options)).await?)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refinement::executor::RefinerFn;
    use pa_types::ai::{AssistantContentBlock, AssistantMessage, Model, StopReason, TextContent};
    use tempfile::TempDir;

    fn text_assistant(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![AssistantContentBlock::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
                rest: Default::default(),
            })],
            api: "openai-completions".to_string(),
            provider: "test".to_string(),
            model: "m".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Default::default(),
            stop_reason: StopReason::Stop,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 0,
            rest: Default::default(),
        }
    }

    fn seam(text: &str) -> RefinerFn {
        let text = text.to_string();
        Box::new(move |_model, _prompt| {
            let text = text.clone();
            Box::pin(async move { Ok(text_assistant(&text)) })
        })
    }

    fn test_model() -> Model {
        Model {
            id: "test".to_string(),
            name: "test".to_string(),
            api: "openai-completions".to_string(),
            provider: "test".to_string(),
            base_url: "https://example.invalid".to_string(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: pa_types::ai::ModelCost {
                input: 0.0.into(),
                output: 0.0.into(),
                cache_read: 0.0.into(),
                cache_write: 0.0.into(),
            },
            context_window: 100_000,
            max_tokens: 8_000,
            featured: None,
            headers: None,
            compat: None,
        }
    }

    fn persisted_session(dir: &TempDir) -> SessionManager {
        let session_dir = dir.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        let mut session = SessionManager::in_memory(dir.path());
        session.materialize_session_file(Some(session_dir));
        session
    }

    fn user_message(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
            rest: Default::default(),
        })
    }

    #[test]
    fn refinement_messages_match_wire_shape() {
        let result = RefinementResult {
            id: "refine_1".to_string(),
            summary: "add memory".to_string(),
            rationale: "seen twice".to_string(),
            expected_outcome: "recall".to_string(),
            applied_edits: vec![],
            harness_state_path: String::new(),
            rollback_of: None,
            scope: Some(HarnessScope::Local),
        };
        let outcome = create_refinement_outcome_message(&result);
        assert_eq!(outcome.custom_type, "refinement_outcome");
        assert_eq!(
            outcome.content,
            UserContent::Text("Refinement complete: add memory".to_string())
        );
        assert!(outcome.display);
        let notice = create_refinement_notice_message(&result, RefinementSource::User);
        assert_eq!(notice.custom_type, "refinement_notice");
        assert!(!notice.display);
        assert_eq!(
            notice.content,
            UserContent::Text("[user-refinement]\n\nadd memory".to_string())
        );
    }

    #[tokio::test]
    async fn execute_refinement_persists_state_and_entries() {
        let dir = TempDir::new().unwrap();
        let mut session = persisted_session(&dir);
        session.append_message(user_message("do a thing twice"));
        let global_dir = dir.path().join("harness");
        let reply = r#"{"summary":"note it","rationale":"repeated","expectedOutcome":"recall","edits":[{"action":"create","kind":"memory","id":"m1","title":"Tactic","content":"Use tactic A"}]}"#;
        let result = execute_refinement(
            &mut session,
            &[user_message("do a thing twice")],
            &global_dir,
            &test_model(),
            &RefineOptions::default(),
            RefinementSource::User,
            seam(reply),
        )
        .await
        .unwrap();
        assert_eq!(result.applied_edits.len(), 1);
        assert!(result.applied_edits[0].applied);
        // State written to the session-local harness store.
        let state_path = Path::new(&result.harness_state_path);
        assert!(state_path.exists());
        let harness_dir =
            crate::refinement::get_local_harness_state_dir(Some(session.get_session_dir()))
                .unwrap();
        let state = load_harness_state(&harness_dir, HarnessScope::Local);
        assert!(state.entries[&crate::refinement::RefinementKind::Memory].contains_key("m1"));
        // Audit + outcome + notice entries appended.
        let entries = session.get_all_entries().to_vec();
        assert_eq!(session_refinement_history(&entries).len(), 1);
        let custom_messages: Vec<&FileEntry> = entries
            .iter()
            .filter(|entry| {
                matches!(entry, FileEntry::CustomMessage { payload, .. }
                    if payload.custom_type == REFINEMENT_OUTCOME_CUSTOM_TYPE
                        || payload.custom_type == REFINEMENT_NOTICE_CUSTOM_TYPE)
            })
            .collect();
        assert_eq!(custom_messages.len(), 2);
        // History merges session results for the next refinement.
        assert_eq!(load_refinement_history(&session, &global_dir).len(), 1);
    }

    #[tokio::test]
    async fn global_refinement_appends_history() {
        let dir = TempDir::new().unwrap();
        let mut session = persisted_session(&dir);
        let global_dir = dir.path().join("harness");
        let reply = r#"{"summary":"global lesson","edits":[{"action":"create","kind":"memory","id":"g1","title":"Lesson","content":"durable"}]}"#;
        let result = execute_refinement(
            &mut session,
            &[user_message("x")],
            &global_dir,
            &test_model(),
            &RefineOptions {
                global: true,
                ..Default::default()
            },
            RefinementSource::SelfRefine,
            seam(reply),
        )
        .await
        .unwrap();
        assert_eq!(result.scope, Some(HarnessScope::Global));
        // Global refinements land in the global store and the cross-session log.
        let global_state = load_harness_state(&global_dir, HarnessScope::Global);
        assert!(
            global_state.entries[&crate::refinement::RefinementKind::Memory].contains_key("g1")
        );
        let history = load_global_refinement_history(&global_dir);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, result.id);
        // Rollback by id works through the merged history.
        let rolled = execute_refinement(
            &mut session,
            &[],
            &global_dir,
            &test_model(),
            &RefineOptions {
                global: true,
                rollback_id: Some(result.id.clone()),
                ..Default::default()
            },
            RefinementSource::User,
            seam("unused"),
        )
        .await
        .unwrap();
        assert_eq!(rolled.rollback_of.as_deref(), Some(result.id.as_str()));
        let global_state = load_harness_state(&global_dir, HarnessScope::Global);
        assert!(
            !global_state.entries[&crate::refinement::RefinementKind::Memory].contains_key("g1")
        );
    }
}
