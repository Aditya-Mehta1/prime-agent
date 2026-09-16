//! Autonomous mode: run-state accounting, continuation decisions, and
//! shell-based quality gates. Port of core/autonomous.ts.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEFAULT_AUTONOMOUS_CONTINUATION_PROMPT: &str = "No human input is available in autonomous mode. Continue working until the host evaluator, verifier, or configured autonomous limits stop the run. If you were asking the user a question, make a reasonable assumption and verify it. If you believe you are blocked, prove it with host-observable evidence, preserve that evidence, and keep looking for safe progress while budget remains. Do not end the session yourself; the verifier/evaluator decides completion when configured gates pass.";

pub const DEFAULT_MAX_CONTINUATIONS: u64 = 3;
pub const DEFAULT_MAX_TURNS: u64 = 12;
pub const DEFAULT_MAX_TOKENS: u64 = 80_000;
pub const DEFAULT_TIMEOUT_MS: u64 = 30 * 60 * 1000;
pub const DEFAULT_GATE_MAX_RETRIES: u64 = 3;
pub const DEFAULT_GATE_TIMEOUT_MS: u64 = 5 * 60 * 1000;
/// One keep-alive continuation per 25 minutes of continuous subagent
/// activity, strictly below the default wall-clock budget.
pub const DEFAULT_SUBAGENT_KEEP_ALIVE_MS: u64 = 25 * 60 * 1000;
/// Largest keep-alive window accepted (a Node-era clamp; still a sane cap).
pub const MAX_SUBAGENT_KEEP_ALIVE_MS: u64 = 2_147_483_647;
/// JSON-safe sentinel meaning "no cap".
pub const UNLIMITED_AUTONOMOUS_LIMIT: u64 = 9_007_199_254_740_991;

const MAX_GATE_OUTPUT_CHARS: usize = 6000;
const MAX_CHILD_PROCESS_OUTPUT_CHARS: usize = 1024 * 1024;

/// User-facing configuration (`/autonomous` options).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentAutonomousConfig {
    pub enabled: Option<bool>,
    pub max_continuations: Option<u64>,
    pub max_turns: Option<u64>,
    pub max_tokens: Option<u64>,
    pub timeout_ms: Option<u64>,
    pub continuation_prompt: Option<String>,
    pub gates: Option<AgentAutonomousGateConfig>,
    /// `0` disables the subagent keep-alive valve.
    pub subagent_keep_alive_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentAutonomousGateConfig {
    pub commands: Option<Vec<String>>,
    pub max_retries: Option<u64>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAutonomousGateFailure {
    pub command: String,
    pub attempt: u64,
    pub exit_text: String,
    pub output: String,
}

/// Hard limits after normalization.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutonomousLimits {
    pub max_continuations: u64,
    pub max_turns: u64,
    pub max_tokens: u64,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAutonomousStatus {
    pub enabled: bool,
    pub continuations_used: u64,
    pub turns_used: u64,
    pub tokens_used: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    pub limits: AutonomousLimits,
    pub gates: NormalizedGateConfig,
    pub gate_attempts: HashMap<String, u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_gate_failure: Option<AgentAutonomousGateFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_keep_alive_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedGateConfig {
    pub commands: Vec<String>,
    pub max_retries: u64,
    pub timeout_ms: u64,
}

/// The mutable runtime state for one autonomous run.
#[derive(Debug, Clone, PartialEq)]
pub struct AutonomousRuntimeState {
    pub enabled: bool,
    pub continuations_used: u64,
    pub turns_used: u64,
    pub tokens_used: u64,
    pub started_at: Option<u64>,
    pub limits: AutonomousLimits,
    pub continuation_prompt: String,
    pub gates: NormalizedGateConfig,
    pub gate_attempts: HashMap<String, u64>,
    pub last_gate_failure: Option<AgentAutonomousGateFailure>,
    pub last_gate_failure_snapshot: Option<GitWorktreeSnapshot>,
    pub subagent_keep_alive_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutonomousLimitReason {
    MaxContinuations,
    MaxTurns,
    MaxTokens,
    TimeoutMs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutonomousGateResult {
    Passed,
    Failed,
    RetryExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutonomousDecisionReason {
    MissingTerminalEvidence,
    GateFailed,
    NotNeeded,
    LimitReached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutonomousDecision {
    pub should_continue: bool,
    pub reason: AutonomousDecisionReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitWorktreeSnapshot {
    pub status: String,
    pub diff: String,
    pub untracked_hash: String,
}

pub fn is_unlimited_autonomous_limit(value: u64) -> bool {
    value >= UNLIMITED_AUTONOMOUS_LIMIT
}

fn normalize_limit(value: Option<u64>, default: u64) -> u64 {
    value.filter(|value| *value > 0).unwrap_or(default)
}

fn normalize_subagent_keep_alive_ms(value: Option<u64>) -> u64 {
    if value == Some(0) {
        return 0;
    }
    normalize_limit(value, DEFAULT_SUBAGENT_KEEP_ALIVE_MS).min(MAX_SUBAGENT_KEEP_ALIVE_MS)
}

pub fn create_autonomous_runtime_state(
    config: Option<&AgentAutonomousConfig>,
    default_limits: Option<&AgentAutonomousConfig>,
) -> AutonomousRuntimeState {
    let defaults = AgentAutonomousConfig::default();
    let defaults = default_limits.unwrap_or(&defaults);
    let enabled = config.is_some_and(|config| config.enabled == Some(true));
    let Some(config) = config else {
        return AutonomousRuntimeState {
            enabled: false,
            continuations_used: 0,
            turns_used: 0,
            tokens_used: 0,
            started_at: None,
            limits: AutonomousLimits {
                max_continuations: normalize_limit(
                    defaults.max_continuations,
                    DEFAULT_MAX_CONTINUATIONS,
                ),
                max_turns: normalize_limit(defaults.max_turns, DEFAULT_MAX_TURNS),
                max_tokens: normalize_limit(defaults.max_tokens, DEFAULT_MAX_TOKENS),
                timeout_ms: normalize_limit(defaults.timeout_ms, DEFAULT_TIMEOUT_MS),
            },
            continuation_prompt: DEFAULT_AUTONOMOUS_CONTINUATION_PROMPT.to_string(),
            gates: NormalizedGateConfig {
                commands: Vec::new(),
                max_retries: DEFAULT_GATE_MAX_RETRIES,
                timeout_ms: DEFAULT_GATE_TIMEOUT_MS,
            },
            gate_attempts: HashMap::new(),
            last_gate_failure: None,
            last_gate_failure_snapshot: None,
            subagent_keep_alive_ms: normalize_subagent_keep_alive_ms(None),
        };
    };
    AutonomousRuntimeState {
        enabled,
        continuations_used: 0,
        turns_used: 0,
        tokens_used: 0,
        started_at: enabled.then(now_millis),
        limits: AutonomousLimits {
            max_continuations: normalize_limit(
                config.max_continuations,
                normalize_limit(defaults.max_continuations, DEFAULT_MAX_CONTINUATIONS),
            ),
            max_turns: normalize_limit(
                config.max_turns,
                normalize_limit(defaults.max_turns, DEFAULT_MAX_TURNS),
            ),
            max_tokens: normalize_limit(
                config.max_tokens,
                normalize_limit(defaults.max_tokens, DEFAULT_MAX_TOKENS),
            ),
            timeout_ms: normalize_limit(
                config.timeout_ms,
                normalize_limit(defaults.timeout_ms, DEFAULT_TIMEOUT_MS),
            ),
        },
        continuation_prompt: config
            .continuation_prompt
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or(DEFAULT_AUTONOMOUS_CONTINUATION_PROMPT)
            .to_string(),
        gates: NormalizedGateConfig {
            commands: config
                .gates
                .as_ref()
                .and_then(|gates| gates.commands.clone())
                .unwrap_or_default(),
            max_retries: normalize_limit(
                config.gates.as_ref().and_then(|gates| gates.max_retries),
                DEFAULT_GATE_MAX_RETRIES,
            ),
            timeout_ms: normalize_limit(
                config.gates.as_ref().and_then(|gates| gates.timeout_ms),
                DEFAULT_GATE_TIMEOUT_MS,
            ),
        },
        gate_attempts: HashMap::new(),
        last_gate_failure: None,
        last_gate_failure_snapshot: None,
        subagent_keep_alive_ms: normalize_subagent_keep_alive_ms(config.subagent_keep_alive_ms),
    }
}

/// Enable/disable the run; enabling resets all counters and failure state.
pub fn set_autonomous_enabled(state: &mut AutonomousRuntimeState, enabled: bool) {
    state.enabled = enabled;
    state.gate_attempts.clear();
    state.last_gate_failure = None;
    state.last_gate_failure_snapshot = None;
    if enabled {
        state.continuations_used = 0;
        state.turns_used = 0;
        state.tokens_used = 0;
        state.started_at = Some(now_millis());
    } else {
        state.started_at = None;
    }
}

/// Apply only the fields present in `config`; the rest keep their values.
pub fn set_autonomous_limits(state: &mut AutonomousRuntimeState, config: &AgentAutonomousConfig) {
    state.limits.max_continuations =
        normalize_limit(config.max_continuations, state.limits.max_continuations);
    state.limits.max_turns = normalize_limit(config.max_turns, state.limits.max_turns);
    state.limits.max_tokens = normalize_limit(config.max_tokens, state.limits.max_tokens);
    state.limits.timeout_ms = normalize_limit(config.timeout_ms, state.limits.timeout_ms);
    if let Some(prompt) = config.continuation_prompt.as_deref().map(str::trim) {
        if !prompt.is_empty() {
            state.continuation_prompt = prompt.to_string();
        }
    }
    if let Some(gates) = &config.gates {
        if let Some(commands) = &gates.commands {
            state.gates.commands = commands.clone();
        }
        state.gates.max_retries = normalize_limit(gates.max_retries, state.gates.max_retries);
        state.gates.timeout_ms = normalize_limit(gates.timeout_ms, state.gates.timeout_ms);
    }
    if config.subagent_keep_alive_ms.is_some() {
        state.subagent_keep_alive_ms =
            normalize_subagent_keep_alive_ms(config.subagent_keep_alive_ms);
    }
}

pub fn autonomous_status(state: &AutonomousRuntimeState) -> AgentAutonomousStatus {
    AgentAutonomousStatus {
        enabled: state.enabled,
        continuations_used: state.continuations_used,
        turns_used: state.turns_used,
        tokens_used: state.tokens_used,
        started_at: state.started_at,
        limits: state.limits,
        gates: state.gates.clone(),
        gate_attempts: state.gate_attempts.clone(),
        last_gate_failure: state.last_gate_failure.clone(),
        subagent_keep_alive_ms: Some(state.subagent_keep_alive_ms),
    }
}

/// Account one assistant turn's usage.
pub fn add_autonomous_usage(
    state: &mut AutonomousRuntimeState,
    usage: Option<&pa_types::ai::Usage>,
) {
    if !state.enabled {
        return;
    }
    state.turns_used += 1;
    state.tokens_used += autonomous_token_delta(usage);
}

pub fn add_autonomous_continuation(state: &mut AutonomousRuntimeState) {
    if !state.enabled {
        return;
    }
    state.continuations_used += 1;
}

/// Cache-read tokens are repeated context served from the provider cache;
/// count input + output + cache-write only.
pub fn autonomous_token_delta(usage: Option<&pa_types::ai::Usage>) -> u64 {
    match usage {
        Some(usage) => usage.input + usage.output + usage.cache_write,
        None => 0,
    }
}

/// Limit check against the current counters.
pub fn autonomous_limit_reason(
    state: &AutonomousRuntimeState,
    now: u64,
) -> Option<AutonomousLimitReason> {
    if state.continuations_used >= state.limits.max_continuations {
        return Some(AutonomousLimitReason::MaxContinuations);
    }
    if state.turns_used >= state.limits.max_turns {
        return Some(AutonomousLimitReason::MaxTurns);
    }
    if state.tokens_used >= state.limits.max_tokens {
        return Some(AutonomousLimitReason::MaxTokens);
    }
    if let Some(started_at) = state.started_at {
        if now.saturating_sub(started_at) >= state.limits.timeout_ms {
            return Some(AutonomousLimitReason::TimeoutMs);
        }
    }
    None
}

/// Decide whether an assistant turn should be followed by a continuation.
pub async fn should_autonomously_continue(
    state: &mut AutonomousRuntimeState,
    stop_reason: Option<pa_types::ai::StopReason>,
    gates: &dyn Fn(&str) -> anyhow::Result<ChildProcessResult>,
) -> AutonomousDecision {
    use pa_types::ai::StopReason;
    if !state.enabled
        || stop_reason == Some(StopReason::Error)
        || stop_reason == Some(StopReason::Aborted)
    {
        return AutonomousDecision {
            should_continue: false,
            reason: AutonomousDecisionReason::NotNeeded,
        };
    }
    let gate_result = refresh_autonomous_quality_gates(state, gates).await;
    let limit_reason = autonomous_limit_reason(state, now_millis());
    match gate_result {
        Some(AutonomousGateResult::Passed) => AutonomousDecision {
            should_continue: false,
            reason: AutonomousDecisionReason::NotNeeded,
        },
        Some(AutonomousGateResult::RetryExhausted) => AutonomousDecision {
            should_continue: false,
            reason: AutonomousDecisionReason::LimitReached,
        },
        None if limit_reason.is_some() => AutonomousDecision {
            should_continue: false,
            reason: AutonomousDecisionReason::LimitReached,
        },
        Some(AutonomousGateResult::Failed) => {
            if limit_reason.is_some() {
                AutonomousDecision {
                    should_continue: false,
                    reason: AutonomousDecisionReason::LimitReached,
                }
            } else {
                AutonomousDecision {
                    should_continue: true,
                    reason: AutonomousDecisionReason::GateFailed,
                }
            }
        }
        None => AutonomousDecision {
            should_continue: true,
            reason: AutonomousDecisionReason::MissingTerminalEvidence,
        },
    }
}

async fn refresh_autonomous_quality_gates(
    state: &mut AutonomousRuntimeState,
    gates: &dyn Fn(&str) -> anyhow::Result<ChildProcessResult>,
) -> Option<AutonomousGateResult> {
    if !state.enabled || state.gates.commands.is_empty() {
        return None;
    }
    Some(run_autonomous_quality_gates(state, gates).await)
}

async fn run_autonomous_quality_gates(
    state: &mut AutonomousRuntimeState,
    gates: &dyn Fn(&str) -> anyhow::Result<ChildProcessResult>,
) -> AutonomousGateResult {
    let commands = state.gates.commands.clone();
    let max_retries = state.gates.max_retries;
    for command in &commands {
        let same_failure = state
            .last_gate_failure
            .as_ref()
            .is_some_and(|failure| &failure.command == command)
            && state.last_gate_failure_snapshot.is_some();
        if same_failure {
            let attempt = state.gate_attempts.get(command).copied().unwrap_or(
                state
                    .last_gate_failure
                    .as_ref()
                    .map(|failure| failure.attempt)
                    .unwrap_or(0),
            ) + 1;
            state.gate_attempts.insert(command.clone(), attempt);
            let mut failure = state.last_gate_failure.clone().unwrap();
            failure.attempt = attempt;
            failure.exit_text =
                "not rerun: workspace unchanged since previous failed gate".to_string();
            failure.output = "The autonomous gate was not rerun because the workspace has not changed since this failure. Edit source files, tests, or a blocker artifact before attempting to finish again.".to_string();
            state.last_gate_failure = Some(failure);
            return if attempt > max_retries {
                AutonomousGateResult::RetryExhausted
            } else {
                AutonomousGateResult::Failed
            };
        }
        let Ok(result) = gates(command) else {
            // A spawn/IO failure counts as a failed attempt.
            let attempt = state.gate_attempts.get(command).copied().unwrap_or(0) + 1;
            state.gate_attempts.insert(command.clone(), attempt);
            state.last_gate_failure = Some(AgentAutonomousGateFailure {
                command: command.clone(),
                attempt,
                exit_text: "failed to run".to_string(),
                output: String::new(),
            });
            state.last_gate_failure_snapshot = None;
            return if attempt > max_retries {
                AutonomousGateResult::RetryExhausted
            } else {
                AutonomousGateResult::Failed
            };
        };
        if result.status == Some(0) && result.error.is_none() && !result.timed_out {
            state.gate_attempts.insert(command.clone(), 0);
            if state
                .last_gate_failure
                .as_ref()
                .is_some_and(|failure| &failure.command == command)
            {
                state.last_gate_failure = None;
                state.last_gate_failure_snapshot = None;
            }
            continue;
        }
        let attempt = state.gate_attempts.get(command).copied().unwrap_or(0) + 1;
        state.gate_attempts.insert(command.clone(), attempt);
        let output = [result.stdout.clone(), result.stderr.clone()]
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();
        state.last_gate_failure = Some(AgentAutonomousGateFailure {
            command: command.clone(),
            attempt,
            exit_text: format_process_exit(&result),
            output: truncate_gate_output(&output, result.output_truncated),
        });
        state.last_gate_failure_snapshot = None;
        return if attempt > max_retries {
            AutonomousGateResult::RetryExhausted
        } else {
            AutonomousGateResult::Failed
        };
    }
    state.last_gate_failure = None;
    state.last_gate_failure_snapshot = None;
    AutonomousGateResult::Passed
}

/// Continuation prompt for a failed gate.
pub fn build_autonomous_gate_failure_continuation(
    failure: &AgentAutonomousGateFailure,
    max_retries: u64,
    timestamp: u64,
) -> String {
    format!(
        "[autonomous-continuation: gate-failed]\n\nAutonomous quality gate failed (attempt {}/{}): `{}` {}.\n{}\n\nContinue working. Fix the failure, then produce terminal evidence. Timestamp: {}.",
        failure.attempt,
        max_retries,
        failure.command,
        failure.exit_text,
        if failure.output.is_empty() {
            String::new()
        } else {
            format!("\nOutput:\n{}\n", failure.output)
        },
        iso_timestamp(timestamp),
    )
}

/// The plain `[autonomous-continuation]` message body.
pub fn autonomous_continuation_text(state: &AutonomousRuntimeState) -> String {
    format!("[autonomous-continuation]\n\n{}", state.continuation_prompt)
}

/// Keep-alive message delivered while subagents are still active.
pub fn create_autonomous_subagent_keep_alive_text(state: &AutonomousRuntimeState) -> String {
    let minutes = (state.subagent_keep_alive_ms / 60_000).max(1);
    let plural = if minutes == 1 { "" } else { "s" };
    format!(
        "[autonomous-continuation: subagent-keep-alive]\n\nSubagents have been running for at least {minutes} minute{plural} without a reply or exit being delivered. Check their status (for example agent_observe, rlm.list_subagents, or process inspection) and cancel or unblock any that are hung; then continue working."
    )
}

fn truncate_gate_output(output: &str, was_truncated: bool) -> String {
    if output.chars().count() <= MAX_GATE_OUTPUT_CHARS {
        return output.to_string();
    }
    let prefix: String = output.chars().take(MAX_GATE_OUTPUT_CHARS - 1).collect();
    format!("{prefix}\u{2026}")
        .replace('\u{2026}', if was_truncated { " (truncated)" } else { "" })
        .trim()
        .to_string()
}

fn format_process_exit(result: &ChildProcessResult) -> String {
    if let Some(error) = &result.error {
        return error.clone();
    }
    if result.timed_out {
        return "timed out".to_string();
    }
    match result.status {
        Some(code) => format!("exited with code {code}"),
        None => "killed by signal".to_string(),
    }
}

/// Result of a gate child process.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChildProcessResult {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>,
    pub timed_out: bool,
    pub output_truncated: bool,
}

/// Run a shell command with a timeout and output caps (the real gate runner).
pub async fn run_gate_command(
    command: &str,
    cwd: &Path,
    timeout_ms: u64,
) -> anyhow::Result<ChildProcessResult> {
    let mut child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let timeout = tokio::time::Duration::from_millis(timeout_ms.max(1));
    let status = tokio::select! {
        _ = tokio::time::sleep(timeout) => None,
        status = child.wait() => status.ok(),
    };
    let Some(status) = status else {
        child.kill().await.ok();
        return Ok(ChildProcessResult {
            timed_out: true,
            ..Default::default()
        });
    };
    let (stdout, stdout_truncated) =
        read_pipe_capped(child.stdout.take(), MAX_CHILD_PROCESS_OUTPUT_CHARS).await;
    let (stderr, stderr_truncated) =
        read_pipe_capped_stderr(child.stderr.take(), MAX_CHILD_PROCESS_OUTPUT_CHARS).await;
    Ok(ChildProcessResult {
        status: status.code(),
        output_truncated: stdout_truncated || stderr_truncated,
        stdout,
        stderr,
        ..Default::default()
    })
}

async fn read_pipe_capped(
    mut pipe: Option<tokio::process::ChildStdout>,
    cap: usize,
) -> (String, bool) {
    use tokio::io::AsyncReadExt;
    let mut buffer = Vec::new();
    if let Some(pipe) = pipe.as_mut() {
        pipe.read_to_end(&mut buffer).await.ok();
    }
    capped_text(&buffer, cap)
}

async fn read_pipe_capped_stderr(
    mut pipe: Option<tokio::process::ChildStderr>,
    cap: usize,
) -> (String, bool) {
    use tokio::io::AsyncReadExt;
    let mut buffer = Vec::new();
    if let Some(pipe) = pipe.as_mut() {
        pipe.read_to_end(&mut buffer).await.ok();
    }
    capped_text(&buffer, cap)
}

fn capped_text(buffer: &[u8], cap: usize) -> (String, bool) {
    let text = String::from_utf8_lossy(buffer);
    if text.len() <= cap {
        return (text.to_string(), false);
    }
    let truncated: String = text.chars().take(cap).collect();
    (truncated, true)
}

fn iso_timestamp(millis: u64) -> String {
    crate::session::manager::format_iso(millis as i64)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[allow(unused)]
fn unused_hash() -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"");
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(enabled: bool) -> AgentAutonomousConfig {
        AgentAutonomousConfig {
            enabled: Some(enabled),
            ..Default::default()
        }
    }

    fn ok_gates(_command: &str) -> anyhow::Result<ChildProcessResult> {
        Ok(ChildProcessResult {
            status: Some(0),
            ..Default::default()
        })
    }

    fn failing_gates(_command: &str) -> anyhow::Result<ChildProcessResult> {
        Ok(ChildProcessResult {
            status: Some(1),
            stdout: "boom\n".to_string(),
            stderr: String::new(),
            ..Default::default()
        })
    }

    #[test]
    fn runtime_state_defaults_and_overrides() {
        let state = create_autonomous_runtime_state(Some(&config(true)), None);
        assert!(state.enabled);
        assert!(state.started_at.is_some());
        assert_eq!(state.limits.max_continuations, DEFAULT_MAX_CONTINUATIONS);
        assert_eq!(state.limits.max_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(
            state.continuation_prompt,
            DEFAULT_AUTONOMOUS_CONTINUATION_PROMPT
        );
        assert_eq!(state.subagent_keep_alive_ms, DEFAULT_SUBAGENT_KEEP_ALIVE_MS);
        // Explicit limits win; invalid (zero) values fall back.
        let custom = AgentAutonomousConfig {
            enabled: Some(true),
            max_turns: Some(2),
            max_tokens: Some(0),
            ..Default::default()
        };
        let custom_state = create_autonomous_runtime_state(Some(&custom), None);
        assert_eq!(custom_state.limits.max_turns, 2);
        assert_eq!(custom_state.limits.max_tokens, DEFAULT_MAX_TOKENS);
        // Disabled by default.
        let off = create_autonomous_runtime_state(None, None);
        assert!(!off.enabled);
        assert_eq!(off.started_at, None);
        // Setting limits only changes provided fields.
        let mut state = off;
        set_autonomous_limits(
            &mut state,
            &AgentAutonomousConfig {
                max_continuations: Some(7),
                continuation_prompt: Some("  keep going  ".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(state.limits.max_continuations, 7);
        assert_eq!(state.limits.max_turns, DEFAULT_MAX_TURNS);
        assert_eq!(state.continuation_prompt, "keep going");
    }

    #[test]
    fn enable_resets_and_disable_clears() {
        let mut state = create_autonomous_runtime_state(Some(&config(true)), None);
        add_autonomous_usage(&mut state, Some(&usage(10, 5)));
        add_autonomous_continuation(&mut state);
        set_autonomous_enabled(&mut state, false);
        assert!(!state.enabled);
        assert_eq!(state.started_at, None);
        assert_eq!(state.turns_used, 1, "disabling keeps counters");
        set_autonomous_enabled(&mut state, true);
        assert_eq!(state.turns_used, 0);
        assert_eq!(state.continuations_used, 0);
        assert!(state.started_at.is_some());
    }

    fn usage(input: u64, output: u64) -> pa_types::ai::Usage {
        pa_types::ai::Usage {
            input,
            output,
            cache_read: 0,
            cache_write: input,
            ..Default::default()
        }
    }

    #[test]
    fn usage_accounting_excludes_cache_reads() {
        let mut state = create_autonomous_runtime_state(Some(&config(true)), None);
        add_autonomous_usage(&mut state, Some(&usage(100, 40)));
        // Disabled state ignores usage.
        add_autonomous_usage(
            &mut AutonomousRuntimeState {
                enabled: false,
                ..state.clone()
            },
            Some(&usage(9, 9)),
        );
        assert_eq!(state.turns_used, 1);
        // input 100 + output 40 + cacheWrite 100.
        assert_eq!(state.tokens_used, 240);
    }

    #[test]
    fn limit_reasons() {
        let mut state = create_autonomous_runtime_state(Some(&config(true)), None);
        let now = state.started_at.unwrap_or(0);
        assert_eq!(autonomous_limit_reason(&state, now), None);
        state.continuations_used = state.limits.max_continuations;
        assert_eq!(
            autonomous_limit_reason(&state, u64::MAX),
            Some(AutonomousLimitReason::MaxContinuations)
        );
        state.continuations_used = 0;
        state.turns_used = state.limits.max_turns;
        assert_eq!(
            autonomous_limit_reason(&state, now),
            Some(AutonomousLimitReason::MaxTurns)
        );
        state.turns_used = 0;
        state.tokens_used = state.limits.max_tokens;
        assert_eq!(
            autonomous_limit_reason(&state, now),
            Some(AutonomousLimitReason::MaxTokens)
        );
        state.tokens_used = 0;
        state.started_at = Some(1_000);
        assert_eq!(
            autonomous_limit_reason(&state, 1_000 + state.limits.timeout_ms),
            Some(AutonomousLimitReason::TimeoutMs)
        );
    }

    #[tokio::test]
    async fn decisions_without_gates() {
        use pa_types::ai::StopReason;
        let mut state = create_autonomous_runtime_state(Some(&config(true)), None);
        let decision =
            should_autonomously_continue(&mut state, Some(StopReason::Stop), &ok_gates).await;
        assert!(decision.should_continue);
        assert_eq!(
            decision.reason,
            AutonomousDecisionReason::MissingTerminalEvidence
        );
        // Error and aborted turns never continue.
        let stopped =
            should_autonomously_continue(&mut state, Some(StopReason::Error), &ok_gates).await;
        assert!(!stopped.should_continue);
        let aborted =
            should_autonomously_continue(&mut state, Some(StopReason::Aborted), &ok_gates).await;
        assert!(!aborted.should_continue);
        // Disabled state never continues.
        state.enabled = false;
        let disabled =
            should_autonomously_continue(&mut state, Some(StopReason::Stop), &ok_gates).await;
        assert!(!disabled.should_continue);
    }

    #[tokio::test]
    async fn gate_results_drive_decisions() {
        use pa_types::ai::StopReason;
        let mut state = create_autonomous_runtime_state(
            Some(&AgentAutonomousConfig {
                enabled: Some(true),
                gates: Some(AgentAutonomousGateConfig {
                    commands: Some(vec!["make check".to_string()]),
                    max_retries: Some(1),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            None,
        );
        // Failing gate -> continue with gate_failed.
        let failed =
            should_autonomously_continue(&mut state, Some(StopReason::Stop), &failing_gates).await;
        assert!(failed.should_continue);
        assert_eq!(failed.reason, AutonomousDecisionReason::GateFailed);
        let failure = state.last_gate_failure.clone().unwrap();
        assert_eq!(failure.command, "make check");
        assert_eq!(failure.attempt, 1);
        assert_eq!(failure.exit_text, "exited with code 1");
        assert_eq!(failure.output, "boom");
        // Passing gate -> stop with not_needed.
        let passed =
            should_autonomously_continue(&mut state, Some(StopReason::Stop), &ok_gates).await;
        assert!(!passed.should_continue);
        assert_eq!(passed.reason, AutonomousDecisionReason::NotNeeded);
        // The pass reset the retry counter, so this failure starts a fresh
        // retry window (TS resets gateAttempts on pass).
        let failed_again =
            should_autonomously_continue(&mut state, Some(StopReason::Stop), &failing_gates).await;
        assert!(failed_again.should_continue);
        assert_eq!(failed_again.reason, AutonomousDecisionReason::GateFailed);
        // Without a pass in between, the next failure exhausts the window.
        let exhausted =
            should_autonomously_continue(&mut state, Some(StopReason::Stop), &failing_gates).await;
        assert!(!exhausted.should_continue);
        assert_eq!(exhausted.reason, AutonomousDecisionReason::LimitReached);
    }

    #[test]
    fn continuation_texts() {
        let state = create_autonomous_runtime_state(Some(&config(true)), None);
        assert_eq!(
            autonomous_continuation_text(&state),
            format!("[autonomous-continuation]\n\n{DEFAULT_AUTONOMOUS_CONTINUATION_PROMPT}")
        );
        let failure = AgentAutonomousGateFailure {
            command: "make check".to_string(),
            attempt: 2,
            exit_text: "exited with code 1".to_string(),
            output: "error here".to_string(),
        };
        let text = build_autonomous_gate_failure_continuation(&failure, 3, 0);
        assert!(text.starts_with("[autonomous-continuation: gate-failed]\n\nAutonomous quality gate failed (attempt 2/3): `make check` exited with code 1.\n\nOutput:\nerror here\n"));
        assert!(text.contains("Continue working. Fix the failure, then produce terminal evidence."));
        // Keep-alive text uses minute pluralization.
        let mut short = state.clone();
        short.subagent_keep_alive_ms = 60_000;
        assert!(create_autonomous_subagent_keep_alive_text(&short)
            .contains("at least 1 minute without"));
        assert!(create_autonomous_subagent_keep_alive_text(&state)
            .contains("at least 25 minutes without"));
    }

    #[test]
    fn unlimited_sentinel() {
        assert!(is_unlimited_autonomous_limit(UNLIMITED_AUTONOMOUS_LIMIT));
        assert!(!is_unlimited_autonomous_limit(80_000));
    }
}
