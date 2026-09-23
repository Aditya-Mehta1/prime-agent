//! Agent-message family e2e: parent-to-child sends deliver, by every
//! identifier form (name, RLM child id, persisted session id), and the
//! child replies back to its parent.
//!
//! One real supervisor, one real parent worker session (the reply target),
//! and one real RLM child spawned through a `SupervisorChildSessions`
//! registry bound to the parent (the same registry `rlm.list_subagents`
//! and the worker's own controller read). The parent-side sends go through
//! the real kernel host handler (`agent_message.send` with
//! receiver_role/receiver_name), resolving through the controller's
//! family view and delivering over the supervisor route; the child is a
//! real worker with a scripted engine whose kernel answers each delivered
//! prompt with a real `agent_message.send` addressed to its parent.
//!
//! Linux-only e2e (AF_UNIX sockets), like the other pa-daemon verifiers.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pa_core::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use pa_core::session_engine::agent_messaging::{
    register_agent_message_host_handlers, AgentFamilyRelationship, AgentMessageController,
    AgentObserveController,
};
use pa_core::session_engine::rlm_host::{RlmSpawnRequest, RlmSubagentHost};
use pa_daemon::agent_messaging::{LinkAgentMessageController, LinkAgentObserveController};
use pa_daemon::rlm_children::{ParentIdentity, SupervisorChildSessions};
use pa_daemon::supervisor_link::SupervisorLink;
use serde_json::{json, Value};

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
fn spawn_supervisor(socket: &Path, agent_dir: &Path, kernel_python: &Path) -> Daemon {
    let binary = env!("CARGO_BIN_EXE_pa-daemon");
    let child = Command::new(binary)
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        .arg("--agent-dir")
        .arg(agent_dir)
        .env("PRIME_AGENT_KERNEL_PYTHON", kernel_python)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // A supervisor killed at teardown must not leak its session workers
        // into later test binaries: the worker's supervisor-lost exit (TS
        // `exitIfSupervisorOrphanedForTooLong`) runs on this short window
        // instead of the 5-minute default.
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
            "15000",
        )
        .spawn()
        .expect("spawn pa-daemon supervisor");
    Daemon {
        child,
        socket: socket.to_path_buf(),
    }
}

fn wait_socket_ready(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "supervisor socket never came up");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// JSONL supervisor client (command envelopes, id-matched responses).
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

    fn send_command(&mut self, id: &str, command: Value) {
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
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        self.reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set timeout");
        loop {
            line.clear();
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("supervisor closed the connection"),
                Ok(_) if line.trim().is_empty() => continue,
                Ok(_) => return serde_json::from_str(line.trim()).expect("parse line"),
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for supervisor line: {error}"
                    );
                }
            }
        }
    }

    fn read_response(&mut self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            assert!(Instant::now() < deadline, "no response for id {id}");
            let line = self.read_line();
            if line.get("id").and_then(Value::as_str) == Some(id) {
                return line;
            }
        }
    }

    fn wait_idle(&mut self, id: &str, active_session_id: &str) {
        self.send_command(
            id,
            json!({ "type": "wait_for_idle", "activeSessionId": active_session_id }),
        );
        let response = self.read_response(id);
        assert_eq!(
            response["success"], true,
            "wait_for_idle failed: {response}"
        );
    }

    fn messages(&mut self, id: &str, active_session_id: &str) -> String {
        self.send_command(
            id,
            json!({ "type": "get_messages", "activeSessionId": active_session_id }),
        );
        let response = self.read_response(id);
        assert_eq!(response["success"], true, "get_messages failed: {response}");
        serde_json::to_string(&response["data"]).expect("messages json")
    }
}

/// The kernel Python with the runtime installed; the child's kernel cell
/// (the parent-directed reply) needs it. Skipped (with a note) on
/// machines without a live install.
fn kernel_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_E2E_KERNEL_PYTHON") {
        let explicit = PathBuf::from(explicit);
        assert!(
            explicit.exists(),
            "PA_E2E_KERNEL_PYTHON {explicit:?} not found"
        );
        return Some(explicit);
    }
    let candidate = PathBuf::from(
        std::env::var("HOME")
            .map(|home| format!("{home}/.prime/agent/kernel-venv/bin/python"))
            .unwrap_or_else(|_| "/home/ubuntu/.prime/agent/kernel-venv/bin/python".to_string()),
    );
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!("kernel python {candidate:?} not found; skipping live family e2e");
    None
}

/// The child's reply turn: a kernel `agent_message.send` addressed to the
/// parent (no receiver name: the parent is the only Parent member), with
/// the receipt recorded on disk for the test to read.
fn child_cell(receipts_dir: &Path) -> String {
    let receipt_path = receipts_dir.join("child-reply.json").display().to_string();
    let error_path = receipts_dir.join("child-reply.error").display().to_string();
    format!(
        "from rlm import host_request\nimport json, traceback\ntry:\n    receipt = await host_request(\"agent_message.send\", {{\"message\": \"kid reply\", \"receiver_role\": \"parent\"}})\n    open({receipt_path:?}, \"w\").write(json.dumps(receipt))\nexcept Exception:\n    open({error_path:?}, \"w\").write(traceback.format_exc())\n    raise",
        receipt_path = receipt_path,
        error_path = error_path,
    )
}

/// The child's scripted responses: text for the spawn prompt, then a
/// parent-directed reply turn for each delivered agent message.
fn child_responses(receipts_dir: &Path) -> Value {
    let cell = child_cell(receipts_dir);
    let reply = json!([
        { "content": [
            { "type": "toolCall", "name": "ipython", "arguments": { "code": cell } },
        ] },
        { "text": "kid turn done" },
    ]);
    json!([
        { "text": "kid spawned" },
        reply[0].clone(),
        reply[1].clone(),
        reply[0].clone(),
        reply[1].clone(),
        reply[0].clone(),
        reply[1].clone(),
    ])
}

/// One faux-engine script written to disk.
fn write_faux_script(dir: &Path, name: &str, responses: Value) -> PathBuf {
    let path = dir.join(format!("{name}.json"));
    std::fs::write(
        &path,
        json!({ "engine": "faux", "responses": responses }).to_string(),
    )
    .expect("write faux script");
    path
}

/// A recorded JSON file, waiting for the turn that writes it.
fn read_recorded(dir: &Path, name: &str) -> Value {
    let path = dir.join(name);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(content) = std::fs::read_to_string(&path) {
            return serde_json::from_str(&content).expect("recorded json");
        }
        assert!(
            Instant::now() < deadline,
            "record {name} never appeared in {}",
            dir.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One `agent_message.send` host request through the real handler map.
async fn send_agent_message(
    handlers: &HostRequestHandlers,
    receiver_name: &str,
) -> anyhow::Result<Value> {
    let send = handlers.get("agent_message.send").expect("send handler");
    send(HostRequestPayload {
        data: json!({
            "message": "hello there",
            "receiver_role": "child",
            "receiver_name": receiver_name,
        }),
        cell_source_code: None,
    })
    .await
}

/// Verifier: the parent session's family view includes its spawned RLM
/// child; a child-directed `agent_message.send` resolves by name, by RLM
/// child id, and by persisted session id, delivers into the real child
/// worker, and the child's own parent-directed reply delivers back.
#[tokio::test]
async fn parent_child_agent_message_round_trip_end_to_end() {
    let Some(kernel_python) = kernel_python() else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let receipts_dir = dir.path().join("receipts");
    std::fs::create_dir_all(&receipts_dir).expect("receipts dir");

    // The parent session: a real worker (the child's reply target) whose
    // script only needs to absorb the reply turns.
    let parent_script = write_faux_script(
        dir.path(),
        "parent",
        json!([
            { "text": "parent turn done" },
            { "text": "parent turn done" },
            { "text": "parent turn done" },
            { "text": "parent turn done" },
        ]),
    );
    let child_script = write_faux_script(dir.path(), "child", child_responses(&receipts_dir));

    // The supervisor passes the kernel python to the workers it launches.
    let _daemon = spawn_supervisor(&socket, &agent_dir, &kernel_python);
    wait_socket_ready(&socket);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");

    client.send_command(
        "create-parent",
        json!({
            "type": "create",
            "name": "parent",
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": sessions_dir.to_string_lossy(),
                "script": parent_script.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response("create-parent");
    assert_eq!(created["success"], true, "create parent failed: {created}");
    let parent = &created["data"];
    let parent_active_session_id = parent["activeSessionId"]
        .as_str()
        .or_else(|| parent["id"].as_str())
        .expect("parent active session id")
        .to_string();
    let parent_session_id = parent["sessionId"].as_str().expect("parent session id");
    let parent_session_file = parent["sessionFile"]
        .as_str()
        .expect("parent session file")
        .to_string();

    // The parent's children registry (the same construction the worker
    // engine performs), bound to the real parent identity: the child
    // spawns through the supervisor and lands in the registry the
    // controller's family view reads.
    let link = Arc::new(SupervisorLink::new(socket.clone()));
    let children = SupervisorChildSessions::new(
        Arc::clone(&link),
        agent_dir.clone(),
        parent_active_session_id.clone(),
        std::sync::Arc::new(pa_daemon::model_allowlist::ModelRefusalTelemetry::new(
            agent_dir.clone(),
            /*telemetry_disabled*/ true,
        )),
    );
    children.set_identity(ParentIdentity {
        rlm_depth: 0,
        rlm_max_depth: 2,
        model: Some("faux/faux-1".to_string()),
        cwd: Some(dir.path().to_string_lossy().to_string()),
        session_id: Some(parent_session_id.to_string()),
        session_file: Some(parent_session_file),
        thinking: None,
        child_script: Some(child_script.to_string_lossy().to_string()),
    });
    let handle = children
        .spawn(RlmSpawnRequest {
            prompt: "work on the lane".to_string(),
            name: Some("kid".to_string()),
            model: None,
            thinking: None,
            cell_source_code: None,
        })
        .await
        .expect("spawn the child");
    assert_eq!(handle.name, "kid");
    let child_id = handle.rlm_child_id.clone();

    // The detached task prompt waits for the parent's turn boundary (the
    // spawn admission ordering); this harness owns its own children
    // registry, separate from the parent worker's engine, so the boundary
    // the real parent's turn would bump has to be simulated here. Without
    // it the spawn prompt never fires and the delivered messages consume
    // the child's scripted spawn response.
    children.notify_turn_done();
    // Wait for the spawn prompt's turn to settle before delivering: the
    // child must run its spawn turn ("kid spawned") before the reply
    // script begins, or the first delivered message would consume the
    // spawn response and lose its own reply cell.
    let spawn_row = loop {
        let roster = children.list_subagents().await.expect("child roster");
        let row = roster.first().expect("one child row");
        // The spawn turn settled once the child went idle with an answer
        // (or an error); a still-running child keeps polling.
        if row.status == "completed" || row.status == "error" {
            break row.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(spawn_row.status, "completed", "spawn turn: {spawn_row:?}");

    // The roster row gives the child's live and persisted session ids.
    let roster = children.list_subagents().await.expect("child roster");
    let child_row = roster.first().expect("one child row");
    let child_active_session_id = child_row
        .active_session_id
        .clone()
        .expect("child active session id");
    let child_session_id = child_row.session_id.clone().expect("child session id");
    assert_eq!(child_row.session_name, "kid");

    // The parent-side controller: the same wiring the worker's engine
    // performs (family + delivery over the supervisor link).
    let own_summary = json!({
        "activeSessionId": parent_active_session_id,
        "sessionId": parent_session_id,
        "sessionName": "parent",
        "runtimeKind": "top-level",
    });
    let controller = Arc::new(LinkAgentMessageController::new(
        Arc::clone(&link),
        parent_active_session_id.clone(),
        // The test has no worker token: the direct peer path is refused
        // and the supervisor-routed send (the TS remote path) delivers.
        "no-worker-token".to_string(),
        Arc::new(std::sync::Mutex::new(Some(own_summary))),
        Some(Arc::new(children)),
    ));
    let mut handlers = HostRequestHandlers::default();
    register_agent_message_host_handlers(Arc::clone(&controller) as Arc<_>, &mut handlers);

    // The family view lists the child (by every identifier form) and no
    // phantom sibling for it.
    let family = controller.family().await.expect("family");
    let child_members: Vec<_> = family
        .iter()
        .filter(|member| member.relationship == AgentFamilyRelationship::Child)
        .collect();
    assert_eq!(child_members.len(), 1, "{family:?}");
    let child_member = child_members[0];
    assert_eq!(child_member.id, child_active_session_id);
    assert_eq!(child_member.name.as_deref(), Some("kid"));
    assert!(child_member.aliases.contains(&child_id), "{child_member:?}");
    assert!(
        child_member.aliases.contains(&child_session_id),
        "{child_member:?}"
    );

    // Send by name, by RLM child id, and by persisted session id: every
    // form resolves through the family view and delivers into the real
    // child worker with the TS receipt shape.
    for selector in ["kid", &child_id, &child_session_id] {
        let receipt = send_agent_message(&handlers, selector)
            .await
            .unwrap_or_else(|error| panic!("child send by {selector} failed: {error:#}"));
        // `delivered` when the child is idle, `queued` behind its current
        // turn (the TS steer lane): both mean the message reached the
        // child worker; the rendering count below proves it ran.
        let status = receipt["deliveryStatus"].as_str().expect("status");
        assert!(
            status == "delivered" || status == "queued",
            "the send by {selector} must reach the child: {receipt}"
        );
        assert_eq!(
            receipt["target"]["activeSessionId"], child_active_session_id,
            "the send by {selector} targets the child: {receipt}"
        );
        assert_eq!(receipt["receiverRole"], "child", "{receipt}");
        assert!(receipt["id"].as_str().unwrap().starts_with("agentmsg_"));
    }

    // The child rendered every delivered prompt once and answered each
    // with a real parent-directed kernel send. Each delivery's card
    // carries the body twice (the row content plus details.message), so
    // three deliveries render the body six times.
    client.wait_idle("w-child", &child_active_session_id);
    let child_messages = client.messages("gm-child", &child_active_session_id);
    assert_eq!(
        child_messages.matches("hello there").count(),
        6,
        "the child rendered every delivered message once: {child_messages}"
    );
    if let Ok(error) = std::fs::read_to_string(receipts_dir.join("child-reply.error")) {
        panic!("child kernel cell failed: {error}");
    }
    let child_receipt = read_recorded(&receipts_dir, "child-reply.json");
    let reply_status = child_receipt["deliveryStatus"].as_str().expect("status");
    assert!(
        reply_status == "delivered" || reply_status == "queued",
        "the child's parent send must reach the parent: {child_receipt}"
    );
    assert_eq!(
        child_receipt["target"]["activeSessionId"], parent_active_session_id,
        "the child's send targets the parent: {child_receipt}"
    );
    assert_eq!(child_receipt["receiverRole"], "parent", "{child_receipt}");

    // The parent rendered every reply prompt from the child's name.
    client.wait_idle("w-parent", &parent_active_session_id);
    let parent_messages = client.messages("gm-parent", &parent_active_session_id);
    // The reply prompt carries the child relationship label (the TS
    // `child:<name>` sender prefix for subagent-origin messages).
    assert_eq!(
        parent_messages
            .matches("[agent-message from child:kid]")
            .count(),
        3,
        "the parent rendered every child reply: {parent_messages}"
    );
    // Each reply's card carries the body twice (row content plus
    // details.message), so three replies render the body six times.
    assert_eq!(
        parent_messages.matches("kid reply").count(),
        6,
        "the reply bodies rendered in the parent: {parent_messages}"
    );
}

/// Verifier (the misroute regression): the family roster derives from
/// durable parent edges, never from names or runtime kinds. A second
/// root's identically-named child is NOT addressable from the first
/// family by role or name, a broadcast reaches only the nuclear family,
/// the child's parent-reply targets its true parent, and the observe
/// roster labels only true edges — the daemon-wide sibling bucket and
/// the "every subagent is a child" labels are gone.
#[tokio::test]
async fn family_edges_never_cross_families_end_to_end() {
    let Some(kernel_python) = kernel_python() else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");

    // Both parents and both children run text-only faux scripts: no
    // kernel is needed (this verifier runs everywhere, like the other
    // Linux e2e).
    let parent_a_script = write_faux_script(
        dir.path(),
        "parent-a",
        json!([
            { "text": "parent-a turn done" },
            { "text": "parent-a turn done" },
            { "text": "parent-a turn done" },
            { "text": "parent-a turn done" },
        ]),
    );
    let parent_b_script = write_faux_script(
        dir.path(),
        "parent-b",
        json!([{ "text": "parent-b turn done" }]),
    );
    let kid_script = write_faux_script(dir.path(), "kid", json!([{ "text": "kid spawned" }]));

    let _daemon = spawn_supervisor(&socket, &agent_dir, &kernel_python);
    wait_socket_ready(&socket);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");

    let mut roots = Vec::new();
    for (name, script) in [("parent-a", parent_a_script), ("parent-b", parent_b_script)] {
        client.send_command(
            &format!("create-{name}"),
            json!({
                "type": "create",
                "name": name,
                "config": {
                    "cwd": dir.path().to_string_lossy(),
                    "sessionDir": sessions_dir.to_string_lossy(),
                    "script": script.to_string_lossy(),
                },
            }),
        );
        let created = client.read_response(&format!("create-{name}"));
        assert_eq!(created["success"], true, "create {name} failed: {created}");
        roots.push((
            created["data"]["activeSessionId"]
                .as_str()
                .or_else(|| created["data"]["id"].as_str())
                .expect("active session id")
                .to_string(),
            created["data"]["sessionId"].as_str().expect("session id").to_string(),
            created["data"]["sessionFile"]
                .as_str()
                .expect("session file")
                .to_string(),
        ));
    }
    let (parent_a_active, parent_a_session, parent_a_file) = &roots[0];
    let (parent_b_active, parent_b_session, _) = &roots[1];

    // Each root spawns its own identically-named child ("kid"): the name
    // collides across the two families — the historical misroute bait.
    let link = Arc::new(SupervisorLink::new(socket.clone()));
    let mut kids = Vec::new();
    for (index, (active, session, file)) in roots.iter().enumerate() {
        let children = SupervisorChildSessions::new(
            Arc::clone(&link),
            agent_dir.clone(),
            active.clone(),
            std::sync::Arc::new(pa_daemon::model_allowlist::ModelRefusalTelemetry::new(
                agent_dir.clone(),
                /*telemetry_disabled*/ true,
            )),
        );
        children.set_identity(ParentIdentity {
            rlm_depth: 0,
            rlm_max_depth: 2,
            model: Some("faux/faux-1".to_string()),
            cwd: Some(dir.path().to_string_lossy().to_string()),
            session_id: Some(session.clone()),
            session_file: Some(file.clone()),
            thinking: None,
            child_script: Some(kid_script.to_string_lossy().to_string()),
        });
        let handle = children
            .spawn(RlmSpawnRequest {
                prompt: "work on the lane".to_string(),
                name: Some("kid".to_string()),
                model: None,
                thinking: None,
                cell_source_code: None,
            })
            .await
            .expect("spawn the child");
        assert_eq!(handle.name, "kid");
        children.notify_turn_done();
        // The spawn turn settles once the child goes idle with an answer.
        loop {
            let roster = children.list_subagents().await.expect("child roster");
            let row = roster.first().expect("one child row");
            if row.status == "completed" || row.status == "error" {
                assert_eq!(row.status, "completed", "spawn turn: {row:?}");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let roster = children.list_subagents().await.expect("child roster");
        let row = roster.first().expect("one child row");
        kids.push((
            row.active_session_id.clone().expect("child active id"),
            row.session_id.clone().expect("child session id"),
            children,
            index,
        ));
    }
    let (kid_a_active, kid_a_session, registry_a, _) = kids[0].clone();
    let (kid_b_active, _kid_b_session, _registry_b, _) = kids[1].clone();

    // Parent-a's family view: the other root is its sibling (root
    // sessions are each other's family), its own "kid" is its Child, and
    // parent-b's "kid" NEVER appears in any role — the old daemon-wide
    // sibling bucket listed it.
    let parent_a_summary = json!({
        "activeSessionId": parent_a_active,
        "sessionId": parent_a_session,
        "sessionName": "parent-a",
        "runtimeKind": "top-level",
        "sessionFile": parent_a_file,
    });
    let controller = Arc::new(LinkAgentMessageController::new(
        Arc::clone(&link),
        parent_a_active.clone(),
        "no-worker-token".to_string(),
        Arc::new(std::sync::Mutex::new(Some(parent_a_summary))),
        Some(Arc::new(registry_a.clone())),
    ));
    let family = controller.family().await.expect("family");
    let roles: Vec<(String, &str)> = family
        .iter()
        .map(|member| (member.id.clone(), member.relationship.as_str()))
        .collect();
    assert!(
        roles.contains(&(parent_b_active.clone(), "sibling")),
        "the other root is a sibling: {family:?}"
    );
    assert!(
        roles.contains(&(kid_a_active.clone(), "child")),
        "the spawned kid is a child: {family:?}"
    );
    assert_eq!(
        family.len(),
        2,
        "no other family's session may enter the family view: {family:?}"
    );

    // The broadcast ("all") reaches exactly the nuclear family: the
    // sibling root and the own child — never parent-b's kid.
    let mut handlers = HostRequestHandlers::default();
    register_agent_message_host_handlers(Arc::clone(&controller) as Arc<_>, &mut handlers);
    let broadcast = handlers.get("agent_message.send").expect("send handler");
    let receipts = broadcast(
        HostRequestPayload {
            data: json!({ "message": "family update", "target": "all" }),
            cell_source_code: None,
        },
    )
    .await
    .expect("broadcast");
    let receipt_targets: Vec<String> = receipts["receipts"]
        .as_array()
        .expect("receipts")
        .iter()
        .map(|receipt| {
            receipt["target"]["activeSessionId"]
                .as_str()
                .or_else(|| receipt["target"].as_str())
                .expect("receipt target")
                .to_string()
        })
        .collect();
    assert_eq!(receipt_targets.len(), 2, "receipts: {receipts}");
    assert!(receipt_targets.contains(&parent_b_active.clone()));
    assert!(
        receipt_targets.contains(&kid_a_active.clone()),
        "the own child is reachable: {receipts}"
    );
    assert!(
        !receipt_targets.contains(&kid_b_active.clone()),
        "another family's kid must never receive the broadcast: {receipts}"
    );

    // The child's own family view: its true parent (parent-a) and nothing
    // else — the identically-named child of parent-b is NOT its sibling.
    let kid_a_summary = json!({
        "activeSessionId": kid_a_active,
        "sessionId": kid_a_session,
        "sessionName": "kid",
        "runtimeKind": "subagent",
        "parentActiveSessionId": parent_a_active,
        "parentSessionId": parent_a_session,
        "parentSessionPath": parent_a_file,
    });
    let kid_controller = LinkAgentMessageController::new(
        Arc::clone(&link),
        kid_a_active.clone(),
        "no-worker-token".to_string(),
        Arc::new(std::sync::Mutex::new(Some(kid_a_summary.clone()))),
        None,
    );
    let kid_family = kid_controller.family().await.expect("kid family");
    assert_eq!(
        kid_family.len(),
        1,
        "the child's family is its parent alone: {kid_family:?}"
    );
    assert_eq!(kid_family[0].relationship, AgentFamilyRelationship::Parent);
    assert_eq!(kid_family[0].id, *parent_a_active);

    // The name collision is unaddressable from the child: a sibling send
    // for "kid" must fail (the child has no siblings), never deliver to
    // parent-b's identically-named child (the historical misroute).
    let mut kid_handlers = HostRequestHandlers::default();
    register_agent_message_host_handlers(
        Arc::new(LinkAgentMessageController::new(
            Arc::clone(&link),
            kid_a_active.clone(),
            "no-worker-token".to_string(),
            Arc::new(std::sync::Mutex::new(Some(kid_a_summary.clone()))),
            None,
        )) as Arc<_>,
        &mut kid_handlers,
    );
    let kid_send = kid_handlers.get("agent_message.send").expect("send handler");
    let crossed = kid_send(
        HostRequestPayload {
            data: json!({
                "message": "hello sibling",
                "receiver_role": "sibling",
                "receiver_name": "kid",
            }),
            cell_source_code: None,
        },
    )
    .await;
    let error = crossed.expect_err("a cross-family sibling send must not resolve");
    assert!(
        error.to_string().contains("No sibling matches"),
        "the sibling send must fail closed: {error:#}"
    );

    // The parent-directed send targets the TRUE parent by its durable
    // edge — the receipt names parent-a's live id.
    let parent_reply = kid_send(
        HostRequestPayload {
            data: json!({
                "message": "parent update",
                "receiver_role": "parent",
            }),
            cell_source_code: None,
        },
    )
    .await
    .expect("parent send");
    assert_eq!(
        parent_reply["target"]["activeSessionId"], *parent_a_active,
        "the parent reply reaches the true parent: {parent_reply}"
    );

    // The observe roster labels only true edges: the child sees itself
    // (isCurrent) and its parent; parent-b's kid and parent-b itself are
    // outside its nuclear family and never labeled "child" of it.
    let observer = Arc::new(LinkAgentObserveController::new(
        Arc::clone(&link),
        kid_a_active.clone(),
        Arc::new(std::sync::Mutex::new(Some(kid_a_summary))),
        None,
    ));
    let roster = observer.list_agents().await.expect("observe roster");
    assert_eq!(
        roster.len(),
        2,
        "the observe roster is the nuclear family: {roster:?}"
    );
    let current = roster
        .iter()
        .find(|summary| summary.is_current)
        .expect("the caller's own row");
    assert_eq!(current.active_session_id.as_deref(), Some(kid_a_active.as_str()));
    let parent_row = roster
        .iter()
        .find(|summary| summary.active_session_id.as_deref() == Some(parent_a_active.as_str()))
        .expect("the parent row");
    assert_eq!(parent_row.relationship, Some(AgentFamilyRelationship::Parent));
    assert!(
        !roster.iter().any(|summary| summary.active_session_id.as_deref()
            == Some(kid_b_active.as_str())),
        "another family's subagent is never in the observe roster: {roster:?}"
    );

    // Parent-a's observe roster: itself, the sibling root, its own child
    // — with the sibling labeled Sibling and the child labeled Child
    // (never the daemon-wide runtime-kind labels).
    let observer = Arc::new(LinkAgentObserveController::new(
        Arc::clone(&link),
        parent_a_active.clone(),
        Arc::new(std::sync::Mutex::new(Some(
            json!({
                "activeSessionId": parent_a_active,
                "sessionId": parent_a_session,
                "sessionName": "parent-a",
                "runtimeKind": "top-level",
                "sessionFile": parent_a_file,
            }),
        ))),
        Some(Arc::new(registry_a.clone())),
    ));
    let roster = observer.list_agents().await.expect("observe roster");
    assert_eq!(
        roster.len(),
        3,
        "the parent's observe roster is its nuclear family: {roster:?}"
    );
    let sibling_row = roster
        .iter()
        .find(|summary| summary.active_session_id.as_deref() == Some(parent_b_active.as_str()))
        .expect("the sibling root row");
    assert_eq!(
        sibling_row.relationship,
        Some(AgentFamilyRelationship::Sibling)
    );
    let child_row = roster
        .iter()
        .find(|summary| summary.active_session_id.as_deref() == Some(kid_a_active.as_str()))
        .expect("the own child row");
    assert_eq!(child_row.relationship, Some(AgentFamilyRelationship::Child));
    assert!(
        !roster.iter().any(|summary| summary.active_session_id.as_deref()
            == Some(kid_b_active.as_str())),
        "another family's subagent is never labeled child here: {roster:?}"
    );
}
