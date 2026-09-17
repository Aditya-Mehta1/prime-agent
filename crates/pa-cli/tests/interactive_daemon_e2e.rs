//! End-to-end verifier for the interactive TUI: spawn the real supervisor
//! (`prime-agent --mode daemon`, the same binary the interactive runtime
//! launches when no daemon is running), then drive the TUI headlessly
//! against a scripted daemon session — create/attach, prompt, streamed
//! assistant output, session list, and a session switch — and assert on the
//! rendered frames plus the daemon-side session state.
//!
//! The scripted engine seam (`create` config `script`) is the same faux
//! provider contract `pa-daemon/tests/supervisor_e2e.rs` uses; the product
//! never sets it.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pa_types::daemon::DaemonCommand;

struct Supervisor {
    child: Child,
    socket: PathBuf,
}

/// Stop the daemon on `socket` by protocol so it can shut its workers down;
/// kill the child when the protocol path fails. Drop runs even when the test
/// panics, so a failing test must not leak worker processes.
impl Drop for Supervisor {
    fn drop(&mut self) {
        graceful_shutdown(&self.socket);
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// RAII guard for a detached supervisor (spawned by
/// `ensure_daemon_running_with`): shuts the daemon down on scope exit.
struct DetachedDaemon {
    socket: PathBuf,
}

impl Drop for DetachedDaemon {
    fn drop(&mut self) {
        graceful_shutdown(&self.socket);
        let _ = std::fs::remove_file(&self.socket);
    }
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

/// Shut the spawned supervisor down by protocol and assert that it — and
/// every worker process it spawned — actually exited and the socket file
/// went away. A daemon that only stops its workers but stays parked on its
/// listening socket would leak both processes (the TS client's
/// `waitForDaemonGone` relies on the daemon exiting).
fn assert_daemon_stops_clean(socket: &Path) {
    // Sync JSONL exchange (called from the sync guard path).
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let stream = UnixStream::connect(socket).expect("connect the spawned daemon");
    let write_half = stream.try_clone().expect("clone socket");
    let mut reader = BufReader::new(stream);
    let mut writer = write_half;
    let mut hello = String::new();
    reader.read_line(&mut hello).expect("read daemon_hello");
    let hello: serde_json::Value = serde_json::from_str(hello.trim()).expect("parse hello");
    let supervisor_pid = hello["supervisorPid"].as_u64().expect("supervisorPid") as u32;
    // The worker processes the supervisor spawned for live sessions, captured
    // before the shutdown so reparented workers can still be tracked.
    let worker_pids = child_pids_of(supervisor_pid);

    let command = serde_json::json!({
        "type": "command",
        "id": "stop-assert",
        "protocol": { "name": "prime-agent.daemon", "version": 7 },
        "command": { "type": "shutdown" },
    });
    let mut line = serde_json::to_string(&command).expect("serialize");
    line.push('\n');
    writer.write_all(line.as_bytes()).expect("send shutdown");
    writer.flush().expect("flush");

    // The supervisor process exits by itself and cleans up its socket.
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_alive(supervisor_pid) {
        assert!(
            Instant::now() < deadline,
            "the spawned supervisor {supervisor_pid} did not exit after shutdown"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !socket.exists(),
        "the spawned supervisor removed its socket file"
    );
    // No worker process outlives the shutdown.
    for pid in worker_pids {
        let deadline = Instant::now() + Duration::from_secs(10);
        while process_alive(pid) {
            assert!(
                Instant::now() < deadline,
                "worker {pid} leaked after shutdown"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Sync JSONL shutdown request (Drop runs inside the async test runtime, so
/// no nested runtime may be built here). Best effort; callers kill the child
/// process afterwards regardless.
fn graceful_shutdown(socket: &Path) {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let Ok(stream) = UnixStream::connect(socket) else {
        return;
    };
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut writer = write_half;
    let _ = reader.read_line(&mut String::new()); // daemon_hello

    let command = serde_json::json!({
        "type": "command",
        "id": "test-shutdown",
        "protocol": { "name": "prime-agent.daemon", "version": 7 },
        "command": { "type": "shutdown" },
    });
    let Ok(mut line) = serde_json::to_string(&command) else {
        return;
    };
    line.push('\n');
    if writer.write_all(line.as_bytes()).is_err() {
        return;
    }
    let _ = writer.flush();
    // Wait briefly for the supervisor to accept the shutdown (it stops every
    // worker before exiting, so the response is the sync point).
    let _ = reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(5)));
    let mut response = String::new();
    let _ = reader.read_line(&mut response);
}

#[allow(clippy::zombie_processes)]
fn spawn_supervisor(dir: &Path) -> Supervisor {
    let socket = dir.join("daemon.sock");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    command
        .args(["--mode", "daemon", "--daemon-socket"])
        .arg(&socket)
        .env("PRIME_AGENT_CODING_AGENT_DIR", &agent_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The launcher strips inherited worker role env vars before spawning the
    // supervisor; a CLI running inside a daemon worker must not leak them.
    for var in [
        pa_daemon::worker::WORKER_ROLE_ENV,
        pa_daemon::worker::WORKER_TOKEN_ENV,
        pa_daemon::worker::WORKER_ACTIVE_SESSION_ID_ENV,
        pa_daemon::worker::WORKER_RECOVERY_JOURNAL_ENV,
        pa_daemon::worker::WORKER_SUPERVISOR_SOCKET_ENV,
        pa_daemon::worker::WORKER_SOCKET_ENV,
        pa_daemon::worker::WORKER_INSTANCE_ID_ENV,
        pa_daemon::worker::WORKER_SCRIPT_ENV,
    ] {
        command.env_remove(var);
    }
    let child = command.spawn().expect("spawn prime-agent --mode daemon");
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if socket.exists() {
            return Supervisor { child, socket };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("supervisor socket never appeared");
}

/// Create a live session through the daemon protocol (the same `create`
/// config the TUI sends), writing the scripted engine config first.
async fn create_session_via_daemon(
    socket: &Path,
    script_path: &Path,
    script: &serde_json::Value,
    cwd: &Path,
    session_dir: &Path,
) -> String {
    std::fs::write(script_path, script.to_string()).expect("write script");
    let (client, _events) = pa_tui::daemon_client::DaemonClient::connect(socket)
        .await
        .expect("connect supervisor");
    let data = client
        .request_ok(DaemonCommand::Create {
            id: None,
            session_path: None,
            continue_recent: None,
            no_session: None,
            name: None,
            config: Some(serde_json::json!({
                "cwd": cwd.display().to_string(),
                "sessionDir": session_dir.display().to_string(),
                "script": script_path.display().to_string(),
            })),
            runtime_metadata: None,
            lifecycle: None,
            env: None,
            launch_env: None,
            rest: Default::default(),
        })
        .await
        .expect("create session");
    client.close();
    data.get("activeSessionId")
        .or_else(|| data.get("id"))
        .and_then(serde_json::Value::as_str)
        .expect("session id")
        .to_string()
}

#[tokio::test]
async fn tui_attaches_prompts_streams_lists_and_switches() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let supervisor = spawn_supervisor(dir.path());

    // A second live session created through the daemon protocol, so the
    // switch target is known by id (not by list position).
    let script = serde_json::json!({ "responses": [
        { "text": "hello from scripted", "delayMs": 20 },
        { "text": "second turn" },
    ] });
    let second = create_session_via_daemon(
        &supervisor.socket,
        &dir.path().join("script.json"),
        &script,
        dir.path(),
        &session_dir,
    )
    .await;

    let options = pa_tui::interactive::InteractiveOptions {
        socket_path: supervisor.socket.clone(),
        cwd: dir.path().to_path_buf(),
        session_dir: Some(session_dir.clone()),
        script_path: Some(dir.path().join("script.json")),
        model_selection: Default::default(),
        no_session: false,
        session: pa_tui::interactive::SessionSelection::New,
        initial_message: None,
        theme: "prime".to_string(),
        version: "0.0.0".to_string(),
        onboarding: None,
    };
    let plan = pa_tui::interactive::HeadlessPlan {
        steps: vec![
            pa_tui::interactive::HeadlessStep::Submit("hi".to_string()),
            pa_tui::interactive::HeadlessStep::WaitIdle { timeout_ms: 30_000 },
            pa_tui::interactive::HeadlessStep::Submit("again".to_string()),
            pa_tui::interactive::HeadlessStep::WaitIdle { timeout_ms: 30_000 },
            // Session list, then switch to the second session by id: the
            // transcript must rebuild from its (empty) snapshot and the next
            // prompt must run against the switched session.
            pa_tui::interactive::HeadlessStep::Submit("/list".to_string()),
            pa_tui::interactive::HeadlessStep::Submit(format!("/switch {second}")),
            pa_tui::interactive::HeadlessStep::Submit("third".to_string()),
            pa_tui::interactive::HeadlessStep::WaitIdle { timeout_ms: 30_000 },
        ],
        width: 100,
        height: 30,
    };
    let outcome =
        pa_tui::interactive::run_interactive(options, pa_tui::interactive::UiMode::Headless(plan))
            .await
            .expect("interactive run");

    assert!(!outcome.frames.is_empty(), "frames were captured");
    let rendered = outcome.frames.join("\n");
    assert!(
        rendered.contains("hello from scripted"),
        "first scripted turn rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("second turn"),
        "second scripted turn rendered:\n{rendered}"
    );
    assert!(rendered.contains("hi"), "user message echoed:\n{rendered}");
    assert!(
        rendered.contains("again"),
        "queued prompt rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("live sessions:"),
        "session list rendered:\n{rendered}"
    );
    // After the switch, the third prompt ran against the switched session:
    // the scripted engine replays response 0 for it.
    assert!(
        rendered.contains("switched to session"),
        "switch note rendered:\n{rendered}"
    );
    assert_eq!(
        outcome.active_session_id, second,
        "the run ended attached to the switched session"
    );
    assert_eq!(
        outcome.last_assistant_text.as_deref(),
        Some("hello from scripted"),
        "the switched session produced its first scripted turn"
    );

    // Daemon-side verification: both sessions hold their turns.
    let (client, _events) = pa_tui::daemon_client::DaemonClient::connect(&supervisor.socket)
        .await
        .expect("connect supervisor");
    let last = client
        .request_ok(DaemonCommand::GetLastAssistantText {
            id: None,
            active_session_id: second.clone(),
            rest: Default::default(),
        })
        .await
        .expect("get_last_assistant_text");
    assert_eq!(last["text"], "hello from scripted");
    let sessions = client
        .request_ok(DaemonCommand::List {
            id: None,
            all: None,
            cwd: None,
            session_dir: None,
            include_client_owned: None,
            rest: Default::default(),
        })
        .await
        .expect("list");
    assert_eq!(
        sessions["sessions"].as_array().map(Vec::len),
        Some(2),
        "both sessions stay live after the TUI exited: {sessions}"
    );
    client.close();

    // The session files are on disk (reattach survives a TUI restart).
    let persisted = std::fs::read_dir(&session_dir)
        .expect("read session dir")
        .flatten()
        .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .count();
    assert_eq!(persisted, 2, "two session files persisted");
    drop(supervisor);
}

/// The product launch path: `ensure_daemon_running` spawns a detached
/// `prime-agent --mode daemon` when nothing is listening, then the TUI
/// attaches through it.
#[tokio::test]
async fn ensure_daemon_running_spawns_supervisor_and_tui_attaches() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let socket = dir.path().join("spawned.sock");
    std::env::set_var("PRIME_AGENT_CODING_AGENT_DIR", &agent_dir);

    let script_path = dir.path().join("script.json");
    std::fs::write(
        &script_path,
        serde_json::json!({ "responses": [{ "text": "spawned hello" }] }).to_string(),
    )
    .expect("write script");

    let options = pa_tui::interactive::InteractiveOptions {
        socket_path: socket.clone(),
        cwd: dir.path().to_path_buf(),
        session_dir: Some(session_dir.clone()),
        script_path: Some(script_path),
        model_selection: Default::default(),
        no_session: false,
        session: pa_tui::interactive::SessionSelection::New,
        initial_message: Some("boot".to_string()),
        theme: "prime".to_string(),
        version: "0.0.0".to_string(),
        onboarding: None,
    };
    // The interactive runtime's own launch sequence, minus the TTY: spawn
    // the real supervisor binary detached and wait for the hello handshake.
    let _guard = DetachedDaemon {
        socket: socket.clone(),
    };
    let exe = std::path::PathBuf::from(env!("CARGO_BIN_EXE_prime-agent"));
    pa_cli::ensure_daemon_running_with(&exe, &socket, dir.path())
        .await
        .expect("spawn the daemon");
    let outcome = pa_tui::interactive::run_interactive(
        options,
        pa_tui::interactive::UiMode::Headless(pa_tui::interactive::HeadlessPlan {
            steps: vec![pa_tui::interactive::HeadlessStep::WaitIdle { timeout_ms: 30_000 }],
            width: 80,
            height: 24,
        }),
    )
    .await
    .expect("headless interactive run");
    assert!(
        outcome
            .frames
            .iter()
            .any(|frame| frame.contains("spawned hello")),
        "initial message ran against the spawned daemon:\n{}",
        outcome.frames.join("\n")
    );
    assert_eq!(
        outcome.last_assistant_text.as_deref(),
        Some("spawned hello")
    );

    // The product contract under test: shut the spawned supervisor down by
    // protocol and require the process tree to actually exit (the guard
    // stays as the panic backstop; this call asserts the clean stop).
    assert_daemon_stops_clean(&socket);
}

/// Slash-command dispatch over a live scripted session: the session command
/// executes in the worker (durable echo + result rows reach the transcript
/// and the session file), client commands without a UI report
/// unavailability, unknown commands get the TS suggestion error, and the
/// autocomplete menu renders from the shared registry.
#[tokio::test]
async fn tui_dispatches_slash_commands_menu_and_suggestions() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let supervisor = spawn_supervisor(dir.path());

    // The faux engine (`engine: "faux"`) drives the real agent engine over
    // the scripted faux provider, so the worker's session-command admission
    // path runs exactly as in the product.
    let script = serde_json::json!({ "engine": "faux", "responses": [
        { "text": "scripted reply" },
    ] });
    std::fs::write(dir.path().join("script.json"), script.to_string()).expect("write script");
    let options = pa_tui::interactive::InteractiveOptions {
        socket_path: supervisor.socket.clone(),
        cwd: dir.path().to_path_buf(),
        session_dir: Some(session_dir.clone()),
        script_path: Some(dir.path().join("script.json")),
        model_selection: Default::default(),
        no_session: false,
        session: pa_tui::interactive::SessionSelection::New,
        initial_message: None,
        theme: "prime".to_string(),
        version: "0.0.0".to_string(),
        onboarding: None,
    };
    let plan = pa_tui::interactive::HeadlessPlan {
        steps: vec![
            // A session command runs in the worker and its durable rows
            // render (echo + result).
            pa_tui::interactive::HeadlessStep::Submit("/goal status".to_string()),
            pa_tui::interactive::HeadlessStep::WaitIdle { timeout_ms: 30_000 },
            // Unknown command: the exact TS suggestion error.
            pa_tui::interactive::HeadlessStep::Submit("/modle".to_string()),
            // A builtin client command whose UI does not exist yet.
            pa_tui::interactive::HeadlessStep::Submit("/model".to_string()),
            // The autocomplete menu: typed input like a user keystroke by
            // keystroke, completed with Enter, then submitted.
            pa_tui::interactive::HeadlessStep::Type("/".to_string()),
            pa_tui::interactive::HeadlessStep::Type("goa".to_string()),
            pa_tui::interactive::HeadlessStep::Type("\n".to_string()),
            pa_tui::interactive::HeadlessStep::Type("\n".to_string()),
            pa_tui::interactive::HeadlessStep::WaitIdle { timeout_ms: 30_000 },
        ],
        width: 120,
        height: 36,
    };
    let outcome =
        pa_tui::interactive::run_interactive(options, pa_tui::interactive::UiMode::Headless(plan))
            .await
            .expect("interactive run");

    // Verification seam: dump the captured frames for manual frame-diffing
    // against the TS product (PA_TUI_DUMP_FRAMES=<dir>).
    if let Ok(dir) = std::env::var("PA_TUI_DUMP_FRAMES") {
        for (index, frame) in outcome.frames.iter().enumerate() {
            let _ = std::fs::write(
                std::path::Path::new(&dir).join(format!("frame-{index:03}.txt")),
                frame,
            );
        }
    }
    let rendered = outcome.frames.join("\n");
    assert!(
        rendered.contains("/goal status"),
        "the session-command echo row rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("No active goal."),
        "the session-command result row rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("Unknown command: /modle. Did you mean /model?"),
        "the unknown-command suggestion matched the TS string:\n{rendered}"
    );
    assert!(
        rendered.contains("/model is not available in this client yet"),
        "the unavailable client command reported itself:\n{rendered}"
    );
    // The menu: the first registry entry is selected at `/`, and `/goa`
    // fuzzy-matches to the goal command.
    assert!(
        rendered.contains("\u{203a} settings"),
        "the slash menu rendered with the selected first entry:\n{rendered}"
    );
    assert!(
        rendered.contains("Open settings menu"),
        "the selected item's description rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("\u{203a} goal"),
        "the fuzzy best match for /goa rendered selected:\n{rendered}"
    );

    // The durable rows persisted: the session file carries the echo and
    // result custom entries for both executions.
    let mut saw_echo = false;
    let mut saw_result = false;
    for entry in std::fs::read_dir(&session_dir)
        .expect("read session dir")
        .flatten()
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        saw_echo |= content.contains("\"session_slash_command\"");
        saw_result |= content.contains("\"session_slash_command_result\"");
    }
    assert!(
        saw_echo,
        "the session file persisted the session_slash_command rows"
    );
    assert!(
        saw_result,
        "the session file persisted the session_slash_command_result rows"
    );
    drop(supervisor);
}
