//! End-to-end supervisor tests against the real `pa-daemon` binary: spawn the
//! supervisor on a temp socket, drive it with a JSONL socket client, verify
//! session lifecycle and streamed events with the scripted engine.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon {
    child: Child,
    #[allow(dead_code)]
    socket: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// The timeout panic path cannot wait on the child; the test process exits
// immediately afterwards, reaping it.
#[allow(clippy::zombie_processes)]
fn spawn_daemon(socket: &std::path::Path, agent_dir: &std::path::Path) -> Daemon {
    let binary = env!("CARGO_BIN_EXE_pa-daemon");
    let child = Command::new(binary)
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        .arg("--agent-dir")
        .arg(agent_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pa-daemon supervisor");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if socket.exists() {
            return Daemon {
                child,
                socket: socket.to_path_buf(),
            };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("supervisor socket never appeared");
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &std::path::Path) -> (Self, serde_json::Value) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("connect supervisor: {error}"),
            }
        };
        let write_stream = stream.try_clone().expect("clone socket");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer: write_stream,
        };
        let hello = client.read_line();
        (client, hello)
    }

    fn send(&mut self, value: &serde_json::Value) {
        let mut line = serde_json::to_string(value).expect("serialize command");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("send");
        self.writer.flush().expect("flush");
    }

    fn send_command(&mut self, id: &str, command: serde_json::Value) {
        self.send(&serde_json::json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        }));
    }

    fn read_line(&mut self) -> serde_json::Value {
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        self.reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set timeout");
        loop {
            line.clear();
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("supervisor closed the connection"),
                Ok(_) if line.trim().is_empty() => continue,
                Ok(_) => {
                    return serde_json::from_str(line.trim()).expect("parse response line");
                }
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for supervisor line: {error}"
                    );
                }
            }
        }
    }

    /// Read lines until one answers the given command id.
    fn read_response(&mut self, id: &str) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(Instant::now() < deadline, "no response for id {id}");
            let line = self.read_line();
            if line.get("id").and_then(|v| v.as_str()) == Some(id) {
                return line;
            }
        }
    }
}

#[test]
fn supervisor_end_to_end_scripted_session_lifecycle() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let _daemon = spawn_daemon(&socket, &agent_dir);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");
    // Differential goldens captured from the TS supervisor
    // (`prime-agent --mode daemon`, protocol 7, schema 28).
    assert_eq!(
        hello["protocol"],
        serde_json::json!({
            "name": "prime-agent.daemon", "version": 7
        })
    );
    assert_eq!(
        hello["schemaId"].as_str().map(|v| v.to_string()),
        Some("protocol-7-schema-28-92bc5368a082".to_string())
    );
    assert!(hello["supervisorOwnerToken"].is_string());
    assert!(hello["supervisorProcessStartId"]
        .as_str()
        .unwrap_or_default()
        .starts_with("proc:"));
    assert_eq!(
        hello["serverCapabilities"],
        serde_json::json!([
            "attach_snapshot",
            "event_sequence",
            "extension_ui",
            "slim_attach",
            "chunked_snapshot",
            "client_owned_sessions",
            "delete_rlm_subagent",
            "heartbeat_catalog",
            "heartbeat_management",
            "model_catalog",
            "side_question_transcript",
            "transient_bash",
            "session_input_admission",
            "prompt_admission_cancellation",
            "owned_prompt_cancellation",
            "queue_message_mutation",
            "authoritative_child_roster",
            "owned_session_recovery_context",
            "rlm_quiescence_barrier",
            "session_input_pause",
            "acp_mcp_servers",
            "agent_roster",
            "direct_peer_transport",
        ])
    );

    // Bare commands are rejected exactly like the TS supervisor: the
    // client-facing protocol requires the command envelope.
    client.send(&serde_json::json!({ "type": "list", "id": "bare" }));
    let rejected = client.read_response("bare");
    assert_eq!(rejected["command"], "parse");
    assert_eq!(rejected["success"], false);
    assert_eq!(
        rejected["error"],
        "Daemon commands require protocol 7 or newer"
    );

    // Empty list: no live sessions.
    client.send_command("l1", serde_json::json!({ "type": "list" }));
    let list = client.read_response("l1");
    assert_eq!(list["success"], true, "list failed: {list}");
    assert_eq!(list["data"]["sessions"], serde_json::json!([]));

    // Create a scripted session.
    let script_path = dir.path().join("script.json");
    std::fs::write(
        &script_path,
        serde_json::json!({ "responses": [
            { "text": "hello from scripted", "delayMs": 30 },
            { "text": "second turn" },
        ] })
        .to_string(),
    )
    .expect("write script");
    client.send_command(
        "c1",
        serde_json::json!({
            "type": "create",
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": agent_dir.join("sessions").to_string_lossy(),
                "script": script_path.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response("c1");
    assert_eq!(created["success"], true, "create failed: {created}");
    let session_id = created["data"]["id"]
        .as_str()
        .or_else(|| created["data"]["sessionId"].as_str())
        .expect("session id in create response")
        .to_string();

    // Attach and stream the first turn.
    client.send_command(
        "a1",
        serde_json::json!({ "type": "attach", "activeSessionId": session_id }),
    );
    let attached = client.read_response("a1");
    assert_eq!(attached["success"], true, "attach failed: {attached}");
    // Attach wire shape (differential goldens from the TS supervisor): slim
    // attach carries summary/messages only inside the snapshot, no
    // `session_attached` convenience event precedes the response.
    let data = &attached["data"];
    let keys: Vec<&str> = data
        .as_object()
        .expect("attach data object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "activeSessionId",
            "client",
            "lastEventCursor",
            "lastEventSequence",
            "protocol",
            "replay",
            "snapshot",
        ]
    );
    let snapshot = &data["snapshot"];
    let snapshot_keys: Vec<&str> = snapshot
        .as_object()
        .expect("snapshot object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        snapshot_keys,
        vec![
            "activeSessionId",
            "children",
            "lastEventCursor",
            "lastEventSequence",
            "messages",
            "state",
            "summary",
        ]
    );
    assert_eq!(snapshot["children"], serde_json::json!([]));
    assert_eq!(
        data["client"]["capabilities"],
        serde_json::json!(["attach_snapshot", "event_sequence", "slim_attach"])
    );
    assert_eq!(data["replay"]["status"], "complete");

    client.send_command(
        "p1",
        serde_json::json!({ "type": "prompt", "activeSessionId": session_id, "message": "hi" }),
    );
    let prompt_ack = client.read_response("p1");
    assert_eq!(prompt_ack["success"], true, "prompt failed: {prompt_ack}");

    // Streamed session events: message_start, updates, message_end, turn_end.
    let mut saw_start = false;
    let mut updates = 0usize;
    let mut final_text = String::new();
    loop {
        let line = client.read_line();
        if line["type"] == "session_event" {
            let event = &line["event"];
            match event["type"].as_str() {
                Some("message_start") => saw_start = true,
                Some("message_update") => updates += 1,
                Some("message_end") => {
                    // The scripted engine emits plain-string content.
                    final_text = event["message"]["content"]
                        .as_str()
                        .expect("final text")
                        .to_string();
                }
                Some("turn_end") => break,
                _ => {}
            }
        }
    }
    assert!(saw_start, "message_start streamed");
    assert!(updates > 0, "assistant updates streamed ({updates} seen)");
    assert_eq!(final_text, "hello from scripted");

    // The final answer is queryable.
    client.send_command(
        "g1",
        serde_json::json!({
            "type": "get_last_assistant_text",
            "activeSessionId": session_id,
        }),
    );
    let last = client.read_response("g1");
    assert_eq!(
        last["success"], true,
        "get_last_assistant_text failed: {last}"
    );
    assert_eq!(last["data"]["text"], "hello from scripted");

    // The session appears in list.
    client.send_command("l2", serde_json::json!({ "type": "list" }));
    let list = client.read_response("l2");
    let sessions = list["data"]["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], session_id.as_str());
    // Session-summary fields match the TS `SessionSummary` wire shape.
    assert_eq!(sessions[0]["runtimeKind"], "top-level");
    assert_eq!(sessions[0]["rlmDepth"], 0);
    assert_eq!(sessions[0]["unfinishedActionCount"], 0);
    assert!(sessions[0]["modified"]
        .as_str()
        .is_some_and(|v| v.ends_with('Z')));
    assert!(sessions[0]["lastActivityAt"]
        .as_str()
        .is_some_and(|v| v.ends_with('Z')));
    // Usage from the scripted turn: input tokens and cost, zero total absent.
    let usage = &sessions[0]["usage"];
    assert!(usage["inputTokens"].as_u64().unwrap_or_default() > 0);
    assert!(usage["outputTokens"].as_u64().unwrap_or_default() > 0);
    assert!(usage["cost"].as_f64().unwrap_or_default() >= 0.0);

    // Saved-session listing: item + progress events, then the final response
    // (differential shape from the TS supervisor's `handleSavedSessionList`).
    client.send_command("e1", serde_json::json!({ "type": "list_saved_sessions" }));
    let rejected = client.read_response("e1");
    assert_eq!(rejected["success"], false);
    assert_eq!(
        rejected["error"],
        "The \"paths[0]\" property must be of type string, got undefined"
    );
    client.send_command(
        "sl1",
        serde_json::json!({
            "type": "list_saved_sessions",
            "cwd": dir.path().to_string_lossy(),
            "sessionDir": agent_dir.join("sessions").to_string_lossy(),
            "scope": "all",
        }),
    );
    let mut items = 0usize;
    let mut progress = 0usize;
    let mut rows = Vec::new();
    let saved = loop {
        let line = client.read_line();
        match line["type"].as_str() {
            Some("session_list_item") => {
                items += 1;
                let session = line["session"].clone();
                assert!(session["path"]
                    .as_str()
                    .unwrap_or_default()
                    .ends_with(".jsonl"));
                assert!(session["firstMessage"].is_string());
                assert!(session["state"]["status"].is_string());
                rows.push(session);
            }
            Some("session_list_progress") => {
                progress += 1;
                assert!(line["loaded"].as_u64().unwrap_or_default() > 0);
                assert!(
                    line["total"].as_u64().unwrap_or_default()
                        >= line["loaded"].as_u64().unwrap_or_default()
                );
            }
            _ if line["id"] == "sl1" => break line,
            _ => {}
        }
    };
    assert_eq!(
        saved["success"], true,
        "list_saved_sessions failed: {saved}"
    );
    assert_eq!(items, 1, "expected exactly the one created session");
    assert!(progress >= 1);
    let sessions = saved["data"]["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), items);
    assert_eq!(rows[0], sessions[0]);

    // Second turn of the script replays the next response.
    client.send_command(
        "p2",
        serde_json::json!({
            "type": "prompt_and_wait",
            "activeSessionId": session_id,
            "message": "again",
        }),
    );
    let done = client.read_response("p2");
    assert_eq!(done["success"], true, "prompt_and_wait failed: {done}");
    client.send_command(
        "g2",
        serde_json::json!({
            "type": "get_last_assistant_text",
            "activeSessionId": session_id,
        }),
    );
    let last = client.read_response("g2");
    assert_eq!(last["data"]["text"], "second turn");
}
// Session-read commands over the persisted branch: differential goldens
// captured from the live TS daemon (protocol 7, schema 28, read-only
// `get_session_header` / `get_session_stats` against a live session).
#[test]
fn session_stats_and_header_match_live_daemon_goldens() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let _daemon = spawn_daemon(&socket, &agent_dir);
    let (mut client, _hello) = Client::connect(&socket);

    let script_path = dir.path().join("script.json");
    std::fs::write(
        &script_path,
        serde_json::json!({ "responses": [{ "text": "hi", "delayMs": 0 }] }).to_string(),
    )
    .expect("write script");
    client.send_command(
        "c1",
        serde_json::json!({
            "type": "create",
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": agent_dir.join("sessions").to_string_lossy(),
                "script": script_path.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response("c1");
    assert_eq!(created["success"], true, "create failed: {created}");
    // `id` is the short display/selector id; `sessionId` is the persisted
    // session UUID (what `get_session_header` / `get_session_stats` report).
    let session_id = created["data"]["id"]
        .as_str()
        .or_else(|| created["data"]["sessionId"].as_str())
        .expect("session id in create response")
        .to_string();
    let session_uuid = created["data"]["sessionId"]
        .as_str()
        .expect("sessionId in create response")
        .to_string();

    // Attach like the lifecycle test: the turn's streamed events go to
    // attached clients only.
    client.send_command(
        "a1",
        serde_json::json!({ "type": "attach", "activeSessionId": session_id }),
    );
    let attached = client.read_response("a1");
    assert_eq!(attached["success"], true, "attach failed: {attached}");

    client.send_command(
        "p1",
        serde_json::json!({ "type": "prompt", "activeSessionId": session_id, "message": "hi" }),
    );
    let ack = client.read_response("p1");
    assert_eq!(ack["success"], true, "prompt failed: {ack}");
    // Drain the streamed turn until it settles.
    loop {
        let line = client.read_line();
        if line["type"] == "session_event" && line["event"]["type"].as_str() == Some("turn_end") {
            break;
        }
    }

    // get_session_header: same key set and header shape as the TS golden:
    // {"header": { type, version, id, timestamp, cwd, parentSession?, rlmDepth?, git? }}.
    client.send_command(
        "h1",
        serde_json::json!({ "type": "get_session_header", "activeSessionId": session_id }),
    );
    let header = client.read_response("h1");
    assert_eq!(
        header["success"], true,
        "get_session_header failed: {header}"
    );
    let header = &header["data"]["header"];
    assert_eq!(header["type"], "session");
    assert_eq!(header["version"], 3);
    assert_eq!(header["id"], session_uuid.as_str());
    assert_eq!(header["cwd"], dir.path().to_string_lossy().to_string());
    assert!(header["timestamp"]
        .as_str()
        .is_some_and(|v| v.ends_with('Z')));
    let header_keys: Vec<&str> = header
        .as_object()
        .expect("header object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        header_keys,
        vec!["cwd", "id", "rlmDepth", "timestamp", "type", "version"]
    );

    // get_session_stats: the TS stats shape over the scripted turn. The
    // scripted engine has no model, so `contextUsage` is omitted exactly like
    // a TS session without a model context window.
    client.send_command(
        "st1",
        serde_json::json!({ "type": "get_session_stats", "activeSessionId": session_id }),
    );
    let stats = client.read_response("st1");
    assert_eq!(stats["success"], true, "get_session_stats failed: {stats}");
    let data = &stats["data"];
    assert_eq!(data["sessionId"], session_uuid.as_str());
    assert!(data["sessionFile"]
        .as_str()
        .is_some_and(|path| path.ends_with(".jsonl")));
    assert_eq!(data["userMessages"], 1);
    assert_eq!(data["assistantMessages"], 1);
    assert_eq!(data["toolCalls"], 0);
    assert_eq!(data["toolResults"], 0);
    assert_eq!(data["totalMessages"], 2);
    assert_eq!(data["cost"], 0.0);
    // Scripted usage block: input 120, output 8.
    assert_eq!(data["tokens"]["input"], 120);
    assert_eq!(data["tokens"]["output"], 8);
    assert_eq!(data["tokens"]["cacheRead"], 0);
    assert_eq!(data["tokens"]["cacheWrite"], 0);
    assert_eq!(data["tokens"]["total"], 128);
    let stats_keys: Vec<&str> = data
        .as_object()
        .expect("stats object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        stats_keys,
        vec![
            "assistantMessages",
            "cost",
            "sessionFile",
            "sessionId",
            "tokens",
            "toolCalls",
            "toolResults",
            "totalMessages",
            "userMessages",
        ]
    );

    // Unknown active session selector fails with the TS error string.
    client.send_command(
        "h2",
        serde_json::json!({ "type": "get_session_stats", "activeSessionId": "nope" }),
    );
    let missing = client.read_response("h2");
    assert_eq!(missing["success"], false);
    assert_eq!(missing["error"], "Unknown active session: nope");
}

/// Pids whose parent is `ppid` (the supervisor's live worker children).
fn child_pids_of(ppid: u32) -> Vec<u32> {
    let mut pids = Vec::new();
    let entries = std::fs::read_dir("/proc").expect("read /proc");
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // `comm` can contain spaces and parens, so parse after the last ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        fields.next(); // process state
        let Ok(parent) = fields.next().unwrap_or_default().parse::<u32>() else {
            continue;
        };
        if parent == ppid {
            pids.push(pid);
        }
    }
    pids
}

/// Liveness that ignores zombies: a detached child nobody reaps keeps its
/// `/proc` entry (exit status pending), so path existence alone would call
/// an exited process alive.
fn process_alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // `comm` can contain spaces and parens, so parse after the last ')'.
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    let state = rest.split_whitespace().next().unwrap_or_default();
    !state.starts_with('Z') && !state.starts_with('X')
}

/// Wait for the child to exit by itself within `timeout` (no kill).
fn wait_child_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return Some(status);
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The shutdown command must stop every worker and exit the supervisor
/// process itself, cleaning up its socket (the CLI's stale-replacement and
/// shutdown paths wait for the daemon to be gone; a supervisor that stays
/// parked on its listening socket would block replacement forever and leak
/// both processes).
#[test]
fn shutdown_command_exits_the_supervisor_process() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(agent_dir.join("sessions")).expect("sessions dir");
    let mut daemon = spawn_daemon(&socket, &agent_dir);
    let supervisor_pid = daemon.child.id();
    let (mut client, _hello) = Client::connect(&socket);

    // A live session so a worker process exists when shutdown arrives.
    let script_path = dir.path().join("script.json");
    std::fs::write(
        &script_path,
        serde_json::json!({ "responses": [ { "text": "x" } ] }).to_string(),
    )
    .expect("write script");
    client.send_command(
        "c1",
        serde_json::json!({
            "type": "create",
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": agent_dir.join("sessions").to_string_lossy(),
                "script": script_path.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response("c1");
    assert_eq!(created["success"], true, "create failed: {created}");
    // The supervisor spawned exactly one worker child for the session.
    let deadline = Instant::now() + Duration::from_secs(10);
    let worker_pids = loop {
        let children = child_pids_of(supervisor_pid);
        if !children.is_empty() {
            break children;
        }
        assert!(Instant::now() < deadline, "worker never spawned");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(worker_pids.len(), 1, "one worker per session");

    client.send_command("sd", serde_json::json!({ "type": "shutdown" }));
    let shutdown = client.read_response("sd");
    assert_eq!(shutdown["success"], true, "shutdown failed: {shutdown}");

    // The supervisor exits on its own, cleanly, and takes the socket file.
    let exit = wait_child_exit(&mut daemon.child, Duration::from_secs(10))
        .expect("the supervisor process exited after shutdown");
    assert!(exit.success(), "supervisor exit: {exit:?}");
    assert!(!socket.exists(), "the socket file is removed on exit");

    // No worker process outlives the shutdown.
    let deadline = Instant::now() + Duration::from_secs(10);
    for pid in worker_pids {
        while process_alive(pid) {
            assert!(
                Instant::now() < deadline,
                "worker {pid} leaked after shutdown"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

// Side questions end to end: `start_side_question`/`abort_side_question` over
// the scripted engine, events routed back to the owner client
// (TS daemon-mode handlers + `core/side-question.ts`).
#[test]
fn side_questions_start_abort_and_events_scripted() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let _daemon = spawn_daemon(&socket, &agent_dir);
    let (mut client, _hello) = Client::connect(&socket);

    // Create a scripted session whose side-question script fails once
    // transiently (retried with fast delays), then answers after a delay long
    // enough to observe the in-flight guards and the abort.
    let script_path = dir.path().join("script.json");
    std::fs::write(
        &script_path,
        serde_json::json!({
            "responses": [],
            "sideQuestion": {
                "responses": [
                    { "error": "stream failed once", "kind": "server_error", "status": 500 },
                    { "text": "the side answer", "delayMs": 1500 },
                ],
                "retry": {
                    "enabled": true, "maxRetries": 2,
                    "baseDelayMs": 5, "maxRetryDelayMs": 1000,
                },
            },
        })
        .to_string(),
    )
    .expect("write script");
    client.send_command(
        "c1",
        serde_json::json!({
            "type": "create",
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": agent_dir.join("sessions").to_string_lossy(),
                "script": script_path.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response("c1");
    assert_eq!(created["success"], true, "create failed: {created}");
    let session_id = created["data"]["id"]
        .as_str()
        .or_else(|| created["data"]["sessionId"].as_str())
        .expect("session id")
        .to_string();

    // Attach first: the supervisor fans worker frames (including
    // `side_question_event`) out to clients attached to the session.
    client.send_command(
        "a1",
        serde_json::json!({ "type": "attach", "activeSessionId": session_id }),
    );
    let attached = client.read_response("a1");
    assert_eq!(attached["success"], true, "attach failed: {attached}");

    // Unknown session fails with the TS routing error.
    client.send_command(
        "sq-missing",
        serde_json::json!({
            "type": "start_side_question",
            "activeSessionId": "no-such-session",
            "sideQuestionId": "q0",
            "question": "hi?",
        }),
    );
    let missing = client.read_response("sq-missing");
    assert_eq!(missing["success"], false);
    assert_eq!(missing["error"], "Unknown active session: no-such-session");

    // Start a side question; the response acknowledges immediately.
    client.send_command(
        "sq1",
        serde_json::json!({
            "type": "start_side_question",
            "activeSessionId": session_id,
            "sideQuestionId": "q1",
            "question": "what is the answer?",
            "previousTurns": [],
        }),
    );
    let started = client.read_response("sq1");
    assert_eq!(started["success"], true, "start failed: {started}");

    // The retry played out before the partial answer: the failure was
    // transient and the second provider attempt answers.
    let mut running_answers: Vec<String> = Vec::new();
    let partial_answer = loop {
        let line = client.read_line();
        if line["type"] != "side_question_event" {
            continue;
        }
        let event = &line["event"];
        assert_eq!(line["activeSessionId"], serde_json::json!(session_id));
        assert_eq!(event["id"], serde_json::json!("q1"));
        assert_eq!(event["question"], serde_json::json!("what is the answer?"));
        assert_eq!(event["status"], serde_json::json!("running"));
        running_answers.push(event["answer"].as_str().expect("answer").to_string());
        if event["answer"] == serde_json::json!("the side answer") {
            break event["answer"].clone();
        }
    };
    assert_eq!(running_answers[0], "", "first running event is empty");

    // While the run is in flight (inside the scripted delay), the TS guards
    // hold: duplicate ids are rejected, and one run per client per session.
    client.send_command(
        "sq-dup",
        serde_json::json!({
            "type": "start_side_question",
            "activeSessionId": session_id,
            "sideQuestionId": "q1",
            "question": "same id?",
        }),
    );
    let duplicate = client.read_response("sq-dup");
    assert_eq!(duplicate["success"], false);
    assert_eq!(
        duplicate["error"], "Side question already exists: q1",
        "duplicate: {duplicate}"
    );
    client.send_command(
        "sq-busy",
        serde_json::json!({
            "type": "start_side_question",
            "activeSessionId": session_id,
            "sideQuestionId": "q2",
            "question": "second?",
        }),
    );
    let busy = client.read_response("sq-busy");
    assert_eq!(busy["success"], false);
    assert_eq!(
        busy["error"],
        "A side question is already running for this client and session"
    );

    // Aborting an unknown id reports { aborted: false }.
    client.send_command(
        "ab-unknown",
        serde_json::json!({
            "type": "abort_side_question",
            "activeSessionId": session_id,
            "sideQuestionId": "never-started",
        }),
    );
    let aborted = client.read_response("ab-unknown");
    assert_eq!(aborted["success"], true, "abort failed: {aborted}");
    assert_eq!(aborted["data"], serde_json::json!({ "aborted": false }));

    // Abort the live run: { aborted: true }, then a cancelled event carrying
    // the partial answer streamed so far.
    client.send_command(
        "ab1",
        serde_json::json!({
            "type": "abort_side_question",
            "activeSessionId": session_id,
            "sideQuestionId": "q1",
        }),
    );
    let aborted = client.read_response("ab1");
    assert_eq!(aborted["success"], true, "abort failed: {aborted}");
    assert_eq!(aborted["data"], serde_json::json!({ "aborted": true }));
    let cancelled = loop {
        let line = client.read_line();
        if line["type"] == "side_question_event"
            && line["event"]["id"] == serde_json::json!("q1")
            && line["event"]["status"] == serde_json::json!("cancelled")
        {
            break line["event"].clone();
        }
    };
    assert_eq!(cancelled["answer"], partial_answer);

    // A cancelled run is gone: the same id starts again and this time
    // completes (the script replays from the top, fresh conversation).
    client.send_command(
        "sq2",
        serde_json::json!({
            "type": "start_side_question",
            "activeSessionId": session_id,
            "sideQuestionId": "q1",
            "question": "what is the answer?",
        }),
    );
    let restarted = client.read_response("sq2");
    assert_eq!(restarted["success"], true, "restart failed: {restarted}");
    let completed = loop {
        let line = client.read_line();
        if line["type"] == "side_question_event"
            && line["event"]["id"] == serde_json::json!("q1")
            && line["event"]["status"] == serde_json::json!("complete")
        {
            break line["event"].clone();
        }
    };
    assert_eq!(completed["answer"], serde_json::json!("the side answer"));
    assert!(completed.get("errorMessage").is_none());
}
