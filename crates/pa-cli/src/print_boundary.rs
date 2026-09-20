//! The print runtime's turn-boundary compaction checks: the TS
//! `_checkCompaction` flow at the settled-turn boundary (`agent_end`) and
//! its pre-turn companion (`_runPreTurnCompaction`, which TS runs before
//! every admitted prompt).
//!
//! The overflow arm (Case 1) is the compact-and-retry recovery for a request
//! that exceeds the context window: a settled turn that errors with a
//! provider context-overflow drops the error turn from the loop context,
//! runs one compaction, and re-issues the turn on the compacted context
//! without re-adding the user message (TS `agent.continue()`). One attempt
//! per overflow; a retry that still overflows ends the turn with the TS
//! failure surface — the durable `compaction_outcome` row plus the
//! `compaction_end` event carrying the TS failure text. The arm also runs
//! before the next admitted prompt, so a stale overflow error left by a
//! previous run gets its recovery attempt on the resumed context.
//!
//! Output surfaces: json mode streams the TS session events (the
//! `compaction_start`/`compaction_end` pair and the outcome row's message
//! pair) on stdout; text mode stays quiet here — the durable rows surface
//! through the headless terminal result (stderr plus the exit code).

use std::path::PathBuf;

use pa_core::session_engine::compact_session::CompactOutcome;
use pa_core::session_engine::engine::SessionEngine;
use pa_core::session_engine::messages::{CompactionOutcomeKind, CompactionOutcomeReason};
use pa_core::session_engine::provider_adapter::json_round_trip;
use pa_core::session_engine::provider_retry::is_context_overflow_failure;
use pa_core::session_engine::TrailingAssistantFilter;
use pa_types::ai::Model;
use pa_types::session::AgentMessage as SessionAgentMessage;
use serde_json::{json, Value};

/// The TS failure text when one compact-and-retry attempt could not save
/// the turn (`_checkCompaction`'s reported state).
const OVERFLOW_RECOVERY_FAILED_MESSAGE: &str = "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model.";

/// One recovery attempt per overflow (TS `_overflowRecovery`): "attempted"
/// marks a compact-and-retry in flight; "reported" dedups the failure
/// notice when the retry overflows too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OverflowRecovery {
    #[default]
    Idle,
    Attempted,
    Reported,
}

/// Which boundary the arm runs at. The re-issue differs: a settled-turn
/// overflow compaction re-issues the turn (TS `agent.continue()`); a
/// pre-turn one leaves the loop to the admitted prompt, which continues
/// on the compacted context (TS `_runPreTurnCompaction` never re-issues).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverflowBoundary {
    SettledTurn,
    PreTurn,
}

/// What the overflow arm decided for the settled turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverflowOutcome {
    /// No arm fired (not an overflow, or a guard skipped it): the requested
    /// compaction and threshold arms still get their turn.
    NotApplicable,
    /// The compact-and-retry ran: the turn re-issued on the compacted
    /// context, and the newly settled turn needs the same checks.
    RetryTurn,
    /// The turn is over (a skipped or failed compaction, or the reported
    /// second overflow): only the requested-refinement consumption follows.
    Finished,
}

/// The print loop's turn-boundary state: the one-attempt overflow machine
/// plus the json/text output mode the surfaces depend on.
pub(crate) struct TurnBoundary {
    recovery: OverflowRecovery,
    /// json mode streams the TS session events on stdout; text mode reads
    /// the durable rows through the headless terminal result.
    json_mode: bool,
}

impl TurnBoundary {
    pub(crate) fn new(json_mode: bool) -> Self {
        Self {
            recovery: OverflowRecovery::Idle,
            json_mode,
        }
    }

    /// Reset the overflow recovery state (TS: a message that starts an
    /// agent run — the admitted prompt — and every settled non-error
    /// assistant turn reset `_overflowRecovery`).
    pub(crate) fn reset(&mut self) {
        self.recovery = OverflowRecovery::Idle;
    }

    /// The pre-turn check before an admitted prompt (TS
    /// `_runPreTurnCompaction`, which runs the same Case 1 over the last
    /// assistant message of the loop context): a stale overflow error from
    /// the previous run gets its compact-and-retry attempt here, so the new
    /// prompt runs on the compacted context. The prompt proceeds regardless
    /// of the compaction outcome (TS `resumeAfterFailure` never re-issues
    /// for overflow). The admitted prompt resets the recovery state right
    /// after the check (TS resets at the agent run's message start).
    pub(crate) async fn run_pre_turn(
        &mut self,
        engine: &SessionEngine,
        model: &Model,
        api_key: Option<String>,
    ) -> Result<(), String> {
        self.overflow_recovery_attempt(engine, model, api_key, OverflowBoundary::PreTurn)
            .await?;
        self.reset();
        Ok(())
    }

    /// The settled-turn boundary (TS `agent_end`): the overflow arm with its
    /// retry loop first, then — when the arm did not fire — the
    /// model-requested compaction and the threshold arm (the order TS keeps
    /// inside `_checkCompaction`: a requested run consumes the check, so the
    /// threshold is not re-evaluated after it), then the requested
    /// refinement (TS `_consumePendingRequestedRefine`, which runs whenever
    /// the turn did not re-issue).
    pub(crate) async fn run_at_settled_turn(
        &mut self,
        engine: &SessionEngine,
        model: &Model,
        api_key: Option<String>,
        global_harness_dir: PathBuf,
    ) -> Result<(), String> {
        let arm_finished = loop {
            match self
                .overflow_recovery_attempt(
                    engine,
                    model,
                    api_key.clone(),
                    OverflowBoundary::SettledTurn,
                )
                .await?
            {
                OverflowOutcome::RetryTurn => continue,
                outcome => break matches!(outcome, OverflowOutcome::Finished),
            }
        };
        if !arm_finished {
            // The requested arm: a pending model-requested compaction runs
            // as `requested` (the overflow arm's own runs consume the
            // request, so it only reaches here when Case 1 stayed silent).
            if let Some(outcome) = engine
                .consume_pending_compaction(model, api_key.clone(), None)
                .await
            {
                if let Err(error) = outcome {
                    eprintln!("pa-cli: requested compaction failed: {error:#}");
                }
            } else {
                // The threshold arm (TS `_checkCompaction` Case 3): the
                // settled turn's usage crossing the reserve headroom
                // compacts before the next prompt. The outcome persists in
                // the session entries the headless terminal result reads.
                if engine
                    .session
                    .auto_compaction_due(model.context_window)
                    .await
                {
                    if let Err(error) = engine
                        .session
                        .compact(None, model, api_key.clone(), None)
                        .await
                    {
                        eprintln!("pa-cli: auto-compaction failed: {error:#}");
                    }
                }
            }
        }
        // The requested refinement runs whenever the turn did not re-issue
        // (a retried turn consumes it at its own boundary).
        if let Some(Err(error)) = engine
            .consume_pending_refinement(model, api_key, global_harness_dir)
            .await
        {
            eprintln!("pa-cli: requested refinement failed: {error:#}");
        }
        Ok(())
    }

    /// The shared Case-1 body (TS `_checkCompaction` Case 1). Guard order is
    /// the TS one: a settled non-error turn resets the recovery state, the
    /// message must come from the session's current model, may not predate
    /// the latest compaction boundary, compaction must be enabled (or a
    /// pending model request covers it — the run consumes it), and the
    /// shared overflow classifier must recognize it. On a retry the
    /// settled-turn arm re-issues the turn without a new user message (TS
    /// `agent.continue()`); the pre-turn arm leaves the loop to the
    /// admitted prompt (the boundary the caller passes decides).
    async fn overflow_recovery_attempt(
        &mut self,
        engine: &SessionEngine,
        model: &Model,
        api_key: Option<String>,
        boundary: OverflowBoundary,
    ) -> Result<OverflowOutcome, String> {
        let Some(SessionAgentMessage::Assistant(wire)) =
            engine.session.last_assistant_message().await
        else {
            return Ok(OverflowOutcome::NotApplicable);
        };
        // A settled non-error turn resets the recovery state (TS resets at
        // every non-error assistant message end).
        if wire.stop_reason != pa_types::ai::StopReason::Error {
            self.reset();
            return Ok(OverflowOutcome::NotApplicable);
        }
        // Skip the overflow check when the message came from a different
        // model (TS `sameModel`: a model switch must not compact for the
        // old model's overflow).
        if wire.provider != model.provider || wire.model != model.id {
            return Ok(OverflowOutcome::NotApplicable);
        }
        // Skip the check when the message predates the latest compaction
        // boundary (TS `assistantIsFromBeforeCompaction`): a stale
        // pre-compaction overflow must not retrigger.
        if engine
            .session
            .latest_compaction_timestamp()
            .await
            .is_some_and(|timestamp| wire.timestamp <= timestamp)
        {
            return Ok(OverflowOutcome::NotApplicable);
        }
        // Enablement: the compaction settings gate, or a pending model
        // request (the run below consumes it and honors its instructions).
        let pending_scheduled = engine.turn_boundary.compaction_scheduled().await;
        if !engine.session.auto_compaction_enabled() && !pending_scheduled {
            return Ok(OverflowOutcome::NotApplicable);
        }
        // The shared overflow classifier (TS `isContextOverflow`).
        let Some(assistant) = json_round_trip::<_, pa_agent::types::AssistantMessage>(&wire) else {
            return Ok(OverflowOutcome::NotApplicable);
        };
        if !is_context_overflow_failure(&assistant, model.context_window) {
            return Ok(OverflowOutcome::NotApplicable);
        }
        // One recovery attempt per overflow (TS `_overflowRecovery`).
        match self.recovery {
            OverflowRecovery::Attempted => {
                self.recovery = OverflowRecovery::Reported;
                // The retry still overflows: report once — the durable
                // outcome row plus the `compaction_end` failure (no error
                // severity on the wire — TS passes none for the auto arms).
                self.end_unsuccessfully(
                    engine,
                    CompactionOutcomeKind::Failed,
                    OVERFLOW_RECOVERY_FAILED_MESSAGE,
                )
                .await;
                return Ok(OverflowOutcome::Finished);
            }
            OverflowRecovery::Reported => return Ok(OverflowOutcome::NotApplicable),
            OverflowRecovery::Idle => self.recovery = OverflowRecovery::Attempted,
        }
        // Remove the error turn from the loop context first (TS: it stays
        // in the session history, but the retry must not re-send it).
        engine
            .session
            .drop_trailing_assistant(TrailingAssistantFilter::Any)
            .await;
        // Any compaction consumes a pending model request and honors its
        // instructions (overflow can fire first and take the request with it).
        let custom_instructions = engine
            .turn_boundary
            .take_compaction()
            .await
            .and_then(|pending| pending.instructions);
        let mut start = json!({ "type": "compaction_start", "reason": "overflow" });
        if let Some(instructions) = custom_instructions.as_deref() {
            start["customInstructions"] = json!(instructions);
        }
        self.emit_json(start);
        // Headless compactions run unsignaled (TS print-mode compactions
        // have no abort trigger), so no abort race wraps the run.
        let outcome = engine
            .session
            .compact(custom_instructions.as_deref(), model, api_key, None)
            .await;
        match outcome {
            Ok(CompactOutcome::Ran(run)) => {
                // Adoption telemetry (TS `compaction_end` handling counts
                // every completed compaction into the active run).
                if let Some(telemetry) = engine.telemetry.as_ref() {
                    telemetry.note_compaction();
                }
                // The wire result is the TS `CompactionResult` shape; the
                // end event carries `willRetry: true` (the turn re-issues).
                let result = json!({
                    "summary": run.result.summary,
                    "firstKeptEntryId": run.result.first_kept_entry_id,
                    "tokensBefore": run.result.tokens_before,
                    "details": run
                        .entry
                        .details
                        .clone()
                        .unwrap_or(json!({ "readFiles": [], "modifiedFiles": [] })),
                });
                let mut end = json!({
                    "type": "compaction_end",
                    "reason": "overflow",
                    "result": result,
                    "aborted": false,
                    "willRetry": true,
                });
                if let Some(instructions) = custom_instructions.as_deref() {
                    end["customInstructions"] = json!(instructions);
                }
                self.emit_json(end);
                // The compaction rebuild re-adds the error turn from the
                // kept tail: drop it again so the retried request is free
                // of it (TS will-retry branch).
                engine
                    .session
                    .drop_trailing_assistant(TrailingAssistantFilter::ErrorOnly)
                    .await;
                if boundary == OverflowBoundary::PreTurn {
                    // The admitted prompt continues the loop on the
                    // compacted context (TS `_runPreTurnCompaction` never
                    // re-issues; the prompt's own commit is the
                    // continuation).
                    return Ok(OverflowOutcome::Finished);
                }
                // Re-issue the turn without a new user message (TS
                // `agent.continue()`), then hand the newly settled turn
                // back to the boundary checks.
                engine
                    .session
                    .agent()
                    .continue_run()
                    .await
                    .map_err(|error| format!("{error:#}"))?;
                engine.session.agent().wait_for_idle().await;
                Ok(OverflowOutcome::RetryTurn)
            }
            // A skipped overflow recovery does not re-issue (TS excludes
            // overflow from `resumeAfterFailure`).
            Ok(CompactOutcome::Skipped(message)) => {
                self.end_unsuccessfully(
                    engine,
                    CompactionOutcomeKind::Skipped,
                    &format!("Auto-compaction skipped: {message}"),
                )
                .await;
                Ok(OverflowOutcome::Finished)
            }
            Err(error) => {
                self.end_unsuccessfully(
                    engine,
                    CompactionOutcomeKind::Failed,
                    &format!("Context overflow recovery failed: {error:#}"),
                )
                .await;
                Ok(OverflowOutcome::Finished)
            }
        }
    }

    /// The unsuccessful-compaction surface (TS `_endCompactionUnsuccessfully`
    /// -> `_persistCompactionOutcome`): record the durable
    /// `compaction_outcome` row (json mode also broadcasts its
    /// `message_start`/`message_end` pair on the event stream, exactly like
    /// the TS session's row push), then emit the `compaction_end` event. A
    /// skip carries `errorSeverity: "warning"`; automatic failures carry no
    /// `errorSeverity` (TS passes none). The text-mode surface rides the
    /// durable rows through the headless terminal result.
    async fn end_unsuccessfully(
        &self,
        engine: &SessionEngine,
        outcome: CompactionOutcomeKind,
        message: &str,
    ) {
        let row = engine
            .session
            .record_compaction_outcome(CompactionOutcomeReason::Overflow, outcome, message)
            .await;
        if self.json_mode {
            let value = crate::headless_autonomous::stop_row_wire_value(&row);
            for event_type in ["message_start", "message_end"] {
                println!("{}", json!({ "type": event_type, "message": value }));
            }
        }
        let mut event = json!({
            "type": "compaction_end",
            "reason": "overflow",
            "aborted": false,
            "willRetry": false,
            "errorMessage": message,
        });
        if outcome == CompactionOutcomeKind::Skipped {
            event["errorSeverity"] = json!("warning");
        }
        self.emit_json(event);
    }

    /// Stream one session event in json mode (text mode stays quiet here).
    fn emit_json(&self, event: Value) {
        if self.json_mode {
            println!("{event}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_core::session::manager::SessionManager;
    use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
    use pa_types::session::FileEntry;
    use serde_json::json;

    /// The faux provider registers process-globally; one test at a time
    /// keeps the queued responses deterministic. Async-aware: the guard
    /// spans the whole await-driven test body.
    static FAUX_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// The TS overflow error shape: an Anthropic token-overflow message.
    /// The retry-turn entry paces the stream (`delayMs`), so its settled
    /// message timestamp lands strictly after the compaction entry's (the
    /// `assistantIsFromBeforeCompaction` guard compares millisecond
    /// timestamps; a real provider round-trip spans more than one).
    fn overflow_error(delay_ms: u64) -> Value {
        let mut entry = json!({
            "text": "",
            "stopReason": "error",
            "errorMessage": "prompt is too long: 213462 tokens > 200000 maximum",
        });
        if delay_ms > 0 {
            entry["delayMs"] = json!(delay_ms);
        }
        entry
    }

    /// The faux error text as it surfaces on the settled message.
    const OVERFLOW_ERROR: &str = "prompt is too long: 213462 tokens > 200000 maximum";

    /// The compactable compaction settings (the `keepRecentTokens` cut
    /// keeps ~10 tokens, so an overflow recovery with pre-cut history
    /// summarizes it).
    fn compactable_settings() -> Value {
        json!({ "compaction": { "enabled": true, "reserveTokens": 1, "keepRecentTokens": 10 } })
    }

    /// One faux-driven engine over its own tempdir with explicit compaction
    /// settings, optionally resuming a persisted session file (the
    /// `--continue` shape).
    async fn faux_engine_with_settings(
        script: Value,
        settings: Value,
        session_manager: Option<SessionManager>,
    ) -> (SessionEngine, tempfile::TempDir, Model) {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("settings.json"), settings.to_string()).unwrap();
        let parsed = pa_ai::faux::script::parse_faux_script(&script).unwrap();
        let registration =
            pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
                models: Some(vec![parsed.model]),
                ..Default::default()
            });
        registration.set_responses(parsed.responses);
        let model = registration.get_model();
        let stream_fn =
            pa_core::session_engine::provider_adapter::real_stream_fn(None, model.clone());
        let agent_model: pa_agent::types::Model =
            json_round_trip(&model).expect("the faux model crosses the loop boundary");
        let session_manager = session_manager
            .or_else(|| {
                Some(SessionManager::persisted(
                    dir.path(),
                    &dir.path().join("sessions"),
                ))
            })
            .expect("a session manager");
        let engine = create_session(SessionEngineConfig {
            telemetry: None,
            cwd: dir.path().to_path_buf(),
            agent_dir,
            mcp_manager: None,
            model: Some(agent_model),
            thinking_level: None,
            stream_fn: Some(stream_fn),
            tools: Vec::new(),
            custom_system_prompt: None,
            prompt_guidelines: Vec::new(),
            generic_mcp_servers: Vec::new(),
            allow_recursion: None,
            session_manager: Some(session_manager),
            extra_host_handlers: None,
            conversation_log_path: None,
            additional_skill_paths: Vec::new(),
            additional_prompt_paths: Vec::new(),
            extra_builtin_skill_overrides: Vec::new(),
            rlm_subagent_host: None,
            rlm_depth: None,
            model_info: Some(model.clone()),
            cli_extension_sources: Vec::new(),
            extension_tool_allow_list: None,
        })
        .await
        .expect("the faux session assembles");
        (engine, dir, model)
    }

    /// Admit one prompt through the production flow: the pre-turn check,
    /// the prompt, and the settled-turn boundary.
    async fn admit(
        boundary: &mut TurnBoundary,
        engine: &SessionEngine,
        model: &Model,
        prompt: String,
    ) -> Result<(), String> {
        boundary.run_pre_turn(engine, model, None).await?;
        engine
            .session
            .prompt(&prompt, Default::default())
            .await
            .expect("the prompt admits");
        engine.session.agent().wait_for_idle().await;
        boundary
            .run_at_settled_turn(engine, model, None, std::path::PathBuf::new())
            .await
    }

    /// The durable `compaction_outcome` rows, in order.
    async fn outcome_rows(engine: &SessionEngine) -> Vec<pa_types::session::CustomMessageEntry> {
        engine
            .session
            .entries()
            .await
            .iter()
            .filter_map(|entry| match entry {
                FileEntry::CustomMessage { payload, .. }
                    if payload.custom_type == "compaction_outcome" =>
                {
                    Some(payload.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// The number of persisted compaction entries.
    async fn compaction_count(engine: &SessionEngine) -> usize {
        engine
            .session
            .entries()
            .await
            .iter()
            .filter(|entry| matches!(entry, FileEntry::Compaction { .. }))
            .count()
    }

    /// The persisted user messages (the retry must not re-add one).
    async fn user_texts(engine: &SessionEngine) -> Vec<String> {
        engine
            .session
            .entries()
            .await
            .iter()
            .filter_map(|entry| match entry {
                FileEntry::Message {
                    message: SessionAgentMessage::User(user),
                    ..
                } => Some(user.content.text()),
                _ => None,
            })
            .collect()
    }

    /// The last assistant message of the live loop context, if any.
    async fn last_assistant(engine: &SessionEngine) -> Option<pa_types::ai::AssistantMessage> {
        if let SessionAgentMessage::Assistant(assistant) =
            engine.session.last_assistant_message().await?
        {
            return Some(assistant);
        }
        None
    }

    /// The compact-and-retry recovery (TS `_checkCompaction` Case 1): an
    /// overflow error drops the failed turn from the loop context, runs one
    /// compaction, and re-issues the turn; when the retried turn overflows
    /// too, the turn ends with the reported failure surface — the durable
    /// `compaction_outcome` row with the TS failure text, exactly once.
    #[tokio::test]
    async fn overflow_compacts_retries_once_then_reports_the_failure() {
        let _faux = FAUX_TEST_LOCK.lock().await;
        let (engine, _dir, model) = faux_engine_with_settings(
            json!({
                "responses": [
                    {"text": "seed reply"},
                    overflow_error(0),
                    {"text": "the summary"},
                    overflow_error(25),
                ]
            }),
            compactable_settings(),
            None,
        )
        .await;
        let mut boundary = TurnBoundary::new(false);
        // A large seed turn, so the overflow recovery has pre-cut history
        // to summarize (the `keepRecentTokens` cut keeps ~10 tokens).
        admit(
            &mut boundary,
            &engine,
            &model,
            format!("seed turn {}", "x".repeat(48_000)),
        )
        .await
        .unwrap();
        admit(
            &mut boundary,
            &engine,
            &model,
            format!("overflow probe {}", "x".repeat(48_000)),
        )
        .await
        .unwrap();

        // One compact-and-retry attempt: the durable compaction entry
        // landed, and the reported failure row is the only outcome row.
        assert_eq!(compaction_count(&engine).await, 1);
        let rows = outcome_rows(&engine).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content.text(), OVERFLOW_RECOVERY_FAILED_MESSAGE);
        assert_eq!(
            rows[0].details,
            Some(json!({ "reason": "overflow", "outcome": "failed" }))
        );
        // The retried turn settled after the compaction (the serve-time
        // pacing puts its timestamp past the compaction boundary), and the
        // live context ends with the second overflow error — the surface
        // the headless terminal result reads (the row trails it).
        let last = last_assistant(&engine).await.expect("a settled error turn");
        assert_eq!(last.stop_reason, pa_types::ai::StopReason::Error);
        assert_eq!(last.error_message.as_deref(), Some(OVERFLOW_ERROR));
        // The retry re-issued without re-adding the user message.
        assert_eq!(user_texts(&engine).await.len(), 2);
    }

    /// The retry on the compacted context succeeds: one compaction entry,
    /// no failure rows, and the recovered turn is the settled outcome.
    #[tokio::test]
    async fn overflow_retry_succeeds_on_the_compacted_context() {
        let _faux = FAUX_TEST_LOCK.lock().await;
        let (engine, _dir, model) = faux_engine_with_settings(
            json!({
                "responses": [
                    {"text": "seed reply"},
                    overflow_error(0),
                    {"text": "the summary"},
                    {"text": "recovered reply"},
                ]
            }),
            compactable_settings(),
            None,
        )
        .await;
        let mut boundary = TurnBoundary::new(false);
        admit(
            &mut boundary,
            &engine,
            &model,
            format!("seed turn {}", "x".repeat(48_000)),
        )
        .await
        .unwrap();
        admit(
            &mut boundary,
            &engine,
            &model,
            format!("overflow probe {}", "x".repeat(48_000)),
        )
        .await
        .unwrap();

        assert_eq!(compaction_count(&engine).await, 1);
        assert!(outcome_rows(&engine).await.is_empty());
        let last = last_assistant(&engine).await.expect("a settled turn");
        let text = last
            .content
            .iter()
            .filter_map(|block| match block {
                pa_types::ai::AssistantContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(text, "recovered reply");
        assert_eq!(last.stop_reason, pa_types::ai::StopReason::Stop);
        assert_eq!(user_texts(&engine).await.len(), 2);
    }

    /// A skipped overflow recovery does not re-issue (TS excludes overflow
    /// from `resumeAfterFailure`): the durable `skipped` outcome row
    /// surfaces, the error turn is gone from the loop context (the next
    /// prompt's pre-turn check sees the prior non-error turn, exactly like
    /// the TS drop), and the next prompt proceeds without another
    /// compaction.
    #[tokio::test]
    async fn overflow_recovery_skip_surfaces_the_warning_row() {
        let _faux = FAUX_TEST_LOCK.lock().await;
        // `keepRecentTokens` beyond the whole session: the cut keeps
        // everything, so the compaction has no history to summarize.
        let (engine, _dir, model) = faux_engine_with_settings(
            json!({
                "responses": [
                    {"text": "seed reply"},
                    overflow_error(0),
                    {"text": "next reply"},
                ]
            }),
            json!({
                "compaction": {
                    "enabled": true, "reserveTokens": 1, "keepRecentTokens": 100000
                }
            }),
            None,
        )
        .await;
        let mut boundary = TurnBoundary::new(false);
        admit(&mut boundary, &engine, &model, "seed turn".to_string())
            .await
            .unwrap();
        admit(&mut boundary, &engine, &model, "overflow probe".to_string())
            .await
            .unwrap();

        let rows = outcome_rows(&engine).await;
        assert_eq!(rows.len(), 1);
        let skipped =
            "Auto-compaction skipped: Session is too short to compact — try again once it grows";
        assert_eq!(rows[0].content.text(), skipped);
        assert_eq!(
            rows[0].details,
            Some(json!({ "reason": "overflow", "outcome": "skipped" }))
        );
        assert_eq!(compaction_count(&engine).await, 0, "nothing committed");
        // The skipped recovery dropped the error turn: the next prompt's
        // pre-turn check finds the prior settled turn and no-ops, so the
        // prompt runs with no second compaction.
        admit(&mut boundary, &engine, &model, "next prompt".to_string())
            .await
            .unwrap();
        assert_eq!(compaction_count(&engine).await, 0);
        assert_eq!(outcome_rows(&engine).await.len(), 1);
        let last = last_assistant(&engine).await.expect("a settled turn");
        assert_eq!(last.stop_reason, pa_types::ai::StopReason::Stop);
        assert_eq!(
            user_texts(&engine).await,
            ["seed turn", "overflow probe", "next prompt"]
                .map(str::to_string)
                .to_vec()
        );
    }

    /// A stale overflow error from a previous run gets its recovery attempt
    /// before the next admitted prompt (TS `_runPreTurnCompaction` runs the
    /// same Case 1): the resumed session compacts first, then the prompt
    /// runs on the compacted context — the flow a `--continue` print run
    /// exhibits, verified against the TS binary.
    #[tokio::test]
    async fn stale_overflow_error_recovers_before_the_next_prompt_after_a_resume() {
        let _faux = FAUX_TEST_LOCK.lock().await;
        // Run one: compaction disabled, the probe overflows, and the error
        // turn stays in the persisted context with no recovery attempt.
        let (engine_a, dir_a, model_a) = faux_engine_with_settings(
            json!({ "responses": [{"text": "seed reply"}, overflow_error(0)] }),
            json!({
                "compaction": { "enabled": false, "reserveTokens": 1, "keepRecentTokens": 10 }
            }),
            None,
        )
        .await;
        let mut boundary = TurnBoundary::new(false);
        admit(
            &mut boundary,
            &engine_a,
            &model_a,
            format!("seed turn {}", "x".repeat(48_000)),
        )
        .await
        .unwrap();
        admit(
            &mut boundary,
            &engine_a,
            &model_a,
            format!("overflow probe {}", "x".repeat(48_000)),
        )
        .await
        .unwrap();
        assert_eq!(compaction_count(&engine_a).await, 0);
        assert!(outcome_rows(&engine_a).await.is_empty());

        // Run two: a fresh boundary over the persisted session (the
        // `--continue` shape) with compaction enabled — the pre-turn arm
        // recovers before the admitted prompt. The faux queue carries the
        // remaining responses (the summarizer, then the recovered turn).
        let session_file = dir_a
            .path()
            .join("sessions")
            .read_dir()
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.extension().and_then(|extension| extension.to_str()) == Some("jsonl"))
            .expect("the run-one session file");
        let resumed =
            SessionManager::open(dir_a.path(), &dir_a.path().join("sessions"), &session_file);
        let (engine_b, _dir_b, model_b) = faux_engine_with_settings(
            json!({
                "responses": [
                    {"text": "the stale recovery summary"},
                    {"text": "recovered after the resume"},
                ]
            }),
            compactable_settings(),
            Some(resumed),
        )
        .await;
        let mut boundary = TurnBoundary::new(false);
        boundary
            .run_pre_turn(&engine_b, &model_b, None)
            .await
            .unwrap();
        assert_eq!(
            compaction_count(&engine_b).await,
            1,
            "the pre-turn arm compacted the stale overflow"
        );
        assert!(outcome_rows(&engine_b).await.is_empty());
        // The stale error turn left the loop context: the admitted prompt
        // runs on the compacted context.
        let stale = last_assistant(&engine_b).await;
        assert!(
            !matches!(
                &stale,
                Some(message) if message.stop_reason == pa_types::ai::StopReason::Error
            ),
            "the stale overflow error is gone from the context"
        );
        admit(
            &mut boundary,
            &engine_b,
            &model_b,
            "next prompt".to_string(),
        )
        .await
        .unwrap();
        let last = last_assistant(&engine_b).await.expect("a settled turn");
        assert_eq!(last.stop_reason, pa_types::ai::StopReason::Stop);
        assert_eq!(user_texts(&engine_b).await.len(), 3);
    }

    /// A plain provider error is not an overflow: the arm never fires and
    /// the turn ends like any error turn.
    #[tokio::test]
    async fn non_overflow_error_never_triggers_the_arm() {
        let _faux = FAUX_TEST_LOCK.lock().await;
        let (engine, _dir, model) = faux_engine_with_settings(
            json!({
                "responses": [
                    {"text": "seed reply"},
                    {"text": "", "stopReason": "error", "errorMessage": "529 overloaded"},
                ]
            }),
            compactable_settings(),
            None,
        )
        .await;
        let mut boundary = TurnBoundary::new(false);
        admit(&mut boundary, &engine, &model, "seed turn".to_string())
            .await
            .unwrap();
        admit(&mut boundary, &engine, &model, "flaky turn".to_string())
            .await
            .unwrap();
        assert_eq!(compaction_count(&engine).await, 0);
        assert!(outcome_rows(&engine).await.is_empty());
        let last = last_assistant(&engine)
            .await
            .expect("the settled error turn");
        assert_eq!(last.error_message.as_deref(), Some("529 overloaded"));
    }

    /// The settings gate (TS `settings.enabled`): with automatic compaction
    /// disabled, an overflow error ends the turn with no recovery.
    #[tokio::test]
    async fn overflow_error_with_compaction_disabled_ends_without_recovery() {
        let _faux = FAUX_TEST_LOCK.lock().await;
        let (engine, _dir, model) = faux_engine_with_settings(
            json!({
                "responses": [
                    {"text": "seed reply"},
                    overflow_error(0),
                    {"text": "the summary"},
                ]
            }),
            json!({
                "compaction": { "enabled": false, "reserveTokens": 1, "keepRecentTokens": 10 }
            }),
            None,
        )
        .await;
        let mut boundary = TurnBoundary::new(false);
        admit(&mut boundary, &engine, &model, "seed turn".to_string())
            .await
            .unwrap();
        admit(&mut boundary, &engine, &model, "overflow probe".to_string())
            .await
            .unwrap();
        assert_eq!(compaction_count(&engine).await, 0);
        assert!(outcome_rows(&engine).await.is_empty());
        let last = last_assistant(&engine)
            .await
            .expect("the settled error turn");
        assert_eq!(last.error_message.as_deref(), Some(OVERFLOW_ERROR));
    }
}
