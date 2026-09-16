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
