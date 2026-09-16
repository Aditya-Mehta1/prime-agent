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
