//! Headless e2e for the top bar's session spend (TS `TopBar`'s `getCostUsd`
//! over `refreshTopBarCost`): a mock supervisor serves one attached session
//! and streams scripted assistant turns.
//!
//! Verifies the TS parity contract of the cost source: the spend refreshes
//! from `get_context_tree`'s root `totalUsage.cost.total` — the cumulative
//! total that includes the compaction call's own usage — not from
//! `get_session_stats`, whose walk counts assistant messages only. A
//! session whose compaction spent $0.25 and whose assistant turns spent
//! $0.10 reads $0.35 on the top bar; the stats cost ($0.10) never renders,
//! and a later context-tree response without a finite total keeps the
//! cached spend (TS: "the cost is cosmetic").
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use pa_tui::interactive::{
    run_interactive, HeadlessPlan, HeadlessStep, InteractiveOptions, ModelSelection,
    SessionSelection, UiMode,
};
use serde_json::{json, Value};

struct MockSupervisor {
    listener: UnixListener,
    /// Prompts served so far: the context tree serves the spend-total
    /// shape until the second turn, then drops the total (the
    /// keep-previous arm).
    prompts_served: usize,
}

impl MockSupervisor {
    fn bind(socket: &std::path::Path) -> Self {
        MockSupervisor {
            listener: UnixListener::bind(socket).expect("bind mock socket"),
            prompts_served: 0,
        }
    }

    /// Serve one connection: attach an empty session, then stream one
    /// scripted assistant turn per prompt.
    fn serve(mut self) {
        let (stream, _) = self.listener.accept().expect("accept");
        let write_stream = stream.try_clone().expect("clone mock socket");
        let mut writer = write_stream;
        let mut reader = BufReader::new(stream);

        let hello = json!({
            "type": "daemon_hello",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "serverCapabilities": [],
            "clientId": "mock",
        });
        write_json(&mut writer, &hello);

        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(envelope) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let id = envelope.get("id").and_then(Value::as_str).unwrap_or("");
            let command = envelope.get("command").cloned().unwrap_or(Value::Null);
            let command_type = command
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            match command_type.as_str() {
                "create" => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "create",
                            "success": true,
                            "data": {
                                "activeSessionId": "s1",
                                "id": "s1",
                                "sessionId": "sess-1",
                                "sessionFile": "/tmp/sess-1.jsonl",
                            },
                        }),
                    );
                }
                "attach" => {
                    write_json(&mut writer, &attach_data(id));
                }
                "get_session_stats" => {
                    // The stats walk counts assistant messages only: the
                    // assistant turns spent $0.10, the compaction call's
                    // $0.25 stays out (TS `getSessionStats`).
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "get_session_stats",
                            "success": true,
                            "data": {
                                "contextUsage": { "tokens": 1200, "contextWindow": 200000 },
                                "cost": 0.10,
                            },
                        }),
                    );
                }
                "get_context_tree" => {
                    // The tree's `totalUsage` is the cumulative spend the
                    // top bar renders (TS `refreshTopBarCost`): the
                    // compaction call's $0.25 joins the assistant turns'
                    // $0.10. From the second turn on, the response carries
                    // no total: the cached spend must survive.
                    let total_usage = json!({
                        "input": 5100, "output": 1050, "cacheRead": 0, "cacheWrite": 0,
                        "totalTokens": 6150,
                        "cost": {
                            "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                            "total": 0.35,
                        },
                    });
                    let data = if self.prompts_served < 2 {
                        json!({
                            "id": "root",
                            "label": "topbar cost session",
                            "status": "active",
                            "ownUsage": total_usage,
                            "totalUsage": total_usage,
                            "children": [],
                        })
                    } else {
                        json!({})
                    };
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "get_context_tree",
                            "success": true,
                            "data": data,
                        }),
                    );
                }
                "prompt" => {
                    self.prompts_served += 1;
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "prompt",
                            "success": true,
                        }),
                    );
                    stream_turn(&mut writer);
                }
                "detach" => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "detach",
                            "success": true,
                        }),
                    );
                }
                _ => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": command_type,
                            "success": true,
                            "data": {},
                        }),
                    );
                }
            }
        }
    }
}

fn write_json(writer: &mut UnixStream, value: &Value) {
    let mut line = serde_json::to_string(value).expect("serialize mock frame");
    line.push('\n');
    writer.write_all(line.as_bytes()).expect("write mock frame");
    writer.flush().expect("flush mock frame");
}

/// The slim attach result: one empty session.
fn attach_data(id: &str) -> Value {
    json!({
        "type": "response",
        "id": id,
        "command": "attach",
        "success": true,
        "data": {
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "activeSessionId": "s1",
            "snapshot": {
                "activeSessionId": "s1",
                "summary": { "id": "s1", "cwd": "/tmp" },
                "state": {
                    "activeSessionId": "s1",
                    "cwd": "/tmp",
                    "sessionId": "sess-1",
                    "sessionName": "topbar cost session",
                    "model": null,
                    "isStreaming": false,
                    "isCompacting": false,
                    "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
                },
                "messages": [],
                "lastEventSequence": 0,
                "lastEventCursor": null,
            },
            "client": { "id": "mock", "capabilities": [] },
            "lastEventSequence": 0,
            "lastEventCursor": null,
        },
    })
}

/// One scripted assistant turn with a zero-cost usage block: the turn's
/// own spend must not reach the top bar (the tree alone feeds it).
fn stream_turn(writer: &mut UnixStream) {
    let event = |payload: Value| json!({ "type": "session_event", "activeSessionId": "s1", "event": payload });
    write_json(writer, &event(json!({ "type": "turn_start" })));
    write_json(
        writer,
        &event(json!({
            "type": "message_start",
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": "" }],
            },
            "assistantMessageEvent": { "type": "start" },
        })),
    );
    write_json(
        writer,
        &event(json!({
            "type": "message_end",
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": "all done" }],
                "usage": {
                    "input": 100,
                    "output": 300,
                    "cacheRead": 0,
                    "cacheWrite": 0,
                    "totalTokens": 400,
                    "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 },
                },
                "stopReason": "stop",
            },
        })),
    );
    write_json(writer, &event(json!({ "type": "turn_end" })));
    write_json(writer, &event(json!({ "type": "agent_end" })));
}

fn options(socket: PathBuf) -> InteractiveOptions {
    InteractiveOptions {
        socket_path: socket,
        cwd: PathBuf::from("/tmp"),
        session_dir: None,
        script_path: None,
        model_selection: ModelSelection::default(),
        model_catalog: Vec::new(),
        model_configured_providers: Default::default(),
        model_recent_models: Vec::new(),
        default_thinking_level: None,
        no_session: false,
        session: SessionSelection::New,
        initial_message: None,
        show_images: true,
        fullscreen_mouse: true,
        theme: "prime".to_string(),
        code_block_indent: "  ".to_string(),
        tree_filter_mode: String::new(),
        branch_summary_skip_prompt: false,
        version: "0.0.0".to_string(),
        onboarding: None,
        telemetry_disabled: None,
        client_auth: None,
        traces: None,
        provider_auth: None,
        update_commands: None,
        telemetry: None,
        keybindings: pa_tui::keybindings::KeybindingsManager::new(),
        session_rlm_depth: None,
        prompt_stash: Default::default(),
        session_has_children: false,
        client_settings: None,
    }
}

/// Run the headless plan against a fresh mock supervisor and return the
/// captured frames.
fn run_plan(steps: Vec<HeadlessStep>) -> Vec<String> {
    std::env::remove_var("TMUX");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let supervisor = MockSupervisor::bind(&socket);
    let handle = std::thread::spawn(move || supervisor.serve());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let plan = HeadlessPlan {
        steps,
        width: 100,
        height: 30,
    };
    let outcome = runtime
        .block_on(run_interactive(options(socket), UiMode::Headless(plan)))
        .expect("interactive run");
    handle.join().expect("mock supervisor finished");
    outcome.frames
}

/// The top bar's spend comes from `get_context_tree`'s `totalUsage`, not
/// `get_session_stats`: a $0.25 compaction plus $0.10 of assistant turns
/// reads `$0.35` once the fire-and-forget fetch lands (TS
/// `refreshTopBarCost` never blocks the open path, so the earliest frames
/// predate it), the stats' `$0.10` never renders, and a later tree
/// response without a total keeps the cached spend instead of clearing it.
#[test]
fn topbar_cost_reads_the_context_tree_total() {
    let steps = vec![
        HeadlessStep::WaitMs(400),
        HeadlessStep::Submit("hello".to_string()),
        HeadlessStep::WaitIdle { timeout_ms: 10_000 },
        HeadlessStep::WaitMs(400),
        HeadlessStep::Submit("hello again".to_string()),
        HeadlessStep::WaitIdle { timeout_ms: 10_000 },
        HeadlessStep::WaitMs(400),
    ];
    let frames = run_plan(steps);
    assert!(!frames.is_empty(), "frames were captured");
    assert!(
        frames.iter().any(|frame| frame.contains("$0.35")),
        "the tree total renders once the background fetch lands:\n{}",
        frames.join("\n---\n")
    );
    assert!(
        frames
            .last()
            .is_some_and(|frame| frame.contains("$0.35")),
        "the total-less tree response keeps the cached spend:\n{}",
        frames.join("\n---\n")
    );
    assert!(
        frames.iter().all(|frame| !frame.contains("$0.10")),
        "the session-stats cost never renders on the top bar:\n{}",
        frames.join("\n---\n")
    );
}
