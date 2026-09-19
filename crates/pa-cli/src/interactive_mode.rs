//! Interactive mode wiring: resolve the daemon socket, ensure a supervisor is
//! listening (spawning one detached, TS `daemon-launch.ts` semantics), pick
//! the session from the CLI session flags, and hand off to the pa-tui
//! interactive loop. The session keeps running in the worker after the UI
//! exits; reattaching later restores it from the same session file.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use crate::config;
use crate::mode::RunOptions;
use pa_tui::interactive::{InteractiveOptions, ModelSelection, SessionSelection, UiMode};

const DAEMON_STARTUP_TIMEOUT_MS: u64 = 30_000;
const DAEMON_SHUTDOWN_WAIT_MS: u64 = 5_000;

/// Persistence for the first-run onboarding answers: the global settings
/// file (TS `setAgentTracesEnabled` / `markOnboardingShown` + flush).
struct SettingsOnboardingSink {
    cwd: PathBuf,
    agent_dir: PathBuf,
    /// When the onboarding task was created: the `onboarding completed`
    /// duration measures sink creation to completion (the TUI starts the
    /// flow right away; the trace-question flow is the whole onboarding).
    created_at: std::time::Instant,
}

impl pa_tui::interactive::OnboardingSink for SettingsOnboardingSink {
    fn agent_traces_enabled(&self) -> bool {
        pa_core::settings::SettingsManager::create(&self.cwd, &self.agent_dir)
            .get_agent_traces_enabled()
    }

    fn set_agent_traces_enabled(&self, enabled: bool) -> Result<()> {
        let mut settings = pa_core::settings::SettingsManager::create(&self.cwd, &self.agent_dir);
        settings.set_agent_traces_enabled(enabled)
    }

    fn mark_onboarding_complete(&self) -> Result<()> {
        let mut settings = pa_core::settings::SettingsManager::create(&self.cwd, &self.agent_dir);
        settings.set_onboarding_shown(true)?;
        // `onboarding completed` (schema v1): the Rust onboarding flow is the
        // trace question, so outcome is always success and no auth/provider
        // step runs (auth_category `none`). Best-effort like all telemetry.
        if !crate::mode::telemetry_disabled(&settings) {
            let client =
                pa_core::session_engine::telemetry::build_client(&settings, &self.agent_dir);
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set(
                "duration_ms",
                serde_json::Value::from(self.created_at.elapsed().as_millis() as u64),
            );
            properties.set("outcome", serde_json::Value::from("success"));
            properties.set("auth_category", serde_json::Value::from("none"));
            properties.set("provider_category", serde_json::Value::from("unknown"));
            client.track("onboarding completed", properties);
        }
        Ok(())
    }
}

/// TS `shouldRunOnboarding` + `isOnboardingModelReady`: first run is defined
/// by the settings flag alone, but the flow only shows the trace question
/// (no login sequence) when the startup model resolves and has configured
/// auth. The startup model follows the TS `findInitialModel` chain —
/// explicit flags, the `--models` scope, the saved settings default, the
/// featured default, the first available model — so a flagless launch with
/// a configured default reaches the trace question exactly like TS. The
/// TS non-ready path (sign-in + provider picker) is not ported yet: a
/// first launch that resolves no usable model skips the notice.
fn onboarding_task(options: &RunOptions) -> Option<pa_tui::interactive::OnboardingTask> {
    let config = &options.config;
    let settings = pa_core::settings::SettingsManager::create(&config.cwd, &config.agent_dir);
    if settings.get_onboarding_shown() {
        return None;
    }
    if !startup_model_ready(options, &settings) {
        return None;
    }
    Some(pa_tui::interactive::OnboardingTask {
        sink: std::sync::Arc::new(SettingsOnboardingSink {
            cwd: config.cwd.clone(),
            agent_dir: config.agent_dir.clone(),
            created_at: std::time::Instant::now(),
        }),
    })
}

/// TS `isOnboardingModelReady` for the startup model: the resolved model
/// must have configured auth (auth storage, an environment credential, or
/// the models.json provider key). An explicit `--api-key` rides the
/// resolved model's provider as a runtime key, so it counts too.
fn startup_model_ready(
    options: &RunOptions,
    settings: &pa_core::settings::SettingsManager,
) -> bool {
    let config = &options.config;
    let auth = pa_core::auth::AuthStorage::create(&config.agent_dir);
    let mut registry =
        pa_core::models::ModelRegistry::create(auth, config.agent_dir.join("models.json"));
    // Sync resolution on a fresh registry must adopt the on-disk private
    // authorization cache before `get_available` (same rule as the daemon
    // create path).
    registry.load_private_authorization_from_cache();
    let all: Vec<pa_types::ai::Model> = registry.get_all().to_vec();
    let available: Vec<pa_types::ai::Model> =
        registry.get_available().into_iter().cloned().collect();
    let scoped = config
        .models
        .as_deref()
        .map(|patterns| pa_core::models::resolve_model_scope_from_models(patterns, &available))
        .unwrap_or_default();
    let is_continuing = options.session.resume.is_some() || options.session.continue_recent;
    let startup_model =
        pa_core::models::find_initial_model(&pa_core::models::InitialModelOptions {
            cli_provider: config.provider.as_deref(),
            cli_model: config.model.as_deref(),
            scoped_models: &scoped,
            is_continuing,
            default_provider: settings.get_default_provider(),
            default_model_id: settings.get_default_model(),
            all_models: &all,
            available_models: &available,
        });
    match startup_model {
        Some(model) => registry.has_configured_auth(&model) || config.api_key.is_some(),
        None => false,
    }
}

/// `tui scroll used` / `tui exit` adoption telemetry: a one-shot client per
/// event, tracked and flushed at the emission point (the `startup`-event
/// pattern). Telemetry must never fail the session: opt-out or a broken
/// install id drops the event.
struct CliInteractionTelemetry {
    cwd: PathBuf,
    agent_dir: PathBuf,
}

impl CliInteractionTelemetry {
    /// A one-shot client, or `None` when telemetry is opted out.
    fn client(&self) -> Option<pa_telemetry::TelemetryClient> {
        let settings = pa_core::settings::SettingsManager::create(&self.cwd, &self.agent_dir);
        if crate::mode::telemetry_disabled(&settings) {
            return None;
        }
        Some(pa_core::session_engine::telemetry::build_client(
            &settings,
            &self.agent_dir,
        ))
    }
}

impl pa_tui::interactive::InteractionTelemetry for CliInteractionTelemetry {
    fn scroll_used(
        &self,
        action: &'static str,
        resumed_following: bool,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("action", serde_json::Value::from(action));
            properties.set(
                "resumed_following",
                serde_json::Value::from(resumed_following),
            );
            client.track("tui scroll used", properties);
            let _ = client.shutdown().await;
        })
    }

    fn client_exit(
        &self,
        reason: &'static str,
        turn_active: bool,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let Some(client) = self.client() else {
                return;
            };
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("exit_reason", serde_json::Value::from(reason));
            properties.set("turn_active", serde_json::Value::from(turn_active));
            client.track("tui exit", properties);
            let _ = client.shutdown().await;
        })
    }
}

/// Run the interactive TUI attached to the daemon. Returns the exit code.
pub fn run_interactive_mode(options: &RunOptions) -> Result<i32> {
    let socket_path = resolve_socket_path(options.daemon_socket.as_deref());
    let tui_options = build_tui_options(options, socket_path)?;
    // Telemetry disclosure (TS agent-session-services): once per
    // installation, only after onboarding marked itself shown (a first
    // interactive run belongs to the onboarding screen; the notice surfaces
    // on the next launch). Divergence from TS: the TS product renders it as
    // (a session diagnostic in the TUI; the Rust build prints it to stderr
    // before the TUI starts, which keeps the same text visible without a
    // daemon-side diagnostics round-trip).
    if !tui_options.telemetry_disabled.unwrap_or(false) {
        let mut settings = pa_core::settings::SettingsManager::create(
            &options.config.cwd,
            &options.config.agent_dir,
        );
        if settings.get_onboarding_shown() && !settings.get_telemetry_notice_shown() {
            eprintln!(
                "Prime Agent sends pseudonymous usage and performance metrics without prompts, responses, tool content, file paths, or repository data. Disable this with telemetry.enabled=false, PRIME_AGENT_TELEMETRY=0, DO_NOT_TRACK=1, or offline mode."
            );
            if let Err(error) = settings.set_telemetry_notice_shown(true) {
                eprintln!("Warning: could not persist the telemetry notice: {error}");
            }
        }
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build the interactive runtime")?;
    let startup_started = std::time::Instant::now();
    runtime.block_on(async {
        ensure_daemon_running(&tui_options.socket_path, &tui_options.cwd).await?;
        // `startup` (schema v1): process entry to a ready interactive
        // session environment (daemon listening). Emitted through a
        // one-shot client that flushes immediately; the session's own
        // telemetry rides the daemon worker.
        if !options.config.telemetry_disabled {
            let agent_dir = options.config.agent_dir.clone();
            let settings =
                pa_core::settings::SettingsManager::create(&options.config.cwd, &agent_dir);
            let client = pa_core::session_engine::telemetry::build_client(&settings, &agent_dir);
            let daemon_ready_ms = startup_started.elapsed().as_millis() as u64;
            let mut properties = pa_telemetry::base_properties("interactive");
            properties.set("duration_ms", serde_json::Value::from(daemon_ready_ms));
            let mut phase_timings = pa_telemetry::Properties::new();
            phase_timings.set("daemon_ready", serde_json::Value::from(daemon_ready_ms));
            properties.set_map("phase_timings", &phase_timings);
            client.track("startup", properties);
            let _ = client.shutdown().await;
        }
        // `prime-agent agents` and bare `--resume` open the agents view
        // (TS `agentsViewRequested`); the view then opens sessions, and a
        // session exits back into the view until the user exits it. TS gates
        // the explicit `agents` request on completed onboarding (a fresh
        // install shows the first-run notice first); bare `--resume` opens
        // the view regardless.
        let agents_view = options.session.resume_bare
            || (options.agents_view_requested && tui_options.onboarding.is_none());
        if agents_view {
            run_agents_view_flow(tui_options, None).await
        } else {
            let outcome =
                pa_tui::interactive::run_interactive(tui_options.clone(), UiMode::Terminal).await?;
            // TS `main.ts`: a direct session run closes into the agents view
            // when the exit came through agents-back or `/resume`
            // (`launchAgentsView` anchored on the session just left); every
            // other exit (ctrl+c/ctrl+d, `/quit`) ends the process.
            if outcome.return_to_agents_view {
                run_agents_view_flow(tui_options, Some(outcome.session_id.clone())).await
            } else {
                print_resume_hint(&outcome.resume_hint);
                Ok(())
            }
        }
    })?;
    // tmux (verified on 3.2a) can drop the pane's final output when the
    // process dies immediately after writing it: the just-printed resume
    // hint — and the tail of the exit flush — races the pane-death
    // handling and the dead pane comes up blank. Holding the process
    // briefly after the last write lets the terminal apply it first. The
    // TS product wins this race only by exiting slower (its input drain
    // plus node teardown); the bound stays far inside the exit-within-1s
    // contract.
    std::thread::sleep(Duration::from_millis(300));
    Ok(0)
}

/// TS `shutdown` prints the dim resume hint (`formatResumeHint`) to stdout
/// after the TUI is restored; agents-view returns suppress it. Dim is the
/// TS `chalk.dim` styling (`ESC[2m` ... `ESC[22m`).
fn print_resume_hint(hint: &Option<String>) {
    if let Some(hint) = hint {
        println!("\x1b[2m{hint}\x1b[22m");
    }
}

/// The agents-view loop: open the view, run the session it opens, and return
/// to the view when the session detaches through agents-back or bare
/// `/resume` (TS `InteractiveMode.run` returning `agents_view`). Every other
/// session exit — ctrl+c/ctrl+d, `/quit`, `/exit` — ends the whole app (TS
/// `shutdown()` exits the process instead of reopening the view). A
/// `/resume <selector>` chain runs its target before the loop decides again.
async fn run_agents_view_flow(base: InteractiveOptions, anchor: Option<String>) -> Result<()> {
    let mut anchor = anchor;
    loop {
        let view_options = pa_tui::agents_view::AgentsViewOptions {
            socket_path: base.socket_path.clone(),
            cwd: base.cwd.clone(),
            session_dir: base.session_dir.clone(),
            theme: base.theme.clone(),
            version: base.version.clone(),
            anchor_session_id: anchor.clone(),
        };
        let view = pa_tui::agents_view::run_agents_view(
            view_options,
            pa_tui::agents_view::AgentsViewUiMode::Terminal,
        )
        .await?;
        let Some(selection) = view.selection else {
            return Ok(());
        };
        let mut session_options = base.clone();
        session_options.session = selection;
        let outcome =
            pa_tui::interactive::run_interactive(session_options, UiMode::Terminal).await?;
        anchor = Some(outcome.session_id.clone());
        if !outcome.return_to_agents_view {
            print_resume_hint(&outcome.resume_hint);
            return Ok(());
        }
        // `/resume <selector>` routes straight to that session before the
        // loop reopens the view.
        let mut pending = outcome.selection_request;
        while let Some(selection) = pending.take() {
            let mut next = base.clone();
            next.session = selection;
            let outcome = pa_tui::interactive::run_interactive(next, UiMode::Terminal).await?;
            anchor = Some(outcome.session_id.clone());
            if !outcome.return_to_agents_view {
                print_resume_hint(&outcome.resume_hint);
                return Ok(());
            }
            pending = outcome.selection_request;
        }
    }
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
    // The chat markdown code-block indent reads the effective settings on
    // startup (TS `getCodeBlockIndent` -> `getMarkdownThemeWithSettings`).
    let code_block_indent =
        pa_core::settings::SettingsManager::create(&config.cwd, &config.agent_dir)
            .get_code_block_indent();
    // The `/model` picker catalog: a startup snapshot of the available
    // models (same registry and private-authorization cache adoption as
    // the startup-model chain; entitlement refreshes run daemon-side, so
    // the picker works off the snapshot).
    let auth = pa_core::auth::AuthStorage::create(&config.agent_dir);
    let mut registry =
        pa_core::models::ModelRegistry::create(auth, config.agent_dir.join("models.json"));
    registry.load_private_authorization_from_cache();
    let model_catalog: Vec<pa_types::ai::Model> =
        registry.get_available().into_iter().cloned().collect();
    Ok(InteractiveOptions {
        code_block_indent,
        model_catalog,
        socket_path,
        cwd: config.cwd.clone(),
        session_dir,
        script_path,
        // Explicit CLI model flags ride every create request (TS
        // runtime-config propagation): the daemon worker must treat them as
        // authoritative, not fall back to a process-wide model.
        model_selection: ModelSelection {
            provider: config.provider.clone(),
            model: config.model.clone(),
            api_key: config.api_key.clone(),
            // The `--thinking` flag rides the same create-config path:
            // the worker clamps it to the model's supported levels.
            thinking: config.thinking,
        },
        no_session: options.session.no_session,
        session,
        initial_message: options.initial_message.clone(),
        theme: String::new(),
        version: crate::config::version().to_string(),
        // The startup-model chain (PR lane): the task is built from the full
        // run options so the resolved startup model gates the notice.
        onboarding: onboarding_task(options),
        // Only Some(true) rides the wire (TS `telemetryDisabled`).
        telemetry_disabled: config.telemetry_disabled.then_some(true),
        // `/mcp login` / `/mcp logout`: the client-side auth flows run in
        // this process (the TS interactive client's placement) and persist
        // through the shared auth store the daemon's sessions read.
        client_auth: Some(pa_tui::client_auth::ClientAuthCommandsHandle(
            std::sync::Arc::new(crate::mcp_login::TerminalMcpAuth::new(
                config.cwd.clone(),
                config.agent_dir.clone(),
            )),
        )),
        telemetry: Some(std::sync::Arc::new(CliInteractionTelemetry {
            cwd: config.cwd.clone(),
            agent_dir: config.agent_dir.clone(),
        })),
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

    #[test]
    fn onboarding_gate_follows_settings_and_auth() {
        use crate::mode::{AppMode, RuntimeConfig};

        fn run_options(dir: &std::path::Path) -> RunOptions {
            RunOptions {
                app_mode: AppMode::Interactive,
                config: RuntimeConfig {
                    cwd: dir.to_path_buf(),
                    agent_dir: dir.join("agent"),
                    ..Default::default()
                },
                session: Default::default(),
                messages: Vec::new(),
                file_args: Vec::new(),
                daemon_socket: None,
                list_models: None,
                export: None,
                initial_message: None,
                verbose: false,
                offline: false,
                agents_view_requested: false,
                attach_agent: None,
            }
        }

        let dir = tempfile::TempDir::new().expect("temp dir");
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&agent).expect("agent dir");

        // A completed onboarding never reopens, regardless of model state.
        let mut settings = pa_core::settings::SettingsManager::create(dir.path(), &agent);
        settings.set_onboarding_shown(true).expect("set flag");
        let options = run_options(dir.path());
        assert!(onboarding_task(&options).is_none());
        // Back to a first run for the readiness checks below.
        settings.set_onboarding_shown(false).expect("reset flag");

        // Flagless launch: a models.json provider key + saved default model
        // resolve the startup model, so the trace question shows (TS
        // `isOnboardingModelReady` over the `findInitialModel` chain).
        std::fs::write(
            agent.join("models.json"),
            r#"{ "providers": {
                "onboard-test": {
                    "baseUrl": "https://onboard.test", "apiKey": "sk-onboard",
                    "api": "openai-completions",
                    "models": [ { "id": "m1", "name": "M1" } ]
                },
                "onboard-naked": {
                    "baseUrl": "https://naked.test",
                    // Present (custom models require "apiKey", same as TS)
                    // but the `!command` resolves to no credential, so the
                    // provider stays unauthorized.
                    "apiKey": "!exit 1",
                    "api": "openai-completions",
                    "models": [ { "id": "m2", "name": "M2" } ]
                }
            } }"#,
        )
        .expect("models.json");
        let mut settings = pa_core::settings::SettingsManager::create(dir.path(), &agent);
        settings
            .set_default_model_and_provider("onboard-test".into(), "m1".into())
            .expect("saved default");
        let options = run_options(dir.path());
        assert!(onboarding_task(&options).is_some());

        // Explicit flags that resolve to a provider without configured auth
        // leave the model not ready: no trace question. TS `validateConfig`
        // requires an "apiKey" for custom providers, but a `!command` key
        // that fails resolves to nothing (TS `resolveConfigValue`), so the
        // provider stays unauthenticated.
        let mut options = run_options(dir.path());
        options.config.provider = Some("onboard-naked".into());
        options.config.model = Some("m2".into());
        assert!(onboarding_task(&options).is_none());
    }

    #[test]
    fn build_tui_options_reads_code_block_indent_settings() {
        // `markdown.codeBlockIndent` rides InteractiveOptions at startup
        // (TS `getCodeBlockIndent` -> `getMarkdownThemeWithSettings`); a
        // non-default value reaches the TUI, and no setting keeps the TS
        // default two spaces.
        fn run_options(dir: &std::path::Path) -> RunOptions {
            RunOptions {
                app_mode: crate::mode::AppMode::Interactive,
                config: crate::mode::RuntimeConfig {
                    cwd: dir.to_path_buf(),
                    agent_dir: dir.join("agent"),
                    ..Default::default()
                },
                session: Default::default(),
                messages: Vec::new(),
                file_args: Vec::new(),
                daemon_socket: None,
                list_models: None,
                export: None,
                initial_message: None,
                verbose: false,
                offline: false,
                agents_view_requested: false,
                attach_agent: None,
            }
        }

        let dir = tempfile::TempDir::new().expect("temp dir");
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&agent).expect("agent dir");
        std::fs::write(
            agent.join("settings.json"),
            r#"{ "markdown": { "codeBlockIndent": "    " } }"#,
        )
        .expect("settings.json");
        let options = build_tui_options(&run_options(dir.path()), dir.path().join("d.sock"))
            .expect("options");
        assert_eq!(options.code_block_indent, "    ");

        // No markdown settings: the TS default.
        let bare = tempfile::TempDir::new().expect("temp dir");
        std::fs::create_dir_all(bare.path().join("agent")).expect("agent dir");
        let options = build_tui_options(&run_options(bare.path()), bare.path().join("d.sock"))
            .expect("options");
        assert_eq!(options.code_block_indent, "  ");
    }
}
