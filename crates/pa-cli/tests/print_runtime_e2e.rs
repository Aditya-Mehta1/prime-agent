//! End-to-end print-runtime verification: the real binary, an isolated HOME,
//! and the scripted faux provider (PRIME_AGENT_FAUX_SCRIPT) drive the complete
//! pipeline — CLI parse, session assembly, agent loop, tool bridge seam, event
//! emission, headless terminal selection — deterministically.

use std::process::Command;

fn run(args: &[&str], script: &serde_json::Value) -> (String, String, i32) {
    let home = tempfile::TempDir::new().unwrap();
    let bin = env!("CARGO_BIN_EXE_prime-agent");
    let output = Command::new(bin)
        .args(args)
        .env("HOME", home.path())
        .env("PRIME_AGENT_AGENT_DIR", home.path().join("agent"))
        .env("PRIME_AGENT_FAUX_SCRIPT", script.to_string())
        .current_dir(home.path())
        .output()
        .expect("binary present");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code().unwrap_or(-1),
    )
}

#[test]
fn print_mode_text_output_matches_the_scripted_response() {
    let script = serde_json::json!({ "responses": ["first answer"] });
    let (stdout, stderr, code) = run(&["-p", "say something"], &script);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "first answer\n");
    assert!(stderr.is_empty());
}

#[test]
fn print_mode_multi_prompt_consumes_responses_in_order() {
    let script = serde_json::json!({ "responses": ["first answer", "second answer"] });
    let (stdout, _, code) = run(&["-p", "one", "two"], &script);
    assert_eq!(code, 0);
    // The terminal result is the final response.
    assert_eq!(stdout, "second answer\n");
}

#[test]
fn print_mode_json_streams_ts_shaped_events() {
    let script = serde_json::json!({ "responses": ["json answer"] });
    let (stdout, stderr, code) = run(&["--mode", "json", "-p", "hi"], &script);
    assert_eq!(code, 0, "stderr: {stderr}");
    let lines: Vec<serde_json::Value> = stdout
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .unwrap();
    // Header first, then the loop lifecycle.
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[0]["version"], 2);
    let types: Vec<&str> = lines
        .iter()
        .map(|line| line["type"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(types[1], "agent_start");
    assert!(types.contains(&"turn_start"));
    assert!(types.contains(&"message_start"));
    assert!(types.contains(&"message_end"));
    assert!(types.contains(&"turn_end"));
    // The user message carries the prompt; the assistant carries the response.
    let user = lines
        .iter()
        .find(|line| line["type"] == "message_start" && line["message"]["role"] == "user")
        .unwrap();
    assert_eq!(user["message"]["content"][0]["text"], "hi");
    let assistant = lines
        .iter()
        .find(|line| line["type"] == "message_end" && line["message"]["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant["message"]["content"][0]["text"], "json answer");
    assert_eq!(assistant["message"]["stopReason"], "stop");
    // The agent ends after the turn.
    assert_eq!(*types.last().unwrap(), "agent_end");
}

#[test]
fn print_mode_reports_provider_errors_as_exit_one() {
    // No responses queued: the faux provider returns an error stop reason.
    let script = serde_json::json!({ "responses": [] });
    let (stdout, stderr, code) = run(&["-p", "hi"], &script);
    assert_eq!(code, 1);
    assert!(stdout.is_empty());
    assert!(!stderr.is_empty());
}

// ---------------------------------------------------------------------------
// Session persistence (headless print sessions must land on disk)
// ---------------------------------------------------------------------------

fn isolated_home() -> tempfile::TempDir {
    tempfile::TempDir::new().unwrap()
}

fn run_in_home(
    home: &std::path::Path,
    args: &[&str],
    script: &serde_json::Value,
) -> (String, String, i32) {
    let bin = env!("CARGO_BIN_EXE_prime-agent");
    let output = Command::new(bin)
        .args(args)
        .env("HOME", home)
        .env("PRIME_AGENT_FAUX_SCRIPT", script.to_string())
        // Keep the isolated HOME authoritative: ambient agent/session dir
        // overrides from the test environment must not leak in.
        .env_remove("PRIME_AGENT_CODING_AGENT_DIR")
        .env_remove("PRIME_AGENT_SESSION_DIR")
        .env_remove("PRIME_AGENT_CODING_AGENT_SESSION_DIR")
        .current_dir(home)
        .output()
        .expect("binary present");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code().unwrap_or(-1),
    )
}

fn session_files(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    let dir = home.join(".prime/agent/sessions");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

fn read_entries(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn print_mode_persists_a_session_file_by_default() {
    let home = isolated_home();
    let script = serde_json::json!({ "responses": ["persisted answer"] });
    let (stdout, stderr, code) = run_in_home(home.path(), &["-p", "hello there"], &script);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "persisted answer\n");

    let files = session_files(home.path());
    assert_eq!(files.len(), 1, "one session file, got {files:?}");
    let entries = read_entries(&files[0]);

    // Header first, then the creation prefix, then user + assistant.
    let types: Vec<&str> = entries
        .iter()
        .map(|entry| entry["type"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(types[0], "session");
    assert!(types.contains(&"model_change"));
    assert!(types.contains(&"thinking_level_change"));
    let user = entries
        .iter()
        .find(|entry| entry["type"] == "message" && entry["message"]["role"] == "user")
        .expect("user message persisted");
    assert_eq!(user["message"]["content"][0]["text"], "hello there");
    let assistant = entries
        .iter()
        .find(|entry| entry["type"] == "message" && entry["message"]["role"] == "assistant")
        .expect("assistant message persisted");
    assert_eq!(
        assistant["message"]["content"][0]["text"],
        "persisted answer"
    );
    // The header records the run cwd (the isolated HOME).
    assert_eq!(entries[0]["cwd"], home.path().display().to_string());
}

#[test]
fn print_mode_no_session_writes_nothing() {
    let home = isolated_home();
    let script = serde_json::json!({ "responses": ["gone"] });
    let (stdout, stderr, code) = run_in_home(home.path(), &["-p", "hi", "--no-session"], &script);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "gone\n");
    assert!(session_files(home.path()).is_empty());
}

#[test]
fn print_mode_resume_appends_to_the_same_session_file() {
    let home = isolated_home();
    let script = serde_json::json!({ "responses": ["first answer"] });
    let (stdout, stderr, code) = run_in_home(home.path(), &["-p", "first"], &script);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "first answer\n");
    let files = session_files(home.path());
    assert_eq!(files.len(), 1);

    let session_id = read_entries(&files[0])[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let before = read_entries(&files[0]).len();

    // A uuid-v7 prefix selects the saved session; the run continues it.
    let selector = &session_id[..8];
    let script = serde_json::json!({ "responses": ["second answer"] });
    let (stdout, stderr, code) = run_in_home(
        home.path(),
        &["--resume", selector, "-p", "second"],
        &script,
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "second answer\n");

    let after_files = session_files(home.path());
    assert_eq!(after_files.len(), 1, "resume reuses the saved session");
    let entries = read_entries(&after_files[0]);
    assert!(entries.len() > before, "new messages were appended");
    let texts: Vec<&str> = entries
        .iter()
        .filter(|entry| entry["type"] == "message")
        .filter_map(|entry| entry["message"]["content"][0]["text"].as_str())
        .collect();
    assert!(texts.contains(&"second"));
}

#[test]
fn print_mode_continue_recent_reuses_the_latest_session() {
    let home = isolated_home();
    let script = serde_json::json!({ "responses": ["one"] });
    let (_, _, code) = run_in_home(home.path(), &["-p", "one"], &script);
    assert_eq!(code, 0);
    assert_eq!(session_files(home.path()).len(), 1);

    let script = serde_json::json!({ "responses": ["two"] });
    let (stdout, stderr, code) = run_in_home(home.path(), &["--continue", "-p", "two"], &script);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "two\n");
    assert_eq!(
        session_files(home.path()).len(),
        1,
        "continue reuses the saved session"
    );
}

#[test]
fn print_mode_resume_unknown_selector_fails_with_browse_hint() {
    let home = isolated_home();
    let script = serde_json::json!({ "responses": [] });
    let (stdout, stderr, code) =
        run_in_home(home.path(), &["--resume", "deadbeef", "-p", "hi"], &script);
    assert_eq!(code, 1);
    assert!(stdout.is_empty());
    assert!(
        stderr.contains("No session found matching 'deadbeef'"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("Open prime-agent and press left-arrow to browse sessions."),
        "stderr: {stderr}"
    );
}

/// Headless regression for thinking-level resolution: `--thinking max` on a
/// reasoning faux model (supported levels `off`..`high`) must persist the
/// clamped effective level in the session JSONL — the same clamp the
/// interactive daemon path now applies.
#[test]
fn print_mode_thinking_max_persists_the_clamped_high_level() {
    let home = isolated_home();
    // `reasoning: true` without a thinkingLevelMap: supported levels are
    // off/minimal/low/medium/high, so max clamps up-to-down to high.
    let script = serde_json::json!({ "reasoning": true, "responses": ["clamped answer"] });
    let (stdout, stderr, code) =
        run_in_home(home.path(), &["-p", "--thinking", "max", "hello"], &script);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "clamped answer\n");

    let files = session_files(home.path());
    assert_eq!(files.len(), 1, "one session file, got {files:?}");
    let entries = read_entries(&files[0]);
    let level = entries
        .iter()
        .find(|entry| entry["type"] == "thinking_level_change")
        .expect("thinking_level_change persisted");
    assert_eq!(level["thinkingLevel"], "high");
}

/// The clamp also applies on the way down: a non-reasoning faux model maps
/// any requested level to off.
#[test]
fn print_mode_thinking_clamps_to_off_for_non_reasoning_models() {
    let home = isolated_home();
    let script = serde_json::json!({ "responses": ["plain answer"] });
    let (stdout, stderr, code) =
        run_in_home(home.path(), &["-p", "--thinking", "high", "hello"], &script);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "plain answer\n");

    let files = session_files(home.path());
    assert_eq!(files.len(), 1, "one session file, got {files:?}");
    let entries = read_entries(&files[0]);
    let level = entries
        .iter()
        .find(|entry| entry["type"] == "thinking_level_change")
        .expect("thinking_level_change persisted");
    assert_eq!(level["thinkingLevel"], "off");
}

// ---------------------------------------------------------------------------
// Overflow compact-and-retry (TS `_checkCompaction` Case 1 in print mode)
// ---------------------------------------------------------------------------

/// Compaction settings into the isolated home's agent dir, resolved by the
/// session engine at assembly time.
fn write_compaction_settings(home: &std::path::Path, settings: &serde_json::Value) {
    let agent = home.join(".prime/agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(agent.join("settings.json"), settings.to_string()).unwrap();
}

/// The TS overflow error shape as a faux response entry (the `content`
/// form carries the stop reason and error message through the print
/// harness's script parser; `delayMs` paces the retried turn's timestamp
/// past the compaction boundary, like a real provider round-trip).
fn overflow_error(delay_ms: u64) -> serde_json::Value {
    let mut entry = serde_json::json!({
        "content": [{ "type": "text", "text": "" }],
        "stopReason": "error",
        "errorMessage": "prompt is too long: 213462 tokens > 200000 maximum",
    });
    if delay_ms > 0 {
        entry["delayMs"] = serde_json::json!(delay_ms);
    }
    entry
}

/// The compactable settings: the `keepRecentTokens` cut keeps ~10 tokens,
/// so an overflow recovery with pre-cut history summarizes it.
fn compactable_settings() -> serde_json::Value {
    serde_json::json!({
        "compaction": { "enabled": true, "reserveTokens": 1, "keepRecentTokens": 10 }
    })
}

/// The reported-overflow failure text (TS `_checkCompaction` verbatim).
const OVERFLOW_RECOVERY_FAILED: &str = "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model.";

/// The full compact-and-retry cycle (verified against the TS binary with
/// the same scripted provider): an overflow error drops the failed turn,
/// runs one compaction, re-issues the turn, and the second overflow ends
/// the run with the assistant error plus the reported failure row on
/// stderr, exit 1.
#[test]
fn print_mode_overflow_compacts_retries_once_then_reports() {
    let home = isolated_home();
    write_compaction_settings(home.path(), &compactable_settings());
    let script = serde_json::json!({
        "responses": [
            {"text": "seed reply"},
            overflow_error(0),
            {"text": "the summary"},
            overflow_error(50),
        ]
    });
    let seed = format!("seed turn {}", "x".repeat(48_000));
    let probe = format!("overflow probe {}", "x".repeat(48_000));
    let (stdout, stderr, code) = run_in_home(home.path(), &["-p", &seed, &probe], &script);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout}");
    let error_at = stderr
        .find("prompt is too long: 213462 tokens > 200000 maximum")
        .expect("the overflow error surfaces");
    let reported_at = stderr
        .find(OVERFLOW_RECOVERY_FAILED)
        .expect("the reported failure row surfaces");
    assert!(
        error_at < reported_at,
        "the primary error precedes the reported outcome row: {stderr}"
    );
    // The durable surface: one compaction entry, one failed outcome row,
    // and no re-added user message for the retried turn.
    let files = session_files(home.path());
    assert_eq!(files.len(), 1);
    let entries = read_entries(&files[0]);
    let compactions = entries
        .iter()
        .filter(|entry| entry["type"] == "compaction")
        .count();
    assert_eq!(compactions, 1, "one compaction entry");
    // A custom-row file entry flattens its payload: `customType` and the
    // details sit at the top level of the JSONL line.
    let outcome = entries
        .iter()
        .find(|entry| entry["customType"] == "compaction_outcome")
        .expect("the durable outcome row");
    let outcome_content = serde_json::to_string(&outcome["content"]).unwrap();
    assert!(outcome_content.contains(OVERFLOW_RECOVERY_FAILED));
    assert_eq!(outcome["details"]["reason"], "overflow");
    assert_eq!(outcome["details"]["outcome"], "failed");
    let users = entries
        .iter()
        .filter(|entry| entry["type"] == "message" && entry["message"]["role"] == "user")
        .count();
    assert_eq!(users, 2, "the retry re-issued without re-adding the prompt");
}

/// The retry on the compacted context recovers the turn: the final answer
/// prints normally, exit 0, and the compaction entry is durable.
#[test]
fn print_mode_overflow_retry_recovers_the_turn() {
    let home = isolated_home();
    write_compaction_settings(home.path(), &compactable_settings());
    let script = serde_json::json!({
        "responses": [
            {"text": "seed reply"},
            overflow_error(0),
            {"text": "the summary"},
            {"text": "recovered reply"},
        ]
    });
    let seed = format!("seed turn {}", "x".repeat(48_000));
    let probe = format!("overflow probe {}", "x".repeat(48_000));
    let (stdout, stderr, code) = run_in_home(home.path(), &["-p", &seed, &probe], &script);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "recovered reply\n");
    assert!(stderr.is_empty(), "stderr: {stderr}");
    let files = session_files(home.path());
    let entries = read_entries(&files[0]);
    let compactions = entries
        .iter()
        .filter(|entry| entry["type"] == "compaction")
        .count();
    assert_eq!(compactions, 1);
    assert!(
        !entries
            .iter()
            .any(|entry| entry["customType"] == "compaction_outcome"),
        "no failure rows on a recovered retry"
    );
}

/// A skipped overflow recovery (nothing compactable) surfaces the warning
/// row on stderr and exits 0 — the TS text-mode contract where the dropped
/// error turn leaves no primary answer (verified against the TS binary).
#[test]
fn print_mode_overflow_skip_surfaces_the_warning_row() {
    let home = isolated_home();
    write_compaction_settings(
        home.path(),
        &serde_json::json!({
            "compaction": { "enabled": true, "reserveTokens": 1, "keepRecentTokens": 100000 }
        }),
    );
    let script = serde_json::json!({ "responses": [overflow_error(0)] });
    let (stdout, stderr, code) = run_in_home(home.path(), &["-p", "overflow probe"], &script);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout}");
    assert_eq!(
        stderr,
        "Auto-compaction skipped: Session is too short to compact — try again once it grows\n"
    );
    assert!(
        !stderr.contains("No response produced."),
        "the TS text mode stays silent without a primary: {stderr}"
    );
}

/// The json-mode event stream for the compact-and-retry cycle (verified
/// against the TS binary's session events): the `compaction_start` /
/// `compaction_end` pair with `reason: "overflow"` and `willRetry: true`,
/// the retried turn without a new user message, and the reported failure
/// surface — the outcome row's message pair, then the `compaction_end`
/// failure. json mode keeps the TS exit contract (0 unless the autonomous
/// gates or a thrown error decide otherwise).
#[test]
fn print_mode_overflow_json_streams_the_compaction_events() {
    let home = isolated_home();
    write_compaction_settings(home.path(), &compactable_settings());
    let script = serde_json::json!({
        "responses": [
            {"text": "seed reply"},
            overflow_error(0),
            {"text": "the summary"},
            overflow_error(50),
        ]
    });
    let seed = format!("seed turn {}", "x".repeat(48_000));
    let probe = format!("overflow probe {}", "x".repeat(48_000));
    let (stdout, stderr, code) = run_in_home(
        home.path(),
        &["--mode", "json", "-p", &seed, &probe],
        &script,
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let events: Vec<serde_json::Value> = stdout
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .unwrap();
    // The pair before the retried turn: start, then end with willRetry.
    let start_at = events
        .iter()
        .position(|event| event["type"] == "compaction_start" && event["reason"] == "overflow")
        .expect("the compaction_start event");
    let success_at = events
        .iter()
        .position(|event| {
            event["type"] == "compaction_end"
                && event["reason"] == "overflow"
                && event["willRetry"] == true
        })
        .expect("the successful compaction_end event");
    assert!(start_at < success_at);
    assert_eq!(events[success_at]["result"]["summary"], "the summary");
    assert_eq!(events[success_at]["aborted"], false);
    // The retried turn's overflow error settles after the compaction.
    let retried_error_at = events
        .iter()
        .rposition(|event| {
            event["type"] == "message_end"
                && event["message"]["role"] == "assistant"
                && event["message"]["stopReason"] == "error"
        })
        .expect("the retried overflow error");
    assert!(
        success_at < retried_error_at,
        "the retried turn follows the compaction"
    );
    // No user message was re-added for the retry.
    let users = events
        .iter()
        .filter(|event| event["type"] == "message_start" && event["message"]["role"] == "user")
        .count();
    assert_eq!(users, 2);
    // The reported failure surface: the durable row's message pair, then
    // the `compaction_end` failure with the TS text and no severity.
    let row_at = events
        .iter()
        .position(|event| {
            event["type"] == "message_end"
                && event["message"]["customType"] == "compaction_outcome"
                && event["message"]["details"]["outcome"] == "failed"
        })
        .expect("the reported outcome row");
    let row_content = serde_json::to_string(&events[row_at]["message"]["content"]).unwrap();
    assert!(row_content.contains(OVERFLOW_RECOVERY_FAILED));
    assert_eq!(events[row_at]["message"]["details"]["reason"], "overflow");
    let reported_at = events
        .iter()
        .position(|event| {
            event["type"] == "compaction_end"
                && event["reason"] == "overflow"
                && event["willRetry"] == false
        })
        .expect("the reported compaction_end event");
    assert!(
        retried_error_at < row_at && row_at < reported_at,
        "the reported row pair precedes the end event"
    );
    assert_eq!(
        events[reported_at]["errorMessage"],
        OVERFLOW_RECOVERY_FAILED
    );
    assert!(
        events[reported_at].get("errorSeverity").is_none(),
        "automatic failures carry no error severity"
    );
    assert_eq!(
        events[start_at],
        serde_json::json!({"type": "compaction_start", "reason": "overflow"}),
        "the start event carries the TS shape"
    );
}

/// The stale-overflow recovery across runs (the `--continue` shape,
/// verified against the TS binary): a run with compaction disabled leaves
/// the overflow error in the session; the resumed run's pre-turn arm
/// compacts before the admitted prompt, which then answers normally.
#[test]
fn print_mode_stale_overflow_recovers_before_the_next_prompt_after_a_resume() {
    let home = isolated_home();
    // Run one: compaction disabled, the probe overflows.
    write_compaction_settings(
        home.path(),
        &serde_json::json!({
            "compaction": { "enabled": false, "reserveTokens": 1, "keepRecentTokens": 10 }
        }),
    );
    let script = serde_json::json!({
        "responses": [{"text": "seed reply"}, overflow_error(0)]
    });
    let seed = format!("seed turn {}", "x".repeat(48_000));
    let probe = format!("overflow probe {}", "x".repeat(48_000));
    let (stdout, stderr, code) = run_in_home(home.path(), &["-p", &seed, &probe], &script);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stdout.is_empty());

    // Run two: the resumed session with compaction enabled — the pre-turn
    // arm compacts the stale overflow before the prompt runs.
    write_compaction_settings(home.path(), &compactable_settings());
    let script = serde_json::json!({
        "responses": [{"text": "the stale recovery summary"}, {"text": "recovered after the resume"}]
    });
    let (stdout, stderr, code) =
        run_in_home(home.path(), &["--continue", "-p", "next prompt"], &script);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "recovered after the resume\n");
    assert!(stderr.is_empty(), "stderr: {stderr}");
    // The recovery compaction is durable in the resumed session file.
    let files = session_files(home.path());
    assert_eq!(files.len(), 1, "the resume reuses the session file");
    let entries = read_entries(&files[0]);
    let compactions = entries
        .iter()
        .filter(|entry| entry["type"] == "compaction")
        .count();
    assert_eq!(compactions, 1, "the pre-turn recovery compacted");
}
