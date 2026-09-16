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

    // Empty list: no live sessions.
    client.send(&serde_json::json!({ "type": "list", "id": "l1" }));
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
    client.send(&serde_json::json!({
        "type": "create",
        "id": "c1",
        "config": {
            "cwd": dir.path().to_string_lossy(),
            "sessionDir": agent_dir.join("sessions").to_string_lossy(),
            "script": script_path.to_string_lossy(),
        },
    }));
    let created = client.read_response("c1");
    assert_eq!(created["success"], true, "create failed: {created}");
    let session_id = created["data"]["id"]
        .as_str()
        .or_else(|| created["data"]["sessionId"].as_str())
        .expect("session id in create response")
        .to_string();

    // Attach and stream the first turn.
    client.send(&serde_json::json!({
        "type": "attach", "id": "a1", "activeSessionId": session_id,
    }));
    let attached = client.read_response("a1");
    assert_eq!(attached["success"], true, "attach failed: {attached}");

    client.send(&serde_json::json!({
        "type": "prompt", "id": "p1", "activeSessionId": session_id, "message": "hi",
    }));
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
    client.send(&serde_json::json!({
        "type": "get_last_assistant_text", "id": "g1", "activeSessionId": session_id,
    }));
    let last = client.read_response("g1");
    assert_eq!(
        last["success"], true,
        "get_last_assistant_text failed: {last}"
    );
    assert_eq!(last["data"]["text"], "hello from scripted");

    // The session appears in list.
    client.send(&serde_json::json!({ "type": "list", "id": "l2" }));
    let list = client.read_response("l2");
    let sessions = list["data"]["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], session_id.as_str());

    // Second turn of the script replays the next response.
    client.send(&serde_json::json!({
        "type": "prompt_and_wait", "id": "p2", "activeSessionId": session_id,
        "message": "again",
    }));
    let done = client.read_response("p2");
    assert_eq!(done["success"], true, "prompt_and_wait failed: {done}");
    client.send(&serde_json::json!({
        "type": "get_last_assistant_text", "id": "g2", "activeSessionId": session_id,
    }));
    let last = client.read_response("g2");
    assert_eq!(last["data"]["text"], "second turn");
}
