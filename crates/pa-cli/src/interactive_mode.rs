//! Interactive mode wiring: resolve the daemon socket, ensure a supervisor is
//! listening (spawning one detached, TS `daemon-launch.ts` semantics), pick
//! the session from the CLI session flags, and hand off to the pa-tui
//! interactive loop. The session keeps running in the worker after the UI
//! exits; reattaching later restores it from the same session file.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use crate::config;
use crate::mode::RunOptions;
use pa_tui::interactive::{InteractiveOptions, SessionSelection, UiMode};

const DAEMON_STARTUP_TIMEOUT_MS: u64 = 30_000;
const DAEMON_SHUTDOWN_WAIT_MS: u64 = 5_000;

/// Run the interactive TUI attached to the daemon. Returns the exit code.
pub fn run_interactive_mode(options: &RunOptions) -> Result<i32> {
    let socket_path = resolve_socket_path(options.daemon_socket.as_deref());
    let tui_options = build_tui_options(options, socket_path)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build the interactive runtime")?;
    runtime.block_on(async {
        ensure_daemon_running(&tui_options.socket_path, &tui_options.cwd).await?;
        pa_tui::interactive::run_interactive(tui_options, UiMode::Terminal).await
    })?;
    Ok(0)
}

/// `--daemon-socket` value or the per-user default socket path.
pub fn resolve_socket_path(daemon_socket: Option<&str>) -> PathBuf {
    daemon_socket
        .map(config::expand_tilde_path)
        .unwrap_or_else(pa_daemon::socket::default_daemon_socket_path)
}

fn build_tui_options(options: &RunOptions, socket_path: PathBuf) -> Result<InteractiveOptions> {
    let config = &options.config;
    if options.session.fork.is_some() {
        // A fork must copy the target session into a new file before the
        // daemon can open it; wiring the copy is tracked with session-queue
        // work. Refuse instead of silently resuming the original file.
        return Err(anyhow!(
            "session forking is not wired into the daemon yet; use --resume to reopen the session"
        ));
    }
    let session_dir = options
        .session
        .session_dir
        .clone()
        .or_else(|| Some(config.agent_dir.join("sessions")));
    // Test seam: a scripted faux daemon session (same contract as the print
    // runtime). Verification harness only; never set by the product.
    let script_path = std::env::var_os("PRIME_AGENT_FAUX_SCRIPT").map(PathBuf::from);
    let session = session_selection(&options.session, &session_dir)?;
    Ok(InteractiveOptions {
        socket_path,
        cwd: config.cwd.clone(),
        session_dir,
        script_path,
        no_session: options.session.no_session,
        session,
        initial_message: options.initial_message.clone(),
        theme: String::new(),
        version: crate::config::VERSION.to_string(),
    })
}

/// Map the CLI session flags onto the TUI session selection (the TS order:
/// explicit `--resume` selector, then `--continue`, then a fresh session).
fn session_selection(
    session: &crate::mode::SessionOptions,
    session_dir: &Option<PathBuf>,
) -> Result<SessionSelection> {
    if let Some(selector) = &session.resume {
        let default_dir = config::get_agent_dir().join("sessions");
        let dir = session_dir.as_deref().unwrap_or(&default_dir);
        return Ok(resolve_resume_selector(selector, dir));
    }
    if session.continue_recent {
        return Ok(SessionSelection::ContinueRecent);
    }
    Ok(SessionSelection::New)
}

/// `--resume <selector>`: an existing session file path, a `<id>.jsonl` under
/// the sessions dir, or a live daemon session id (attach).
fn resolve_resume_selector(selector: &str, session_dir: &Path) -> SessionSelection {
    let path = config::expand_tilde_path(selector);
    if path.is_file() {
        return SessionSelection::Resume(path);
    }
    let candidate = session_dir.join(format!("{selector}.jsonl"));
    if candidate.is_file() {
        return SessionSelection::Resume(candidate);
    }
    SessionSelection::Attach(selector.to_string())
}

/// The daemon probe outcome (TS `DaemonVersionProbe`).
enum DaemonProbe {
    /// No socket answered.
    Absent,
    /// A supervisor answered whose protocol/schema matches this build.
    Current,
    /// A supervisor answered with a different protocol/schema.
    Stale(pa_tui::daemon_client::DaemonClient),
}

/// Probe the socket once: connect, read the hello, and classify it.
async fn probe_daemon(socket_path: &Path) -> DaemonProbe {
    let Ok((client, _events)) = pa_tui::daemon_client::DaemonClient::connect(socket_path).await
    else {
        return DaemonProbe::Absent;
    };
    let hello = client.hello();
    let current = hello.get("protocol").and_then(|p| p.get("version"))
        == Some(&serde_json::json!(
            pa_types::daemon::DAEMON_PROTOCOL_VERSION
        ))
        && hello.get("schemaId") == Some(&serde_json::json!(pa_types::daemon::DAEMON_SCHEMA_ID));
    if current {
        client.close();
        DaemonProbe::Current
    } else {
        DaemonProbe::Stale(client)
    }
}

/// Ensure a current daemon is listening on `socket_path`, spawning this
/// executable in `--mode daemon` when it is not (TS `ensureDaemonRunning`:
/// probe; a stale idle daemon is shut down, a busy one refuses replacement).
pub async fn ensure_daemon_running(socket_path: &Path, spawn_cwd: &Path) -> Result<()> {
    match probe_daemon(socket_path).await {
        DaemonProbe::Current => return Ok(()),
        DaemonProbe::Stale(client) => shutdown_stale_daemon(client, socket_path).await?,
        DaemonProbe::Absent => {}
    }
    let exe = std::env::current_exe().context("resolve the prime-agent executable")?;
    ensure_daemon_running_with(&exe, socket_path, spawn_cwd).await
}

/// [`ensure_daemon_running`] with an explicit supervisor executable (the
/// product path uses this process's own binary, TS parity).
pub async fn ensure_daemon_running_with(
    exe: &Path,
    socket_path: &Path,
    spawn_cwd: &Path,
) -> Result<()> {
    match probe_daemon(socket_path).await {
        DaemonProbe::Current => return Ok(()),
        DaemonProbe::Stale(client) => {
            // A stale daemon appeared between the caller's check and here:
            // fall through to the spawn path after refusing busy ones.
            client.close();
        }
        DaemonProbe::Absent => {}
    }
    spawn_supervisor_detached(socket_path, spawn_cwd, exe)?;
    let deadline = Instant::now() + Duration::from_millis(DAEMON_STARTUP_TIMEOUT_MS);
    loop {
        match probe_daemon(socket_path).await {
            DaemonProbe::Current => return Ok(()),
            DaemonProbe::Stale(client) => {
                // A concurrent launcher won the socket with a build whose
                // protocol matches ours at connect time but failed the
                // schema check: re-probe before deciding.
                client.close();
            }
            DaemonProbe::Absent => {}
        }
        if Instant::now() > deadline {
            return Err(anyhow!(
                "Timed out waiting for the Prime Agent daemon to start on {}. Run: prime-agent shutdown --force, then retry the original command.",
                socket_path.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Shut a stale daemon down when no session is busy (TS
/// `shutdownStaleDaemonIfNotBusy`); a busy one refuses replacement.
async fn shutdown_stale_daemon(
    client: pa_tui::daemon_client::DaemonClient,
    socket_path: &Path,
) -> Result<()> {
    let sessions = client
        .request_ok(pa_types::daemon::DaemonCommand::List {
            id: None,
            all: None,
            cwd: None,
            session_dir: None,
            include_client_owned: None,
            rest: Default::default(),
        })
        .await;
    let busy = sessions
        .map(|data| {
            data.get("sessions")
                .and_then(serde_json::Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .any(|row| row.get("isSessionActive") == Some(&serde_json::json!(true)))
                })
                .unwrap_or(true)
        })
        .unwrap_or(true);
    client.close();
    if busy {
        return Err(anyhow!(
            "An incompatible Prime Agent daemon is running on {}.\n\nRun:\n  prime-agent shutdown --force\n\nThen retry the original command (the running daemon has active work).",
            socket_path.display()
        ));
    }
    // Idle: replace it.
    if let Ok((client, _)) = pa_tui::daemon_client::DaemonClient::connect(socket_path).await {
        let _ = client
            .request_ok(pa_types::daemon::DaemonCommand::Shutdown {
                id: None,
                force: None,
                rest: Default::default(),
            })
            .await;
        client.close();
    }
    wait_for_socket_gone(socket_path).await;
    Ok(())
}

/// Wait until nothing accepts connections on the socket (bounded).
async fn wait_for_socket_gone(socket_path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_millis(DAEMON_SHUTDOWN_WAIT_MS);
    while Instant::now() < deadline {
        if !pa_daemon::socket::can_connect(socket_path, Duration::from_millis(250)).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// Spawn a detached supervisor on `socket_path` (TS spawns its own entrypoint
/// with `--mode daemon --daemon-socket`; the child outlives this CLI).
fn spawn_supervisor_detached(socket_path: &Path, spawn_cwd: &Path, exe: &Path) -> Result<()> {
    let mut command = Command::new(exe);
    command
        .args(["--mode", "daemon", "--daemon-socket"])
        .arg(socket_path)
        .current_dir(spawn_cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Strip inherited worker/supervisor role env vars so the spawned
        // supervisor never starts in worker mode (a CLI running inside a
        // daemon worker would otherwise launch a supervisor that listens but
        // never handshakes) — the TS launcher deletes the same set.
        .env_remove(pa_daemon::worker::WORKER_ROLE_ENV)
        .env_remove(pa_daemon::worker::WORKER_TOKEN_ENV)
        .env_remove(pa_daemon::worker::WORKER_ACTIVE_SESSION_ID_ENV)
        .env_remove(pa_daemon::worker::WORKER_RECOVERY_JOURNAL_ENV)
        .env_remove(pa_daemon::worker::WORKER_SUPERVISOR_SOCKET_ENV)
        .env_remove(pa_daemon::worker::WORKER_SOCKET_ENV)
        .env_remove(pa_daemon::worker::WORKER_INSTANCE_ID_ENV)
        .env_remove(pa_daemon::worker::WORKER_SCRIPT_ENV);
    // Detached: own process group, reaped by init, survives this CLI.
    pa_core::platform::process::set_new_process_group(&mut command);
    command
        .spawn()
        .with_context(|| format!("spawn the Prime Agent daemon on {}", socket_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_flags_map_to_selections() {
        let mut session = crate::mode::SessionOptions::default();
        let dir = tempfile::TempDir::new().expect("temp dir");
        let session_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&session_dir).expect("sessions dir");
        assert_eq!(
            session_selection(&session, &Some(session_dir.clone())).unwrap(),
            SessionSelection::New
        );
        session.continue_recent = true;
        assert_eq!(
            session_selection(&session, &Some(session_dir.clone())).unwrap(),
            SessionSelection::ContinueRecent
        );
        session.continue_recent = false;
        session.resume = Some("a1b2c3".to_string());
        // A bare selector that is not a file attaches a live session id.
        assert_eq!(
            session_selection(&session, &Some(session_dir.clone())).unwrap(),
            SessionSelection::Attach("a1b2c3".to_string())
        );
        // An id with a saved file under the sessions dir reopens the file.
        let saved = session_dir.join("deadbeefcafe.jsonl");
        std::fs::write(&saved, "{}\n").expect("write file");
        session.resume = Some("deadbeefcafe".to_string());
        assert_eq!(
            session_selection(&session, &Some(session_dir.clone())).unwrap(),
            SessionSelection::Resume(saved)
        );
        // An explicit file path reopens that session file.
        let file = dir.path().join("saved.jsonl");
        std::fs::write(&file, "{}\n").expect("write file");
        session.resume = Some(file.to_string_lossy().to_string());
        assert_eq!(
            session_selection(&session, &Some(session_dir)).unwrap(),
            SessionSelection::Resume(file)
        );
    }
}
