//! The passive-RLM roster walk at scale: a synthetic ledger with one root
//! and 1,000 spawned children (each with a persisted session file in the
//! artifacts tree, none resident) must surface the full 1,001-row roster
//! from `list --all`, performance-bounded (TS: 1,001 rows in 0.78s).
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// How long `list --all` over 1,000 ledger children may take: the TS daemon
/// answers the same shape in 0.78s; the bound stays generous for CI noise.
const LIST_ALL_BOUND: Duration = Duration::from_secs(10);

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

#[allow(clippy::zombie_processes)]
fn spawn_daemon(socket: &Path, agent_dir: &Path) -> Daemon {
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
    fn connect(socket: &Path) -> (Self, Value) {
        let stream = UnixStream::connect(socket).expect("connect supervisor");
        let write_stream = stream.try_clone().expect("clone socket");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer: write_stream,
        };
        let hello = client.read_line();
        (client, hello)
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .expect("read supervisor line");
        assert!(!line.trim().is_empty(), "supervisor closed the connection");
        serde_json::from_str(line.trim()).expect("parse supervisor line")
    }

    fn send_command(&mut self, id: &str, command: Value) -> Value {
        let envelope = json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        });
        let mut line = serde_json::to_string(&envelope).expect("serialize command");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("send");
        self.writer.flush().expect("flush");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(Instant::now() < deadline, "no response for id {id}");
            let response = self.read_line();
            if response.get("id").and_then(Value::as_str) == Some(id) {
                return response;
            }
        }
    }
}

/// One persisted child session file: a header plus two messages, laid out
/// the way a spawned child persists (per-child dir under the parent's
/// session-artifacts tree).
fn write_child_session(path: &Path, id: &str, name: &str, prompt: &str) {
    std::fs::create_dir_all(path.parent().expect("child dir")).expect("child dir");
    let content = format!(
        "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-09-18T00:00:00.000Z\",\"cwd\":\"/tmp\",\"parentSession\":\"/parent/file.jsonl\",\"rlmDepth\":1}}\n\
         {{\"type\":\"session_info\",\"id\":\"i1\",\"timestamp\":\"2026-09-18T00:00:01.000Z\",\"name\":\"{name}\"}}\n\
         {{\"type\":\"message\",\"id\":\"m1\",\"timestamp\":\"2026-09-18T00:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"{prompt}\",\"timestamp\":1}}}}\n\
         {{\"type\":\"message\",\"id\":\"m2\",\"timestamp\":\"2026-09-18T00:00:02.000Z\",\"message\":{{\"role\":\"assistant\",\"content\":\"child answer\",\"timestamp\":2}}}}\n"
    );
    std::fs::write(path, content).expect("write child session");
}

/// Build one synthetic family: a root session in the sessions dir plus
/// `children` ledger spawn records pointing at persisted (non-resident)
/// child files, and write the ledger file at its canonical path.
fn write_synthetic_family(agent_dir: &Path, children: usize) -> (PathBuf, usize) {
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let root_id = "root-session-1";
    let root_path = sessions_dir.join(format!("{root_id}.jsonl"));
    std::fs::write(
        &root_path,
        format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"{root_id}\",\"timestamp\":\"2026-09-18T00:00:00.000Z\",\"cwd\":\"/tmp\"}}\n\
             {{\"type\":\"message\",\"id\":\"m1\",\"timestamp\":\"2026-09-18T00:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"root task\",\"timestamp\":1}}}}\n"
        ),
    )
    .expect("write root session");

    let mut records = String::new();
    let mut live = 0;
    for index in 0..children {
        let child_id = format!("sub-{index:04}");
        let child_path = agent_dir
            .join("session-artifacts")
            .join(root_id)
            .join(&child_id)
            .join(format!("{child_id}.jsonl"));
        write_child_session(
            &child_path,
            &child_id,
            &format!("worker-{:04}", index),
            &format!("child task {index}"),
        );
        records.push_str(
            &json!({
                "v": 1,
                "op": "spawn",
                "at": "2026-09-18T00:00:03.000Z",
                "childId": child_id,
                "parent": root_path.to_string_lossy(),
                "child": child_path.to_string_lossy(),
                "depth": 1,
                "name": format!("worker-{:04}", index),
            })
            .to_string(),
        );
        records.push('\n');
        live += 1;
    }
    let ledger_path = pa_daemon::rlm_ledger::rlm_ledger_path(agent_dir, &sessions_dir);
    std::fs::create_dir_all(ledger_path.parent().expect("ledger dir")).expect("ledger dir");
    let payload = format!(
        "{{\"v\":1,\"op\":\"meta\",\"at\":\"2026-09-18T00:00:00.000Z\",\"sessionsDir\":\"{}\"}}\n{records}",
        sessions_dir.to_string_lossy(),
    );
    std::fs::write(&ledger_path, payload).expect("write ledger");
    (root_path, live)
}

#[test]
fn list_all_returns_the_full_synthetic_thousand_child_roster() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    const CHILDREN: usize = 1_000;
    write_synthetic_family(&agent_dir, CHILDREN);
    let _daemon = spawn_daemon(&socket, &agent_dir);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");

    let started = Instant::now();
    let response = client.send_command("l1", json!({ "type": "list", "all": true }));
    let elapsed = started.elapsed();
    assert_eq!(
        response["success"], true,
        "list --all succeeded: {response}"
    );
    let sessions = response["data"]["sessions"]
        .as_array()
        .expect("list sessions array");
    assert_eq!(sessions.len(), CHILDREN + 1, "root + every ledger child");
    assert!(
        elapsed < LIST_ALL_BOUND,
        "list --all over {CHILDREN} ledger children answered in {elapsed:?}"
    );

    // The root row and one child row carry the passive-roster identity.
    let root_row = sessions
        .iter()
        .find(|row| row["sessionId"].as_str() == Some("root-session-1"))
        .expect("root row");
    assert_eq!(root_row["messageCount"], 1);
    let child_row = sessions
        .iter()
        .find(|row| row["rlmChildId"].as_str() == Some("sub-0007"))
        .expect("ledger child row");
    assert_eq!(child_row["runtimeKind"], "subagent");
    assert_eq!(child_row["sessionName"], "worker-0007");
    assert_eq!(child_row["rlmDepth"], 1);
    assert_eq!(child_row["messageCount"], 2);
    assert_eq!(
        child_row["parentSessionPath"].as_str(),
        Some(
            agent_dir
                .join("sessions")
                .join("root-session-1.jsonl")
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .as_ref()
        )
    );
}
