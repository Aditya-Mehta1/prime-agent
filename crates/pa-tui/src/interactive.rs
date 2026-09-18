//! Interactive agent session over the daemon: attach to a live session, send
//! prompts, render streamed assistant output, and switch sessions. This is
//! the interactive product surface of `crates/pa-tui` (the port of the
//! interactive mode's daemon-attach path); the session loop itself keeps
//! running in the daemon worker, so closing the UI detaches instead of
//! stopping the session.
//!
//! Two UI sources drive the same loop: a crossterm terminal (raw mode, alt
//! screen) and a headless plan (programmatic input, captured frames). The
//! headless source is the verifier seam: it exercises the identical
//! attach/submit/stream/render path without a TTY.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::daemon_client::DaemonClient;
use crate::session_ui::SessionUi;
use crate::view::AgentView;

use crossterm::event::KeyEvent;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

/// Which session the interactive run opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSelection {
    /// Create a fresh session (`create`, then `attach`).
    New,
    /// Attach an existing live session by active session id.
    Attach(String),
    /// Create with `continueRecent`: the supervisor picks the most recent
    /// saved session for the cwd.
    ContinueRecent,
    /// Create with `sessionPath`: reopen a saved session file (`--resume`).
    Resume(PathBuf),
}

/// Explicit model selection carried into every `create` config: the CLI
/// `--provider`/`--model`/`--api-key`/`--thinking` flags. Explicit flags are
/// authoritative end-to-end — the daemon worker resolves its session model
/// and thinking level from this selection instead of a process-wide fallback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelSelection {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    /// The requested thinking level (`--thinking`). The worker clamps it to
    /// the model's supported levels and records the effective level.
    pub thinking: Option<pa_types::ai::ModelThinkingLevel>,
}

/// Persistence for the first-run onboarding answers. The TUI crate owns
/// only the surface; the composition root (pa-cli) implements the sink
/// against the settings manager, keeping pa-tui decoupled from pa-core.
pub trait OnboardingSink: Send + Sync {
    /// The persisted trace-sharing answer (TS `getAgentTracesEnabled`).
    fn agent_traces_enabled(&self) -> bool;
    /// Persist the trace-sharing answer (TS `setAgentTracesEnabled`).
    fn set_agent_traces_enabled(&self, enabled: bool) -> anyhow::Result<()>;
    /// Mark the onboarding flow completed (TS `markOnboardingShown` +
    /// `flush`); an aborted flow leaves the flag unset.
    fn mark_onboarding_complete(&self) -> anyhow::Result<()>;
}

/// The first-run flow to run before the session screen (TS
/// `runStartupOnboarding`, model-ready branch: splash + trace question).
#[derive(Clone)]
pub struct OnboardingTask {
    pub sink: std::sync::Arc<dyn OnboardingSink>,
}

impl std::fmt::Debug for OnboardingTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnboardingTask").finish()
    }
}

/// Options for one interactive run.
#[derive(Debug, Clone)]
pub struct InteractiveOptions {
    pub socket_path: PathBuf,
    pub cwd: PathBuf,
    /// Persistence directory for new sessions (`sessionDir` in the create
    /// config; defaults to the daemon's sessions dir when `None`).
    pub session_dir: Option<PathBuf>,
    /// Scripted faux-engine script path. Verification seam only; the product
    /// never sets it.
    pub script_path: Option<PathBuf>,
    /// Model flags to carry into the create config.
    pub model_selection: ModelSelection,
    /// Create without a session file (`--no-session`).
    pub no_session: bool,
    pub session: SessionSelection,
    /// Prompt sent immediately after attach (CLI message arguments).
    pub initial_message: Option<String>,
    pub theme: String,
    /// Product version for the brand splash.
    pub version: String,
    /// Run the first-run onboarding flow before the session screen.
    pub onboarding: Option<OnboardingTask>,
    /// Telemetry opt-out (TS `telemetryDisabled`): `Some(true)` only when
    /// the invocation disabled telemetry; carried on create/attach so the
    /// daemon worker installs no telemetry subscriber and attach obeys the
    /// TS `assertTelemetryAttachAllowed` guard.
    pub telemetry_disabled: Option<bool>,
    /// `/mcp login` / `/mcp logout`: the client-side auth flows the
    /// composition root provides (login suspends the TUI and prompts on
    /// the terminal). `None` reports the commands as unavailable.
    pub client_auth: Option<crate::client_auth::ClientAuthCommandsHandle>,
}

impl InteractiveOptions {
    /// The `create` config carried on every new-session request.
    pub(crate) fn create_config(&self) -> Value {
        let mut config = json!({ "cwd": self.cwd.display().to_string() });
        if let Some(session_dir) = &self.session_dir {
            config["sessionDir"] = json!(session_dir.display().to_string());
        }
        if let Some(script) = &self.script_path {
            config["script"] = json!(script.display().to_string());
        }
        if let Some(provider) = &self.model_selection.provider {
            config["provider"] = json!(provider);
        }
        if let Some(model) = &self.model_selection.model {
            config["model"] = json!(model);
        }
        if let Some(api_key) = &self.model_selection.api_key {
            config["apiKey"] = json!(api_key);
        }
        if let Some(thinking) = self.model_selection.thinking {
            config["thinking"] = json!(thinking.wire_name());
        }
        config
    }
}

/// How the UI is driven.
pub enum UiMode {
    /// Raw-mode terminal on stdout.
    Terminal,
    /// Headless plan: submitted prompts plus idle barriers, with rendered
    /// frames captured for assertions.
    Headless(HeadlessPlan),
}

/// A scripted headless run.
#[derive(Debug, Clone)]
pub struct HeadlessPlan {
    pub steps: Vec<HeadlessStep>,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone)]
pub enum HeadlessStep {
    /// Submit text (the same editor submit path as a user typing it).
    Submit(String),
    /// Type text character by character (raw editor input, so autocomplete
    /// and editor state react exactly as to a keystroke).
    Type(String),
    /// Hold until the current turn finishes (bounded by `timeout_ms`).
    WaitIdle { timeout_ms: u64 },
}

/// One typed string as key events: characters become `Char` presses, `\n`
/// becomes Enter, and `\t` becomes Tab (the keys autocomplete reacts to).
fn typed_keys(text: &str) -> Vec<KeyEvent> {
    text.chars()
        .map(|c| match c {
            '\n' | '\r' => KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
            '\t' => KeyEvent::new(
                crossterm::event::KeyCode::Tab,
                crossterm::event::KeyModifiers::NONE,
            ),
            other => KeyEvent::new(
                crossterm::event::KeyCode::Char(other),
                crossterm::event::KeyModifiers::NONE,
            ),
        })
        .collect()
}

/// Drive the onboarding pane until the trace question settles. Returns
/// `true` when the exit keys quit the app (TS `onExit` → shutdown).
async fn run_onboarding_phase(
    task: &OnboardingTask,
    view: &mut AgentView,
    ui_rx: &mut mpsc::UnboundedReceiver<UiInput>,
    renderer: &mut Renderer,
) -> Result<bool> {
    // TS model-ready branch: a user who already opted into traces sees no
    // flow at all — the flow completes silently and marks itself seen.
    if task.sink.agent_traces_enabled() {
        let _ = task.sink.mark_onboarding_complete();
        return Ok(false);
    }
    let mut screen = crate::onboarding::OnboardingScreen::new();
    let keybindings = crate::keybindings::KeybindingsManager::new();
    let mut exit_requested = false;
    loop {
        tokio::select! {
            maybe_input = ui_rx.recv() => {
                if let Some(UiInput::Key(key)) = maybe_input {
                    let Some(key_id) = crate::keys::key_event_to_id(&key) else {
                        continue;
                    };
                    match screen.handle_key(&key_id, &keybindings) {
                        Some(crate::onboarding::OnboardingDecision::Selected(index)) => {
                            // `Share` opts in; `Not now` keeps traces off
                            // (TS finish(index === 0)). A cancel writes no
                            // answer at all, but the flow still completed.
                            let _ = task.sink.set_agent_traces_enabled(index == 0);
                            let _ = task.sink.mark_onboarding_complete();
                            break;
                        }
                        Some(crate::onboarding::OnboardingDecision::Cancelled) => {
                            let _ = task.sink.mark_onboarding_complete();
                            break;
                        }
                        Some(crate::onboarding::OnboardingDecision::Exit) => {
                            exit_requested = true;
                            break;
                        }
                        None => {}
                    }
                }
            }
            // The field animates behind the flow panels until dismissal
            // (TS ANIMATION_INTERVAL_MS).
            _ = tokio::time::sleep(Duration::from_millis(120)) => {
                screen.tick();
            }
        }
        view.onboarding = Some(screen.clone());
        if let Some(renderer) = renderer.is_terminal_mut() {
            crate::app::draw(renderer, view)?;
        }
        view.onboarding = None;
    }
    if exit_requested {
        return Ok(true);
    }
    Ok(false)
}

/// Result of an interactive run: session identity plus, in headless mode, the
/// rendered frames.
#[derive(Debug, Clone, Default)]
pub struct InteractiveOutcome {
    pub active_session_id: String,
    pub session_id: String,
    pub last_assistant_text: Option<String>,
    pub frames: Vec<String>,
    /// `/resume` requested the agents view next (return-to-session flow).
    pub return_to_agents_view: bool,
    /// `/resume <selector>` requested this session next.
    pub selection_request: Option<SessionSelection>,
}

/// Inputs consumed by the UI loop. Terminal keys arrive one event at a time;
/// headless steps arrive as whole submissions.
enum UiInput {
    Key(KeyEvent),
    Paste(String),
    Submit(String),
    WaitIdle { timeout_ms: u64 },
    HeadlessDone,
}

/// Run the interactive UI until the user exits (terminal) or the plan
/// completes (headless).
pub async fn run_interactive(
    options: InteractiveOptions,
    ui: UiMode,
) -> Result<InteractiveOutcome> {
    // The TS theme emits raw ANSI color codes regardless of NO_COLOR; match
    // that so the same terminal renders the same frames either way.
    crossterm::style::force_color_output(true);
    let (client, mut events) = DaemonClient::connect(&options.socket_path)
        .await
        .with_context(|| "the interactive UI could not attach to the daemon")?;
    let mut session = SessionUi::open(client, &options).await?;

    let theme = crate::app::load_theme(&options.theme);
    let mut view = AgentView::new(theme);
    apply_startup_chrome(&mut view, &options);
    session.refresh_stats().await;
    session.rebuild_view(&mut view);
    if let Some(notice) = check_tmux_keyboard_setup().await {
        view.push_entry(crate::chat::ChatEntry::Status {
            text: format!("\u{26a0} {notice}"),
            kind: crate::chat::StatusKind::Warning,
        });
        session.dirty = true;
    }
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiInput>();
    let mut renderer = Renderer::setup(ui, ui_tx)?;
    // First-run onboarding owns the pane before the session screen (TS
    // `runStartupOnboarding`, model-ready branch: splash + trace question).
    // Headless harness runs have no terminal to draw it on and skip it.
    if let Some(task) = options.onboarding.clone() {
        let exit_requested =
            run_onboarding_phase(&task, &mut view, &mut ui_rx, &mut renderer).await?;
        if exit_requested {
            let _ = session.detach().await;
            return Ok(InteractiveOutcome {
                active_session_id: session.active_session_id.clone(),
                session_id: session.session_id.clone(),
                last_assistant_text: None,
                frames: Vec::new(),
                // Onboarding exit leaves no session open; no return-to-view
                // or pending selection applies.
                return_to_agents_view: false,
                selection_request: None,
            });
        }
    }
    if let Some(initial) = &options.initial_message {
        session.submit_prompt(initial, &mut view).await?;
    }

    let mut pending: VecDeque<UiInput> = VecDeque::new();
    let mut running = true;
    let mut headless_done = false;
    let mut wait_idle_deadline: Option<Instant> = None;

    while running {
        // Process one queued UI input. A WaitIdle step is a barrier: it stays
        // at the head of the queue until the turn finishes (or its deadline).
        if let Some(UiInput::WaitIdle { timeout_ms }) = pending.front() {
            let timeout_ms = *timeout_ms;
            if session.turn_active {
                if wait_idle_deadline.is_none() {
                    wait_idle_deadline = Some(Instant::now() + Duration::from_millis(timeout_ms));
                } else if Instant::now() > wait_idle_deadline.unwrap() {
                    wait_idle_deadline = None;
                    pending.pop_front();
                    session.note("timed out waiting for the turn to finish", &mut view);
                }
            } else {
                wait_idle_deadline = None;
                pending.pop_front();
            }
        } else if let Some(input) = pending.pop_front() {
            session.dirty = true;
            match input {
                UiInput::Key(key) => {
                    session.handle_key(key, &mut view, &mut running).await?;
                }
                UiInput::Paste(text) => {
                    let _ = view.editor.handle_paste(&text);
                }
                UiInput::Submit(text) => {
                    // A terminal-suspending client command (`/mcp login`):
                    // the auth flow prompts on the plain terminal.
                    let suspended = session.needs_terminal_suspension(&text);
                    if suspended {
                        renderer.suspend()?;
                    }
                    let dispatched = session.submit_prompt(&text, &mut view).await;
                    if suspended {
                        renderer.resume()?;
                    }
                    dispatched?;
                }
                UiInput::HeadlessDone => headless_done = true,
                UiInput::WaitIdle { .. } => unreachable!("barrier handled above"),
            }
            // Paint the handled input in this iteration: the select below can
            // otherwise wait out its 50ms tick before the next draw, and
            // that wait is felt directly as keystroke-to-render lag.
            if let Some(renderer) = renderer.is_terminal_mut() {
                crate::app::draw(renderer, &mut view)?;
                session.dirty = false;
            }
        }
        if headless_done
            && pending.is_empty()
            && !session.turn_active
            && wait_idle_deadline.is_none()
            && !session.dirty
        {
            break;
        }

        let was_active = session.turn_active;
        tokio::select! {
            maybe_event = events.recv() => {
                match maybe_event {
                    Some(event) => {
                        session.apply_client_event(event, &mut view);
                        // A settled turn refreshes the tray's context usage.
                        if was_active && !session.turn_active {
                            session.refresh_stats().await;
                            session.rebuild_tray(&mut view);
                        }
                    }
                    None => {
                        session.note("the daemon connection closed", &mut view);
                        running = false;
                    }
                }
            }
            maybe_input = ui_rx.recv() => {
                if let Some(input) = maybe_input {
                    pending.push_back(input);
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }

        // Spinner animation: the loader frame advances while a turn runs.
        if session.turn_active {
            view.pulse_frame = view.pulse_frame.wrapping_add(1);
        }

        if let Some(renderer) = renderer.is_terminal_mut() {
            crate::app::draw(renderer, &mut view)?;
            session.dirty = false;
        } else if session.dirty {
            renderer.render_headless(&mut session, &mut view);
        }
        if session.exit_requested {
            running = false;
        }
    }

    // Detach explicitly so the session's attached-client count stays honest;
    // the supervisor also detaches this connection when the socket closes.
    let _ = session.detach().await;
    let outcome = InteractiveOutcome {
        active_session_id: session.active_session_id.clone(),
        session_id: session.session_id.clone(),
        last_assistant_text: session.last_assistant_text.clone(),
        frames: renderer.finish(),
        return_to_agents_view: session.open_agents_view,
        selection_request: session.pending_selection,
    };
    session.client.close();
    Ok(outcome)
}

/// Seed the static chrome state for a fresh interactive run: splash
/// version/cwd, top-bar name, and the `manage` hint for persisted sessions.
fn apply_startup_chrome(view: &mut AgentView, options: &InteractiveOptions) {
    view.chrome.version = options.version.clone();
    view.chrome.cwd = options.cwd.to_string_lossy().to_string();
    view.chrome.chat_name = crate::chrome::display_name(&view.chrome.cwd);
    view.chrome.show_manage = !options.no_session;
}

/// The tmux keyboard notice (TS `checkTmuxKeyboardSetup`): warn once per
/// start when tmux runs without `extended-keys`. Runs `tmux show` read-only
/// against the ambient socket; a timeout or error suppresses the notice.
async fn check_tmux_keyboard_setup() -> Option<String> {
    if std::env::var("TMUX").is_err() {
        return None;
    }
    let query = |option: &'static str| async move {
        tokio::time::timeout(
            Duration::from_millis(2_000),
            tokio::task::spawn_blocking(move || {
                std::process::Command::new("tmux")
                    .args(["show", "-gv", option])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
            }),
        )
        .await
        .ok()
        .and_then(|joined| joined.ok())
        .and_then(|output| output.ok())
        .and_then(|output| {
            if output.status.success() {
                Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                None
            }
        })
    };
    let extended_keys = query("extended-keys").await?;
    if extended_keys != "on" && extended_keys != "always" {
        return Some(
            "tmux extended-keys is off. Modified Enter keys may not work. Add `set -g extended-keys on` to ~/.tmux.conf and restart tmux.".to_string(),
        );
    }
    None
}

/// Rendering sink: the real terminal or headless frame capture.
enum Renderer {
    Terminal(Terminal<CrosstermBackend<std::io::Stdout>>),
    Headless {
        width: u16,
        height: u16,
        frames: Vec<String>,
    },
}

impl Renderer {
    fn setup(ui: UiMode, ui_tx: mpsc::UnboundedSender<UiInput>) -> Result<Renderer> {
        match ui {
            UiMode::Terminal => {
                terminal::enable_raw_mode()?;
                crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
                // One reader thread feeds the loop; crossterm events are
                // process-global, so the reader registry joins the previous
                // surface's reader before this one starts polling.
                crate::input::spawn_terminal_reader(move |event| match event {
                    crossterm::event::Event::Key(key) => ui_tx.send(UiInput::Key(key)).is_ok(),
                    crossterm::event::Event::Paste(text) => {
                        ui_tx.send(UiInput::Paste(text)).is_ok()
                    }
                    _ => true,
                });
                let backend = CrosstermBackend::new(std::io::stdout());
                Ok(Renderer::Terminal(Terminal::new(backend)?))
            }
            UiMode::Headless(plan) => {
                let steps = plan.steps;
                tokio::spawn(async move {
                    for step in steps {
                        match step {
                            HeadlessStep::Submit(text) => {
                                if ui_tx.send(UiInput::Submit(text)).is_err() {
                                    return;
                                }
                            }
                            HeadlessStep::Type(text) => {
                                for key in typed_keys(&text) {
                                    if ui_tx.send(UiInput::Key(key)).is_err() {
                                        return;
                                    }
                                }
                            }
                            HeadlessStep::WaitIdle { timeout_ms } => {
                                if ui_tx.send(UiInput::WaitIdle { timeout_ms }).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    let _ = ui_tx.send(UiInput::HeadlessDone);
                });
                Ok(Renderer::Headless {
                    width: plan.width,
                    height: plan.height,
                    frames: Vec::new(),
                })
            }
        }
    }

    /// Hand the terminal back to the process (raw mode off, alternate
    /// screen left) so an interactive client command can prompt on it.
    /// Headless verification runs keep their plain pipes.
    fn suspend(&mut self) -> Result<()> {
        match self {
            Renderer::Terminal(_) => {
                terminal::disable_raw_mode()?;
                crossterm::execute!(std::io::stdout(), LeaveAlternateScreen)?;
                Ok(())
            }
            Renderer::Headless { .. } => Ok(()),
        }
    }

    /// Take the terminal back after a suspended client command.
    fn resume(&mut self) -> Result<()> {
        match self {
            Renderer::Terminal(terminal) => {
                terminal::enable_raw_mode()?;
                crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
                // A fresh full redraw: the suspended command left arbitrary
                // output behind.
                terminal.clear()?;
                Ok(())
            }
            Renderer::Headless { .. } => Ok(()),
        }
    }

    fn is_terminal_mut(&mut self) -> Option<&mut Terminal<CrosstermBackend<std::io::Stdout>>> {
        match self {
            Renderer::Terminal(terminal) => Some(terminal),
            Renderer::Headless { .. } => None,
        }
    }

    /// Capture one frame as plain text (headless assertions).
    fn render_headless(&mut self, session: &mut SessionUi, view: &mut AgentView) {
        let Renderer::Headless {
            width,
            height,
            frames,
        } = self
        else {
            return;
        };
        let text = crate::app::render_frame_text(view, *width, *height).join("\n");
        if std::env::var("PA_TUI_DEBUG_EVENTS").is_ok() {
            eprintln!(
                "[tui-frame] len={} has_second={} has_again={}",
                text.len(),
                text.contains("second turn"),
                text.contains("again")
            );
        }
        if frames.last().map(String::as_str) != Some(text.as_str()) {
            frames.push(text);
        }
        session.dirty = false;
    }

    fn finish(self) -> Vec<String> {
        match self {
            Renderer::Terminal(_) => {
                let _ = terminal::disable_raw_mode();
                let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
                Vec::new()
            }
            Renderer::Headless { frames, .. } => frames,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(selection: ModelSelection) -> InteractiveOptions {
        InteractiveOptions {
            socket_path: PathBuf::from("/tmp/unused.sock"),
            cwd: PathBuf::from("/tmp"),
            session_dir: None,
            script_path: None,
            model_selection: selection,
            no_session: false,
            session: SessionSelection::New,
            initial_message: None,
            theme: "prime".to_string(),
            version: "0.0.0".to_string(),
            onboarding: None,
            telemetry_disabled: None,
            client_auth: None,
        }
    }

    #[test]
    fn create_config_carries_the_requested_thinking_level() {
        let config = options(ModelSelection {
            thinking: Some(pa_types::ai::ModelThinkingLevel::Max),
            ..Default::default()
        })
        .create_config();
        assert_eq!(config["thinking"], "max");
    }

    #[test]
    fn create_config_omits_thinking_when_no_flag_was_given() {
        let config = options(ModelSelection::default()).create_config();
        assert!(config.get("thinking").is_none());
    }
}
