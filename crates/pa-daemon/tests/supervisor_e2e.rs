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
