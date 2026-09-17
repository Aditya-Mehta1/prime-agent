//! End-to-end ACP-mode verification: the real binary speaks the ACP
//! JSON-RPC surface over stdio, driven by the scripted faux provider, and
//! the emitted frames are checked against the TS capture corpus
//! (`crates/pa-daemon/testdata/acp`).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The child plus the tempdir it runs in: the tempdir must outlive the
/// child process (its cwd), so it is held on the struct.
struct AcpChild {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
    /// Held (never read) so the child's cwd directory outlives the process:
    /// dropping the tempdir deletes it and the child's current_dir fails.
    _home: tempfile::TempDir,
    spawn_stderr: Option<std::process::ChildStderr>,
}

impl AcpChild {
    fn spawn(args: &[&str], script: &serde_json::Value) -> AcpChild {
        let home = tempfile::TempDir::new().unwrap();
        let bin = env!("CARGO_BIN_EXE_prime-agent");
        let mut child = Command::new(bin)
            .args(args)
            .env("HOME", home.path())
            .env("PRIME_AGENT_AGENT_DIR", home.path().join("agent"))
            .env("PRIME_AGENT_FAUX_SCRIPT", script.to_string())
            .current_dir(home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("binary present");
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        AcpChild {
            child,
            stdin,
            lines,
            next_id: 0,
            _home: home,
            spawn_stderr: Some(stderr),
        }
    }

    fn send(&mut self, frame: Value) {
        let mut line = serde_json::to_string(&frame).unwrap();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Read frames until the request `id` answers; returns the answer with
    /// the notifications seen before it, in order.
    fn wait_response(&mut self, id: u64, timeout: Duration) -> (Value, Vec<Value>) {
        let deadline = Instant::now() + timeout;
        let mut notifications = Vec::new();
        loop {
            let timeout_left = deadline.saturating_duration_since(Instant::now());
            if timeout_left.is_zero() {
                panic!("timed out waiting for response {id}");
            }
            match self.lines.recv_timeout(timeout_left) {
                Ok(line) => {
                    let frame: Value = serde_json::from_str(&line).expect("valid JSON line");
                    if frame.get("id").and_then(Value::as_u64) == Some(id)
                        && (frame.get("result").is_some() || frame.get("error").is_some())
                    {
                        return (frame, notifications);
                    }
                    notifications.push(frame);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for response {id}")
                }
                Err(_) => panic!("ACP server closed stdout"),
            }
        }
    }
}

impl Drop for AcpChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(mut stderr) = self.spawn_stderr.take() {
            use std::io::Read;
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            if !text.is_empty() {
                eprintln!("ACP child stderr: {text}");
            }
        }
    }
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {},
        "clientInfo": { "name": "acp-e2e", "title": "ACP E2E", "version": "0.0.1" },
    })
}

const TIMEOUT: Duration = Duration::from_secs(60);

/// The TS initialize response shape (capture `ts-happy_path.jsonl`), with the
/// version and sessionId-class fields normalized as volatile.
#[test]
fn acp_initialize_matches_the_ts_golden() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let id = client.request("initialize", initialize_params());
    let (response, notifications) = client.wait_response(id, TIMEOUT);
    assert!(notifications.is_empty(), "nothing precedes initialize");
    let result = &response["result"];
    assert_eq!(result["protocolVersion"], 1);
    let capabilities = &result["agentCapabilities"];
    assert_eq!(capabilities["loadSession"], false);
    assert_eq!(
        capabilities["promptCapabilities"],
        json!({ "image": true, "embeddedContext": true })
    );
    assert_eq!(capabilities["sessionCapabilities"], json!({ "close": {} }));
    // The in-process slice does not serve ACP MCP servers, so the TS
    // daemon-path `mcpCapabilities` flag is intentionally absent.
    assert!(capabilities.get("mcpCapabilities").is_none());
    let info = &result["agentInfo"];
    assert_eq!(info["name"], "prime-agent");
    assert_eq!(info["title"], "Prime Agent");
    assert_eq!(
        result["_meta"],
        json!({ "ai.primeintellect.prime-agent": {} })
    );
}

#[test]
fn acp_second_initialize_is_served() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp"], &script);
    let first = client.request("initialize", initialize_params());
    let _ = client.wait_response(first, TIMEOUT);
    let second = client.request("initialize", initialize_params());
    let (response, _) = client.wait_response(second, TIMEOUT);
    assert_eq!(response["result"]["protocolVersion"], 1);
}

#[test]
fn acp_prompt_stream_completion_envelope_and_stop_reason_match_ts() {
    let script = json!({ "responses": ["ACP-OK"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", json!({ "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let prompt = client.request(
        "session/prompt",
        json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": "Reply with exactly: ACP-OK" }] }),
    );
    let (prompt_response, updates) = client.wait_response(prompt, TIMEOUT);

    // Frame shape sequence from ts-happy_path.jsonl: the chunk stream, the
    // response boundary, the completion event, the terminal envelope, and
    // then the response. The faux provider emits its text in one chunk.
    let mut shapes = Vec::new();
    for update in &updates {
        let body = &update["params"]["update"];
        let meta = &body["_meta"]["ai.primeintellect.prime-agent"];
        shapes.push((
            body["sessionUpdate"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            meta["phase"].as_str().unwrap_or_default().to_string(),
            meta["outcome"].as_str().map(str::to_string),
            meta["terminalQuiescenceExpected"].as_bool(),
        ));
    }
    let boundary = (
        "session_info_update".to_string(),
        "responseBoundary".to_string(),
        Some("result".to_string()),
        Some(true),
    );
    let completion = (
        "session_info_update".to_string(),
        "event".to_string(),
        None,
        None,
    );
    let terminal = (
        "session_info_update".to_string(),
        "terminalQuiescence".to_string(),
        Some("result".to_string()),
        None,
    );
    assert_eq!(
        shapes.first().map(|(tag, _, _, _)| tag.clone()),
        Some("agent_message_chunk".to_string())
    );
    assert!(shapes.contains(&boundary), "shapes: {shapes:?}");
    assert!(shapes.contains(&completion), "shapes: {shapes:?}");
    assert!(shapes.contains(&terminal), "shapes: {shapes:?}");
    assert_eq!(shapes.last(), Some(&terminal));

    assert_eq!(
        prompt_response["result"],
        json!({ "stopReason": "end_turn" })
    );

    // Sequences are strictly increasing across the whole turn.
    let mut sequences: Vec<u64> = Vec::new();
    for update in &updates {
        sequences.push(
            update["params"]["update"]["_meta"]["ai.primeintellect.prime-agent"]["eventSequence"]
                .as_u64()
                .unwrap(),
        );
    }
    let mut sorted = sequences.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sequences, sorted, "eventSequence strictly increases");

    let close = client.request("session/close", json!({ "sessionId": session_id }));
    let (close_response, _) = client.wait_response(close, TIMEOUT);
    assert_eq!(close_response["result"], json!({}));
}

#[test]
fn acp_prompt_chunk_carries_the_assistant_message_id() {
    let script = json!({ "responses": ["ACP-OK"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", json!({ "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let prompt = client.request(
        "session/prompt",
        json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": "hello" }] }),
    );
    let (_, updates) = client.wait_response(prompt, TIMEOUT);
    let chunk = updates
        .iter()
        .find(|update| update["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
        .expect("a message chunk");
    assert_eq!(
        chunk["params"]["update"]["messageId"],
        "prime-agent-assistant-1"
    );
    assert_eq!(
        chunk["params"]["update"]["content"],
        json!({ "type": "text", "text": "ACP-OK" })
    );
    assert_eq!(
        chunk["params"]["update"]["_meta"]["ai.primeintellect.prime-agent"],
        json!({ "promptTurnId": 1, "eventSequence": 1, "phase": "event" })
    );
}

#[test]
fn acp_cwd_mismatch_is_reported_not_adopted() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", json!({ "cwd": "/tmp", "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let meta = &new_response["result"]["_meta"]["ai.primeintellect.prime-agent"]["cwd"];
    assert_eq!(meta["requested"], "/tmp");
    // The actual cwd is the temp dir the client runs in; only the mismatch
    // shape is asserted here (the value is tempdir-random).
    assert!(meta["actual"]
        .as_str()
        .is_some_and(|actual| actual.starts_with(std::path::MAIN_SEPARATOR)));
}

#[test]
fn acp_error_shapes_match_the_ts_goldens() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", initialize_params());
    let _ = client.wait_response(init, TIMEOUT);

    // Unknown session (ts-errors.jsonl): -32603 with the details string.
    let prompt = client.request(
        "session/prompt",
        json!({ "sessionId": "bogus-session", "prompt": [{ "type": "text", "text": "hi" }] }),
    );
    let (response, _) = client.wait_response(prompt, TIMEOUT);
    assert_eq!(response["error"]["code"], -32603);
    assert_eq!(response["error"]["message"], "Internal error");
    assert_eq!(
        response["error"]["data"]["details"],
        "Unknown ACP session: bogus-session"
    );

    let close = client.request("session/close", json!({ "sessionId": "bogus-session" }));
    let (response, _) = client.wait_response(close, TIMEOUT);
    assert_eq!(
        response["error"]["data"]["details"],
        "Unknown ACP session: bogus-session"
    );

    let new = client.request("session/new", json!({ "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    // Second session/new on a live connection (ts-errors.jsonl).
    let again = client.request("session/new", json!({ "mcpServers": [] }));
    let (response, _) = client.wait_response(again, TIMEOUT);
    assert_eq!(response["error"]["code"], -32603);
    assert_eq!(
        response["error"]["data"]["details"],
        "prime-agent ACP mode hosts one session per connection; start another prime-agent process for a second session"
    );

    // Unknown method (ts-errors.jsonl): -32601 with the observed message.
    let unknown = client.request("unknown/method", json!({}));
    let (response, _) = client.wait_response(unknown, TIMEOUT);
    assert_eq!(response["error"]["code"], -32601);
    assert_eq!(
        response["error"]["message"],
        "\"Method not found\": unknown/method"
    );
    assert_eq!(response["error"]["data"]["method"], "unknown/method");

    let close = client.request("session/close", json!({ "sessionId": session_id }));
    let (response, _) = client.wait_response(close, TIMEOUT);
    assert_eq!(response["result"], json!({}));
}

#[test]
fn acp_initialize_with_string_protocol_version_is_invalid_params() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let id = client.request(
        "initialize",
        json!({ "protocolVersion": "1", "clientCapabilities": {} }),
    );
    let (response, _) = client.wait_response(id, TIMEOUT);
    assert_eq!(response["error"]["code"], -32602);
    assert_eq!(response["error"]["message"], "Invalid params");
    assert_eq!(
        response["error"]["data"]["protocolVersion"]["_errors"][0],
        "Invalid input: expected number, received string"
    );
}

#[test]
fn acp_image_block_without_mime_type_is_invalid_params() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", json!({ "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let prompt = client.request(
        "session/prompt",
        json!({ "sessionId": session_id, "prompt": [{ "type": "image", "data": "AAAA" }] }),
    );
    let (response, _) = client.wait_response(prompt, TIMEOUT);
    assert_eq!(response["error"]["code"], -32602);
    assert_eq!(response["error"]["message"], "Invalid params");
    assert_eq!(
        response["error"]["data"]["reason"],
        "image block requires base64 `data` and `mimeType` strings"
    );
}

#[test]
fn acp_cancel_without_an_active_turn_is_a_noop() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", json!({ "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    client.notify("session/cancel", json!({ "sessionId": session_id }));
    // The no-op cancel answers nothing; the session still closes cleanly.
    let close = client.request("session/close", json!({ "sessionId": session_id }));
    let (close_response, notifications) = client.wait_response(close, TIMEOUT);
    assert!(notifications.is_empty(), "a no-op cancel publishes nothing");
    assert_eq!(close_response["result"], json!({}));
}

#[test]
fn acp_mcp_servers_are_rejected_until_the_slice_serves_them() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request(
        "session/new",
        json!({ "mcpServers": [{ "type": "stdio", "command": "echo", "args": [] }] }),
    );
    let (response, _) = client.wait_response(new, TIMEOUT);
    assert_eq!(response["error"]["code"], -32602);
    assert_eq!(
        response["error"]["data"]["reason"],
        "MCP servers are unavailable in this ACP host"
    );
}
