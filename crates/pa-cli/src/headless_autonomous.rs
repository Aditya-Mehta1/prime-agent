//! The autonomous gate loop for headless print/json runs — the verifier and
//! eval composition surface. CLI autonomous flags build the run state (TS
//! `runtimeAutonomousConfigFromArgs`); after every settled model turn the
//! [`ShellAutonomousDriver`] runs the configured gate commands in the
//! session cwd (the #98 seams, reused unmodified); continuations land as
//! durable user rows; a stop surfaces as the durable `autonomous_status`
//! row plus its `message_end` event, and the process exit code follows the
//! TS print-mode contract (`print-mode.ts` + the selection half of
//! `headless-completion.ts`, which live in pa-core).

use std::path::PathBuf;
use std::sync::Arc;

use pa_core::autonomous::{
    autonomous_limit_reason, autonomous_status, autonomous_stop_row,
    create_autonomous_runtime_state, describe_autonomous_limit, latest_autonomous_gate_attempt,
    now_millis, AgentAutonomousConfig, AutonomousDriver, AutonomousFollowUp,
    AutonomousRuntimeState, ShellAutonomousDriver,
};
use pa_core::session_engine::engine::SessionEngine;
use pa_core::session_engine::provider_adapter::json_round_trip;
use pa_types::ai::Model;
use pa_types::session::CustomMessage;

use crate::args::AutonomousConfig;

/// The autonomous runtime config from the typed CLI flags (TS
/// `runtimeAutonomousConfigFromArgs`: any autonomous flag enables the run).
pub fn autonomous_runtime_config(config: &AutonomousConfig) -> AgentAutonomousConfig {
    AgentAutonomousConfig {
        enabled: Some(true),
        max_continuations: config.max_continuations.map(u64::from),
        max_turns: config.max_turns.map(u64::from),
        max_tokens: config.max_tokens,
        timeout_ms: config.timeout_ms,
        continuation_prompt: None,
        gates: config
            .gates
            .as_ref()
            .map(|gates| pa_core::autonomous::AgentAutonomousGateConfig {
                commands: Some(gates.commands.clone()),
                max_retries: gates.max_retries.map(u64::from),
                timeout_ms: gates.timeout_ms,
            }),
        subagent_keep_alive_ms: None,
    }
}

/// One headless autonomous run: the runtime state plus the shell-gate
/// driver, shared with the per-message accounting subscription.
pub struct HeadlessAutonomous {
    state: Arc<tokio::sync::Mutex<AutonomousRuntimeState>>,
    driver: ShellAutonomousDriver,
}

impl HeadlessAutonomous {
    /// Build the run from the CLI flags; the gates run in the session cwd.
    pub fn from_cli(config: &AutonomousConfig, cwd: impl Into<PathBuf>) -> Self {
        let runtime_config = autonomous_runtime_config(config);
        let state = create_autonomous_runtime_state(Some(&runtime_config), None);
        Self {
            state: Arc::new(tokio::sync::Mutex::new(state)),
            driver: ShellAutonomousDriver::new(cwd),
        }
    }

    /// The runtime state handle (the session-command executor mutates the
    /// same state the accounting subscription and the drive loop read).
    pub(crate) fn state_handle(&self) -> Arc<tokio::sync::Mutex<AutonomousRuntimeState>> {
        Arc::clone(&self.state)
    }

    /// Per-message usage accounting: every settled assistant message is
    /// forwarded to the driver as it arrives (the daemon worker runs the
    /// same policy through its own subscription).
    pub async fn wire_accounting(
        &self,
        agent: &Arc<pa_agent::agent::Agent>,
    ) -> pa_agent::agent::Subscription {
        let state = Arc::clone(&self.state);
        let driver = self.driver.clone();
        agent
            .subscribe(move |event, _signal| {
                let state = Arc::clone(&state);
                let driver = driver.clone();
                Box::pin(async move {
                    if let pa_agent::types::AgentEvent::MessageEnd {
                        message:
                            pa_agent::types::AgentMessage::Standard(
                                pa_agent::types::Message::Assistant(assistant),
                            ),
                    } = &event
                    {
                        if let Some(message) =
                            json_round_trip::<_, pa_types::ai::AssistantMessage>(assistant)
                        {
                            let mut state = state.lock().await;
                            driver.account_message(&mut state, &message);
                        }
                    }
                    Ok(())
                })
            })
            .await
    }

    /// Drive the continuation loop after a settled prompt: the driver
    /// decides after every settled turn — inject the continuation text as
    /// the next turn, or stop. Each admitted continuation crosses the
    /// print boundary pair (TS: the session admits an owed continuation
    /// through its own turn loop, so `_runPreTurnCompaction` runs before
    /// the prompt and the `agent_end` compaction/refine arms after it —
    /// the arms fire on continuation turns exactly like CLI-prompt turns).
    /// Returns the durable stop row when the run stopped (`None` when
    /// autonomous mode was inactive for the turn).
    pub async fn drive(
        &self,
        engine: &SessionEngine,
        boundary: &mut crate::print_boundary::TurnBoundary,
        model: &Model,
        api_key: Option<String>,
        global_harness_dir: PathBuf,
    ) -> anyhow::Result<Option<CustomMessage>> {
        loop {
            let Some(message) = latest_assistant(engine).await else {
                return Ok(None);
            };
            let follow_up = {
                let mut state = self.state.lock().await;
                self.driver.after_turn(&mut state, &message).await
            };
            match follow_up {
                AutonomousFollowUp::Inactive => return Ok(None),
                AutonomousFollowUp::Continue { text } => {
                    boundary
                        .admit_continuation(
                            engine,
                            model,
                            api_key.clone(),
                            &text,
                            global_harness_dir.clone(),
                        )
                        .await
                        .map_err(anyhow::Error::msg)?;
                }
                AutonomousFollowUp::Stop { reason, status } => {
                    let row = autonomous_stop_row(&reason, &status);
                    persist_stop_row(engine, &row).await;
                    return Ok(Some(row));
                }
            }
        }
    }

    /// The TS print-mode exit contract: stderr text when the run must exit
    /// non-zero — a configured gate still failing (after its retry window,
    /// or with an autonomous limit reached), or an autonomous run without
    /// gates that stopped before terminal evidence.
    pub async fn exit_stderr(&self) -> Option<String> {
        let state = self.state.lock().await;
        let status = autonomous_status(&state);
        let now = now_millis();
        let limit = autonomous_limit_reason(&state, now);
        if let Some(failure) = status
            .last_gate_failure
            .as_ref()
            .filter(|_| status.enabled && !status.gates.commands.is_empty())
        {
            let limit_text = limit
                .map(|reason| {
                    format!(
                        "; autonomous limit reached: {}",
                        describe_autonomous_limit(&status, reason, now)
                    )
                })
                .unwrap_or_default();
            return Some(format!(
                "Autonomous quality gate still failing after attempt {}/{}: {}{}",
                latest_autonomous_gate_attempt(&status),
                status.gates.max_retries,
                failure.exit_text,
                limit_text
            ));
        }
        if status.enabled && status.gates.commands.is_empty() {
            if let Some(reason) = limit {
                return Some(format!(
                    "Autonomous run stopped before terminal evidence; {}",
                    describe_autonomous_limit(&status, reason, now)
                ));
            }
        }
        None
    }
}

/// The latest settled assistant message of the loop state, if any.
async fn latest_assistant(engine: &SessionEngine) -> Option<pa_types::ai::AssistantMessage> {
    let state = engine.session.agent().state().await;
    state
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(
                assistant,
            )) => json_round_trip(assistant),
            _ => None,
        })
}

/// The latest settled assistant message's error text, when the loop's
/// terminal state is a failed model request (the goal's terminal-error
/// surface: a failed turn fails an active goal, an abort keeps it).
pub(crate) async fn latest_assistant_error(engine: &SessionEngine) -> Option<Option<String>> {
    latest_assistant(engine).await.and_then(|message| {
        (message.stop_reason == pa_types::ai::StopReason::Error)
            .then(|| message.error_message.clone())
    })
}

/// Persist the durable stop row into the session state (same session, same
/// flush, as the session-command executor's rows).
async fn persist_stop_row(engine: &SessionEngine, row: &CustomMessage) {
    let session = engine.session.shared_persistence();
    let mut session = session.lock().await;
    session.append_custom_message(
        &row.custom_type,
        row.content.clone(),
        row.display,
        row.details.clone(),
    );
    session.flush_now();
}

/// The wire shape of the stop row for the json event stream: the custom
/// message in the shared message wire form.
pub fn stop_row_wire_value(row: &CustomMessage) -> serde_json::Value {
    serde_json::to_value(pa_types::session::AgentMessage::Custom(row.clone()))
        .unwrap_or(serde_json::Value::Null)
}
