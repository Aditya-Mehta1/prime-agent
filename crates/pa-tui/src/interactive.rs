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
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::daemon_client::DaemonClient;
use crate::exit_guard::ExitGuard;
use crate::keybindings::KeybindingsManager;
use crate::session_ui::SessionUi;
use crate::view::{AgentView, FlushPlan};

use crossterm::event::KeyEvent;
use crossterm::terminal;
use ratatui::Terminal;
use tokio::sync::mpsc;

/// Cap on the exit-path telemetry flush: the PostHog sink alone allows up
/// to 1.5s, so the exit event must be dropped rather than awaited past the
/// exit-within-1s contract.
const TELEMETRY_EXIT_TIMEOUT_MS: u64 = 500;

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

/// Adoption telemetry for interactive-view interactions (schema v1 events
/// `tui scroll used` and `tui exit`). pa-tui stays pa-types-only, so the
/// composition root implements this against the telemetry client.
/// The seam is object-safe (held as `Arc<dyn InteractionTelemetry>` in the
/// options and session UI), so the async methods return boxed futures with an
/// explicit `Send` bound instead of RPITIT.
pub trait InteractionTelemetry: Send + Sync {
    /// The first transcript scroll action of a run: `action` is
    /// `page_up` / `page_down` / `top` / `follow`.
    fn scroll_used(
        &self,
        action: &'static str,
        resumed_following: bool,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
    /// A builtin client command was submitted (`agent command used`):
    /// `command` is the canonical name (`model`, `effort`, ...). Session
    /// commands report through the session telemetry instead.
    fn command_used(&self, command: &'static str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
    /// How the client run ended: `reason` is `ctrl_c_twice` / `ctrl_d` /
    /// `session_request` / `daemon_closed`, with whether a turn was still
    /// active at exit.
    fn client_exit(
        &self,
        reason: &'static str,
        turn_active: bool,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
    /// The subagent summary line opened the scoped agents view (`tui
    /// subagents open`): `children_total` is the live descendant count at
    /// open time.
    fn subagents_view_opened(
        &self,
        children_total: u64,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
    /// An image was pasted into the editor from the clipboard (event
    /// `tui image pasted`); `mime_type` is the attachment's sniffed format.
    fn image_pasted(&self, mime_type: &str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
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

/// Options for one interactive run. `Debug` skips the telemetry handle (the
/// trait object is not `Debug`).
#[derive(Clone)]
pub struct InteractiveOptions {
    pub socket_path: PathBuf,
    pub cwd: PathBuf,
    /// The model catalog for the `/model` picker (a startup snapshot
    /// resolved by the composition root; pa-tui stays pa-types only, so
    /// the registry itself lives above this crate). The daemon's
    /// `get_model_catalog` refresh replaces it once it lands.
    pub model_catalog: Vec<pa_types::ai::Model>,
    /// Providers with configured auth for the picker's sign-in marking.
    pub model_configured_providers: std::collections::HashSet<String>,
    /// The settings recent-model list (`provider/id` keys, newest first).
    pub model_recent_models: Vec<String>,
    /// The settings default thinking level (the picker's effort seed for
    /// non-reasoning current models).
    pub default_thinking_level: Option<String>,
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
    /// The `terminal.showImages` setting, default true (TS `getShowImages`):
    /// whether image blocks render their metadata rows or the
    /// `[Image: ...]` placeholders.
    pub show_images: bool,
    pub theme: String,
    /// The chat markdown fenced-code indent, resolved by the composition
    /// root from `markdown.codeBlockIndent` (TS `getCodeBlockIndent`;
    /// default two spaces).
    pub code_block_indent: String,
    /// The `/tree` selector's initial filter mode, resolved by the
    /// composition root from the `treeFilterMode` setting (default view).
    pub tree_filter_mode: String,
    /// The `branchSummary.skipPrompt` setting: `/tree` navigation skips the
    /// "Summarize branch?" question and navigates with no summary.
    pub branch_summary_skip_prompt: bool,
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
    /// Adoption telemetry for the interactive view; `None` drops events.
    pub telemetry: Option<std::sync::Arc<dyn InteractionTelemetry>>,
    /// The effective keybindings (defaults merged with the user's
    /// `keybindings.json`, loaded by the composition root; TS
    /// `KeybindingsManager.create()`): every hint and key handler renders
    /// and dispatches through this set.
    pub keybindings: KeybindingsManager,
    /// The attached session's persisted RLM depth (TS `sessionDepth`):
    /// the agents view passes it when it opens a row, and a subagent
    /// session renders its `depth N` tray label.
    pub session_rlm_depth: Option<u32>,
    /// Whether the opened session had direct children (TS
    /// `sessionHasChildren`).
    pub session_has_children: bool,
}

impl std::fmt::Debug for InteractiveOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractiveOptions")
            .field("socket_path", &self.socket_path)
            .field("cwd", &self.cwd)
            .field("session_dir", &self.session_dir)
            .field("script_path", &self.script_path)
            .field("model_selection", &self.model_selection)
            .field("model_catalog", &self.model_catalog)
            .field("no_session", &self.no_session)
            .field("session", &self.session)
            .field("initial_message", &self.initial_message)
            .field("theme", &self.theme)
            .field("code_block_indent", &self.code_block_indent)
            .field("version", &self.version)
            .field("onboarding", &self.onboarding)
            .field("telemetry_disabled", &self.telemetry_disabled)
            .field("client_auth", &self.client_auth)
            .field("keybindings", &self.keybindings.get_effective_config())
            .finish()
    }
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
    /// Materialize the parked editor suggestions — the state a live user
    /// gets after pausing typing for one input-idle tick, so the next step
    /// (typically `Enter`) completes against the open dropdown. A burst of
    /// `Type` steps without this barrier submits as typed, exactly like a
    /// terminal keystroke burst.
    SettleIdle,
    /// Hold until the current turn finishes (bounded by `timeout_ms`).
    WaitIdle { timeout_ms: u64 },
    /// Scroll the transcript to its top row (the `tui.viewport.top` key
    /// path): the verifier's window into the head of the transcript.
    ScrollTop,
    /// One raw key event: the verifier's window into the selector/picker
    /// surfaces (arrows, escape), which typed text cannot express.
    Key(crossterm::event::KeyEvent),
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
    exit_guard: &ExitGuard,
) -> Result<bool> {
    // TS model-ready branch: a user who already opted into traces sees no
    // flow at all — the flow completes silently and marks itself seen.
    if task.sink.agent_traces_enabled() {
        let _ = task.sink.mark_onboarding_complete();
        return Ok(false);
    }
    let mut screen = crate::onboarding::OnboardingScreen::new();
    let keybindings = view.editor.keybindings().clone();
    let mut exit_requested = false;
    loop {
        tokio::select! {
            maybe_input = ui_rx.recv() => {
                if let Some(UiInput::Key(key)) = maybe_input {
                    let Some(key_id) = crate::keys::key_event_to_id(&key) else {
                        continue;
                    };
                    // The onboarding exit keys include Ctrl+C (`app.clear`):
                    // report the handled press so the force-quit guard's
                    // handled counter stays in sync with the reader's
                    // observations.
                    if key_id == "ctrl+c" {
                        exit_guard.note_ctrl_c_handled();
                    }
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
    /// The TS `formatResumeHint` line (a resumable, flushed session), for
    /// the composition root to print after the terminal is restored.
    pub resume_hint: Option<String>,
    pub last_assistant_text: Option<String>,
    pub frames: Vec<String>,
    /// `/resume` requested the agents view next (return-to-session flow).
    pub return_to_agents_view: bool,
    /// The subagent summary line opened the agents view scoped to this
    /// session's subtree; `None` with `return_to_agents_view` means the
    /// plain view.
    pub agents_view_scope: Option<crate::agents_view::AgentsViewScope>,
    /// `/resume <selector>` requested this session next.
    pub selection_request: Option<SessionSelection>,
}

/// Inputs consumed by the UI loop. Terminal keys arrive one event at a time;
/// headless steps arrive as whole submissions.
enum UiInput {
    Key(KeyEvent),
    Paste(String),
    Submit(String),
    /// One materialized input-idle tick (the headless `SettleIdle` step).
    SettleIdle,
    WaitIdle {
        timeout_ms: u64,
    },
    ScrollTop,
    /// The terminal was resized: the next draw repaints the new geometry.
    Resize,
    HeadlessDone,
}

/// Spec §10.2: the client reconnect window after an update restart
/// (10 minutes).
const RECONNECT_WINDOW: Duration = Duration::from_secs(10 * 60);
/// One reconnect attempt's connect budget.
const RECONNECT_ATTEMPT_TIMEOUT_S: u64 = 5;
/// One reconnect attempt's reattach budget: a queued attach can legitimately
/// wait out a slow restore (§10.4), so the attempt hands back to the loop
/// instead of wedging the UI.
const RECONNECT_ATTACH_TIMEOUT_S: u64 = 30;
/// The reconnect backoff cap.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(10);

/// TS `DAEMON_RECONNECT_TIMEOUT_MS`: the bounded session-plane reconnect
/// window after the direct worker link dies.
const SESSION_RECONNECT_WINDOW: Duration = Duration::from_secs(60);
/// TS reconnect backoff cap (`min(2000, 100 * 2 ** min(attempt, 5))`).
const SESSION_RECONNECT_BACKOFF_MAX: Duration = Duration::from_millis(2_000);
/// One re-attach attempt's budget: the attach carries its own request
/// timeouts; this bounds a wedged attempt so the loop reschedules instead
/// of blocking the UI.
const SESSION_RECONNECT_ATTEMPT_TIMEOUT_S: u64 = 10;

/// The interactive loop's session re-attach driver (TS
/// `DaemonAgentConnection.reconnect` over a direct-transport loss): the
/// worker process behind the direct link died, so the attach retries
/// through the supervisor — which respawns the worker and hands out a
/// fresh peer ticket — with the TS backoff inside the TS window.
struct SessionReconnect {
    active_session_id: String,
    deadline: tokio::time::Instant,
    next_attempt: tokio::time::Instant,
    delay: Duration,
    last_error: String,
}

impl SessionReconnect {
    fn start(active_session_id: &str) -> Self {
        SessionReconnect {
            active_session_id: active_session_id.to_string(),
            deadline: tokio::time::Instant::now() + SESSION_RECONNECT_WINDOW,
            next_attempt: tokio::time::Instant::now(),
            delay: Duration::from_millis(100),
            last_error: String::new(),
        }
    }

    /// The next attempt with doubling backoff (capped).
    fn next_attempt(mut self) -> Self {
        self.delay = (self.delay * 2).min(SESSION_RECONNECT_BACKOFF_MAX);
        self.next_attempt = tokio::time::Instant::now() + self.delay;
        self
    }
}

/// The interactive loop's reconnect driver (spec §10.2): attempts with
/// doubling backoff inside the 10-minute window; the user can leave with
/// Ctrl+C at any point (UI input keeps flowing through the same loop).
struct ReconnectLoop {
    deadline: tokio::time::Instant,
    next_attempt: tokio::time::Instant,
    delay: Duration,
}

impl ReconnectLoop {
    fn start(_update: &crate::daemon_client::DaemonClosingUpdate) -> Self {
        let delay = Duration::from_secs(1);
        ReconnectLoop {
            deadline: tokio::time::Instant::now() + RECONNECT_WINDOW,
            next_attempt: tokio::time::Instant::now() + delay,
            delay,
        }
    }

    /// The next attempt with doubling backoff (capped).
    fn next_attempt(mut self) -> Self {
        self.delay = (self.delay * 2).min(RECONNECT_BACKOFF_MAX);
        self.next_attempt = tokio::time::Instant::now() + self.delay;
        self
    }
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
    // Background notes (a failed abort request) fold into the transcript
    // through the same loop that renders daemon events.
    let (notes_tx, mut notes_rx) = mpsc::unbounded_channel::<String>();
    // The `/share` upload task reports here; the loop folds the outcome
    // into the transcript and clears the loader.
    let (share_tx, mut share_rx) = mpsc::unbounded_channel::<crate::session_ui::ShareNote>();
    // The background model-catalog refresh (`get_model_catalog`) reports
    // here; the loop folds it into the picker catalog and any open picker.
    let (catalog_tx, mut catalog_rx) =
        mpsc::unbounded_channel::<crate::session_ui::ModelCatalogUpdate>();
    // The double-Ctrl+C force-quit guard: the terminal reader observes the
    // pair even while this loop is wedged in a daemon request, and a plain
    // std-thread watchdog enforces the exit deadline without the runtime.
    let exit_guard = ExitGuard::new();
    let mut session = SessionUi::open(client, &options, notes_tx, share_tx, catalog_tx).await?;
    session.exit_guard = exit_guard.clone();

    let theme = crate::app::load_theme(&options.theme);
    let mut view = AgentView::new(theme);
    view.code_block_indent = options.code_block_indent.clone();
    // The effective bindings (user `keybindings.json` merged over the TS
    // defaults) drive the editor, the pickers, and every hint the view
    // renders (TS `KeybindingsManager.create()` + `setKeybindings`).
    view.editor.set_keybindings(options.keybindings.clone());
    // The `terminal.showImages` setting rides the startup options (TS
    // `getShowImages`), resolved by the composition root.
    view.show_images = options.show_images;
    apply_startup_chrome(&mut view, &options);
    session.refresh_stats().await;
    // The startup catalog fetch (TS `updateAvailableProviderCount` →
    // `getConnectionAvailableModels`): failures stay silent and the
    // composition-root snapshot keeps serving the picker.
    session.spawn_model_catalog_refresh();
    session.rebuild_view(&mut view);
    if let Some(notice) = check_tmux_keyboard_setup().await {
        view.push_entry(crate::chat::ChatEntry::Status {
            text: format!("\u{26a0} {notice}"),
            kind: crate::chat::StatusKind::Warning,
        });
        session.dirty = true;
    }
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiInput>();
    let mut renderer = Renderer::setup(ui, ui_tx, exit_guard.clone())?;
    // First-run onboarding owns the pane before the session screen (TS
    // `runStartupOnboarding`, model-ready branch: splash + trace question).
    // Headless harness runs have no terminal to draw it on and skip it.
    if let Some(task) = options.onboarding.clone() {
        let exit_requested =
            run_onboarding_phase(&task, &mut view, &mut ui_rx, &mut renderer, &exit_guard).await?;
        if exit_requested {
            // The exit deadline is armed from the moment the run decides to
            // leave: no cleanup below may block past it.
            exit_guard.arm_for_exit();
            session.detach_for_exit().await;
            // The user quit at the onboarding screen: still hand the
            // terminal back (raw mode off, alt screen left and flushed)
            // exactly like a session exit.
            renderer.finish(&mut view, false);
            return Ok(InteractiveOutcome {
                active_session_id: session.active_session_id.clone(),
                session_id: session.session_id.clone(),
                resume_hint: None,
                last_assistant_text: None,
                frames: Vec::new(),
                agents_view_scope: None,
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
    // Spec §10.2: the reconnect loop after a `daemon_closing` update frame.
    // Retry with backoff for up to RECONNECT_WINDOW; each attempt reads the
    // successor's hello (`update_resume`, §10.3) and reattaches by durable
    // session id (§10.4 - the supervisor queues the attach behind any
    // restore still in flight). UI input keeps flowing while reconnecting,
    // so the user can leave with Ctrl+C instead of riding out the window.
    let mut reconnect: Option<ReconnectLoop> = None;
    // The session re-attach driver: armed when the direct worker link dies
    // (a killed or crashed worker); it re-attaches through the supervisor
    // so the respawned worker serves the session again.
    let mut session_reconnect: Option<SessionReconnect> = None;

    while running {
        // The tray override row (the Ctrl+C exit hint) follows the session's
        // hint state on every frame.
        view.chrome.tray_override = session.tray_override();

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
                    session.handle_paste(&text, &mut view);
                }
                // The headless plan's pause step: the queued keystroke
                // batch ahead of this barrier is fully handled, so the
                // parked suggestions materialize now — the same state the
                // terminal loop's 50 ms idle tick produces after a real
                // user pauses typing.
                UiInput::SettleIdle => {
                    session.materialize_editor_autocomplete(&mut view);
                }
                UiInput::Submit(text) => {
                    // A terminal-suspending client command (`/mcp login`):
                    // the auth flow prompts on the plain terminal.
                    let suspended = session.needs_terminal_suspension(&text);
                    if suspended {
                        renderer.suspend(&mut view)?;
                    }
                    let dispatched = session.submit_prompt(&text, &mut view).await;
                    if suspended {
                        renderer.resume()?;
                    }
                    if let Err(error) = dispatched {
                        // TS: a rejected submission surfaces the `⚠ Error`
                        // row and keeps the client mounted with the draft
                        // restored — a failed prompt never exits the UI.
                        session.error_row(&format!("{error:#}"), &mut view);
                        view.editor.set_text(&text);
                        session.dirty = true;
                    }
                }
                UiInput::HeadlessDone => headless_done = true,
                UiInput::ScrollTop => view.scroll_to_top(),
                UiInput::Resize => {
                    // The editor lays its window out against the new row
                    // count; the branch's dirty flag repaints the frame at
                    // the new geometry.
                    if let Ok((_width, height)) = crossterm::terminal::size() {
                        view.set_terminal_rows(height);
                    }
                }
                UiInput::WaitIdle { .. } => unreachable!("barrier handled above"),
            }
            // Paint the handled input in this iteration: the select below can
            // otherwise wait out its 50ms tick before the next draw, and
            // that wait is felt directly as keystroke-to-render lag.
            if let Some(renderer) = renderer.is_terminal_mut() {
                crate::app::draw(renderer, &mut view)?;
                session.dirty = false;
            }
            // An exit key must not wait out the select tick before the
            // bounded shutdown path runs.
            if !running {
                break;
            }
        }
        // A `/share` upload in flight holds the run open like an active
        // turn: the headless harness must not finish before its outcome
        // rows land (a live terminal never ends the run on its own).
        if headless_done
            && pending.is_empty()
            && !session.turn_active
            && wait_idle_deadline.is_none()
            && !session.dirty
            && !session.share_pending()
        {
            break;
        }

        let was_active = session.turn_active;
        tokio::select! {
            maybe_event = events.recv() => {
                match maybe_event {
                    Some(event) => {
                        session.apply_client_event(event, &mut view);
                        // Batch the rest of the queued frames before this
                        // iteration's render: a stream burst applies as one
                        // transcript pass instead of one full re-layout per
                        // frame (a replay-scale ingest renders once per
                        // batch, not once per row).
                        while let Ok(event) = events.try_recv() {
                            session.apply_client_event(event, &mut view);
                        }
                        // A succeeded compaction rebuilt the durable
                        // transcript: replace the view's chat with it, and
                        // refresh the tray usage the same way a settled
                        // turn does (TS refreshes after "a turn or
                        // compaction completes" — post-compaction usage is
                        // unknown until the next assistant response).
                        if session.transcript_stale {
                            session.rebuild_transcript(&mut view).await;
                            session.refresh_stats().await;
                            session.rebuild_tray(&mut view);
                        }
                        // A settled turn refreshes the tray's context usage.
                        if was_active && !session.turn_active {
                            session.refresh_stats().await;
                            session.rebuild_tray(&mut view);
                        }
                        // An update close frame arms the reconnect driver
                        // immediately: the doomed connection's reader task is
                        // gone, but the client struct retains an event
                        // sender, so the channel itself never closes - the
                        // frame, not the EOF, is the trigger (spec §10.2).
                        if reconnect.is_none() {
                            if let Some(update) = session.reconnect.take() {
                                session.note(
                                    &format!(
                                        "the daemon is restarting for an update (about {}s) — reconnecting…",
                                        update.est_seconds.max(1)
                                    ),
                                    &mut view,
                                );
                                reconnect = Some(ReconnectLoop::start(&update));
                                session.dirty = true;
                            }
                        }
                        // A dead direct worker link arms the session
                        // re-attach driver (TS `connection_status:
                        // "reconnecting"`): the warning row rides the chat
                        // while the driver retries the attach.
                        if session_reconnect.is_none() {
                            if let Some(lost) = session.transport_lost.take() {
                                session.note_as(
                                    "Daemon connection lost; reconnecting…",
                                    crate::chat::StatusKind::Warning,
                                    &mut view,
                                );
                                session_reconnect = Some(SessionReconnect::start(&lost));
                                session.dirty = true;
                            }
                        }
                    }
                    None => {
                        if let Some(update) = session.reconnect.take() {
                            // §10: an update restart closed the daemon; the
                            // UI stays mounted and reconnects.
                            session.note(
                                &format!(
                                    "the daemon is restarting for an update (about {}s) — reconnecting…",
                                    update.est_seconds.max(1)
                                ),
                                &mut view,
                            );
                            reconnect = Some(ReconnectLoop::start(&update));
                            session.dirty = true;
                        } else if reconnect.is_some() {
                            // Already reconnecting: the dead channel's
                            // terminal None frames are expected.
                        } else {
                            session.note("the daemon connection closed", &mut view);
                            session.exit_reason = "daemon_closed";
                            running = false;
                        }
                    }
                }
            }
            maybe_input = ui_rx.recv() => {
                if let Some(input) = maybe_input {
                    pending.push_back(input);
                }
            }
            maybe_note = notes_rx.recv() => {
                if let Some(note) = maybe_note {
                    session.apply_background_note(&note, &mut view);
                }
            }
            maybe_share = share_rx.recv() => {
                if let Some(outcome) = maybe_share {
                    session.apply_share_outcome(outcome, &mut view);
                }
            }
            maybe_catalog = catalog_rx.recv() => {
                if let Some(update) = maybe_catalog {
                    session.apply_model_catalog(update, &mut view);
                }
            }
            _reconnect_tick = async {
                match reconnect.as_ref() {
                    Some(state) => tokio::time::sleep_until(state.next_attempt).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let Some(state) = reconnect.take() else {
                    continue;
                };
                if tokio::time::Instant::now() > state.deadline {
                    session.note(
                        "could not reconnect to the daemon within 10 minutes — the update finished but this window is detached. Run `prime-agent attach` to resume.",
                        &mut view,
                    );
                    session.exit_reason = "update_reconnect_failed";
                    session.dirty = true;
                    running = false;
                    continue;
                }
                // One reconnect attempt: bounded connect, hello, reattach
                // by durable id (§10.4-§10.5).
                let attempt = tokio::time::timeout(
                    Duration::from_secs(RECONNECT_ATTEMPT_TIMEOUT_S),
                    DaemonClient::connect(&options.socket_path),
                )
                .await;
                match attempt {
                    Ok(Ok((client, fresh_events))) => {
                        match tokio::time::timeout(
                            Duration::from_secs(RECONNECT_ATTACH_TIMEOUT_S),
                            session.reattach_after_update(client, &mut view),
                        )
                        .await
                        {
                            Ok(Ok(())) => {
                                events = fresh_events;
                                session.reconnect = None;
                                reconnect = None;
                                session.dirty = true;
                            }
                            Ok(Err(error)) => {
                                session.note(
                                    &format!("reattach after the update failed: {error:#} — run `prime-agent attach` to resume"),
                                    &mut view,
                                );
                                session.exit_reason = "update_reattach_failed";
                                session.dirty = true;
                                running = false;
                            }
                            Err(_) => {
                                // The queued attach outlived this attempt's
                                // budget (a long restore): schedule another.
                                session.note(
                                    "the daemon is still restoring — retrying…",
                                    &mut view,
                                );
                                session.dirty = true;
                                reconnect = Some(state.next_attempt());
                            }
                        }
                    }
                    Ok(Err(_)) | Err(_) => {
                        reconnect = Some(state.next_attempt());
                    }
                }
            }
            _session_reconnect_tick = async {
                match session_reconnect.as_ref() {
                    Some(state) => tokio::time::sleep_until(state.next_attempt).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let Some(state) = session_reconnect.take() else {
                    continue;
                };
                // The user switched sessions while the link was down: the
                // new attach owns its own connection, so this driver stops.
                if state.active_session_id != session.active_session_id {
                    continue;
                }
                let attempt = tokio::time::timeout(
                    Duration::from_secs(SESSION_RECONNECT_ATTEMPT_TIMEOUT_S),
                    session.attach_session(&state.active_session_id),
                )
                .await;
                match attempt {
                    Ok(Ok(())) => {
                        // The resynced transcript replaces the chat (TS
                        // `session_resynced`), then the reconnected status
                        // lands on the rebuilt chat (TS
                        // `connection_status: "connected"`).
                        session.rebuild_view(&mut view);
                        session.note_as(
                            "Daemon reconnected",
                            crate::chat::StatusKind::Info,
                            &mut view,
                        );
                        session.reconnection_failed = None;
                        session_reconnect = None;
                        session.dirty = true;
                    }
                    Ok(Err(error)) => {
                        let mut state = state;
                        state.last_error = format!("{error:#}");
                        if tokio::time::Instant::now() > state.deadline {
                            // TS terminal close: the window expired, the
                            // last error surfaces as the closed event's
                            // error row, and the UI stays mounted without
                            // dispatching anything.
                            let failure =
                                format!("Daemon reconnection failed: {}", state.last_error);
                            session.error_row(&failure, &mut view);
                            session.reconnection_failed = Some(state.last_error.clone());
                            session_reconnect = None;
                            session.dirty = true;
                        } else {
                            session_reconnect = Some(state.next_attempt());
                        }
                    }
                    Err(_) => {
                        let mut state = state;
                        state.last_error =
                            "the session re-attach attempt timed out".to_string();
                        if tokio::time::Instant::now() > state.deadline {
                            let failure =
                                format!("Daemon reconnection failed: {}", state.last_error);
                            session.error_row(&failure, &mut view);
                            session.reconnection_failed = Some(state.last_error.clone());
                            session_reconnect = None;
                            session.dirty = true;
                        } else {
                            session_reconnect = Some(state.next_attempt());
                        }
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                // The input stream went quiet for a tick: parked editor
                // autocomplete requests materialize now (TS resolves
                // suggestions asynchronously after the keystroke batch, so
                // a typed command plus Enter in one burst submits as typed
                // and the dropdown opens only once typing pauses).
                session.materialize_editor_autocomplete(&mut view);
            }
        }

        // The tray goal label follows the live goal state (TS
        // `syncGoalTray`); the label only changes when the state does.
        session.sync_goal_tray(&mut view);

        // Spinner animation: the loader frame advances while a turn or a
        // compaction runs.
        if session.turn_active || view.compaction.is_some() || view.share_loader.is_some() {
            view.pulse_frame = view.pulse_frame.wrapping_add(1);
        }

        // Render when the transcript changed or an animation is live; an
        // idle session re-renders nothing (a full-transcript layout costs
        // linear time, so redrawing an unchanged idle frame burns CPU for
        // every attached session).
        let animating = session.turn_active
            || view.retry.is_some()
            || view.compaction.is_some()
            || view.share_loader.is_some();
        if animating {
            session.dirty = true;
        }
        if let Some(renderer) = renderer.is_terminal_mut() {
            if session.dirty {
                crate::app::draw(renderer, &mut view)?;
                session.dirty = false;
            }
        } else if session.dirty {
            renderer.render_headless(&mut session, &mut view);
        }
        if session.exit_requested {
            session.exit_reason = "session_request";
            running = false;
        }
    }

    // The run decided to leave: arm the force-quit deadline so every
    // cleanup step below is best-effort (stats fetch, detach, telemetry,
    // the exit flush). A wedged shutdown path cannot hold the process
    // open past it; the healthy path always finishes well inside.
    if renderer.is_terminal() {
        exit_guard.arm_for_exit();
    }
    // TS `shutdown` fetches the session stats while the connection is
    // alive, then prints the resume hint after teardown; pa-cli prints it
    // once the terminal is restored. Bounded best-effort.
    let resume_hint = session.exit_resume_hint().await;
    // Detach explicitly so the session's attached-client count stays honest;
    // the supervisor also detaches this connection when the socket closes.
    // Bounded hard: a wedged worker socket can never hold the exit path.
    session.detach_for_exit().await;
    // `tui exit` (schema v1): how the run ended. Bounded the same way as
    // the detach — telemetry must never hold the exit path open either.
    let exit_reason = session.exit_reason();
    let turn_active_at_exit = session.turn_active;
    if let Some(telemetry) = &session.telemetry {
        let _ = tokio::time::timeout(
            Duration::from_millis(TELEMETRY_EXIT_TIMEOUT_MS),
            telemetry.client_exit(exit_reason, turn_active_at_exit),
        )
        .await;
    }
    // Agents-back and `/resume` hand the pane to the agents view; the
    // alternate screen stays in place for it instead of flushing to the
    // main screen (TS `stop({ preserveAltScreen: true })`).
    let preserve_alt_screen = session.open_agents_view;
    let outcome = InteractiveOutcome {
        active_session_id: session.active_session_id.clone(),
        session_id: session.session_id.clone(),
        resume_hint,
        last_assistant_text: session.last_assistant_text.clone(),
        frames: renderer.finish(&mut view, preserve_alt_screen),
        return_to_agents_view: preserve_alt_screen,
        agents_view_scope: session.scoped_agents_view.take(),
        selection_request: session.pending_selection,
    };
    session.client.close();
    // A handoff (agents view, `/resume <selector>`) lets the process keep
    // running: retire the watchdog. Every other completion is a process
    // exit, where the deadline dies with the process — or fires when the
    // exit wedged, which is the point.
    if outcome.return_to_agents_view || outcome.selection_request.is_some() {
        exit_guard.cancel();
    }
    Ok(outcome)
}

/// Seed the static chrome state for a fresh interactive run: splash
/// version/cwd, top-bar name, and the `manage` hint for persisted sessions.
fn apply_startup_chrome(view: &mut AgentView, options: &InteractiveOptions) {
    view.chrome.version = options.version.clone();
    view.chrome.cwd = options.cwd.to_string_lossy().to_string();
    view.chrome.chat_name = crate::chrome::display_name(&view.chrome.cwd);
    view.chrome.show_manage = !options.no_session;
    view.chrome.tray_depth = options.session_rlm_depth;
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
    Terminal(Terminal<crate::hyperlinks::LinkBackend>),
    Headless {
        width: u16,
        height: u16,
        frames: Vec<String>,
    },
}

impl Renderer {
    fn setup(
        ui: UiMode,
        ui_tx: mpsc::UnboundedSender<UiInput>,
        exit_guard: ExitGuard,
    ) -> Result<Renderer> {
        match ui {
            UiMode::Terminal => {
                terminal::enable_raw_mode()?;
                // Adopt the alternate screen the previous surface left in
                // place (TS `pendingAltScreenHandoff`); only the first
                // surface of the process enters it, so a view switch never
                // flashes the primary screen.
                crate::altscreen::enter()?;
                // One reader thread feeds the loop; crossterm events are
                // process-global, so the reader registry joins the previous
                // surface's reader before this one starts polling. The
                // reader also observes Ctrl+C pairs for the exit guard:
                // this thread stays alive when the UI loop is wedged, so
                // the force-quit contract holds regardless of loop state.
                crate::input::spawn_terminal_reader(move |event| match event {
                    crossterm::event::Event::Key(key) => {
                        exit_guard.observe_key(&key);
                        ui_tx.send(UiInput::Key(key)).is_ok()
                    }
                    crossterm::event::Event::Paste(text) => {
                        ui_tx.send(UiInput::Paste(text)).is_ok()
                    }
                    // TS forces a full re-render on resize (tui.ts
                    // widthChanged/heightChanged); the loop repaints on
                    // the dirty flag this sets.
                    crossterm::event::Event::Resize(..) => ui_tx.send(UiInput::Resize).is_ok(),
                    _ => true,
                });
                let mut terminal = Terminal::new(crate::hyperlinks::stdout_backend())?;
                // The adopted buffer still holds the previous view's frame;
                // clear it so the first draw is a full repaint of the same
                // buffer (a fresh alt screen is already blank).
                terminal.clear()?;
                // The handoff left the cursor hidden (TS `stop` with
                // `preserveAltScreen` hides it); this surface wants its own
                // visible cursor back.
                crossterm::execute!(std::io::stdout(), crossterm::cursor::Show)?;
                Ok(Renderer::Terminal(terminal))
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
                            HeadlessStep::SettleIdle => {
                                if ui_tx.send(UiInput::SettleIdle).is_err() {
                                    return;
                                }
                            }
                            HeadlessStep::WaitIdle { timeout_ms } => {
                                if ui_tx.send(UiInput::WaitIdle { timeout_ms }).is_err() {
                                    return;
                                }
                            }
                            HeadlessStep::ScrollTop => {
                                if ui_tx.send(UiInput::ScrollTop).is_err() {
                                    return;
                                }
                            }
                            HeadlessStep::Key(key) => {
                                if ui_tx.send(UiInput::Key(key)).is_err() {
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
    /// screen left and flushed, cursor visible) so an interactive client
    /// command can prompt on it. Headless verification runs keep their
    /// plain pipes. The trailing cursor-show leaves the terminal with a
    /// visible cursor (the TS teardown contract: the shell prompt that
    /// follows must not sit on a hidden cursor).
    fn suspend(&mut self, view: &mut AgentView) -> Result<()> {
        match self {
            Renderer::Terminal(_) => {
                self.flush_to_main_screen(view)?;
                crossterm::execute!(std::io::stdout(), crossterm::cursor::Show)?;
                terminal::disable_raw_mode()?;
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
                // The suspension released the alternate screen (the client
                // command prompted on the primary one); re-enter it.
                crate::altscreen::enter()?;
                // A fresh full redraw: the suspended command left arbitrary
                // output behind.
                terminal.clear()?;
                Ok(())
            }
            Renderer::Headless { .. } => Ok(()),
        }
    }

    /// Leave the alternate screen and paint the accumulated inline layout
    /// onto the main screen (TS `TUI.stop` -> `exitFullscreen`: leave the
    /// alt screen first, then the inline repaint flushes the
    /// fullscreen-era transcript into native scrollback). This is what
    /// keeps the exit frame — and the resume hint the composition root
    /// prints below it — visible after the process exits, instead of the
    /// blank main screen an alt-screen exit alone leaves behind.
    ///
    /// The kill-switch `PRIME_AGENT_TUI_EXIT_FLUSH=0` skips the paint (the
    /// alt screen is still left): the flush is the one new output path that
    /// writes into the user's scrollback, so it can be disabled without a
    /// release if it misbehaves.
    fn flush_to_main_screen(&mut self, view: &mut AgentView) -> Result<()> {
        // Only the terminal renderer owns a real screen to flush;
        // headless verification keeps its plain pipes.
        if !matches!(self, Renderer::Terminal(_)) {
            return Ok(());
        }
        crate::altscreen::leave()?;
        if !exit_flush_enabled() {
            return Ok(());
        }
        let (width, height) = terminal::size()?;
        let plan = view.take_flush_plan(width as usize, height as usize);
        use std::io::Write;
        let mut out = std::io::stdout();
        let mut buffer = String::new();
        match plan {
            FlushPlan::Append(rows) if rows.is_empty() => {}
            FlushPlan::Append(rows) => {
                write_flush_rows(&mut buffer, &rows);
            }
            FlushPlan::Repaint(rows) => {
                // Erase the visible screen only — scrollback above it
                // stays (TS `fullRender`'s `\x1b[2J\x1b[H`).
                buffer.push_str("\x1b[2J\x1b[H");
                write_flush_rows(&mut buffer, &rows);
            }
        }
        out.write_all(buffer.as_bytes())?;
        out.flush()?;
        Ok(())
    }

    /// Whether this run owns a real terminal (the force-quit guard arms on
    /// terminal runs; headless verification keeps deterministic teardown).
    fn is_terminal(&self) -> bool {
        matches!(self, Renderer::Terminal(_))
    }

    fn is_terminal_mut(&mut self) -> Option<&mut Terminal<crate::hyperlinks::LinkBackend>> {
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

    /// Teardown. `preserve_alt_screen` mirrors TS `ui.stop({ preserveAltScreen })`:
    /// an exit that hands the pane to the agents view (agents-back, `/resume`)
    /// keeps the alternate screen for the adopting view, hides the cursor, and
    /// skips the main-screen flush — raw mode also stays on, because the
    /// in-process handoff gap would otherwise echo keypresses into the
    /// preserved frame (TS `pendingInputHandoff`). Every other exit follows
    /// TS `TUI.stop`: leave the alt screen, flush the inline frame onto the
    /// main screen, show the cursor, restore cooked mode — the resume hint
    /// the composition root prints next lands right below the flushed frame.
    fn finish(mut self, view: &mut AgentView, preserve_alt_screen: bool) -> Vec<String> {
        match self {
            Renderer::Terminal(_) => {
                if preserve_alt_screen {
                    let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Hide);
                } else {
                    let _ = self.flush_to_main_screen(view);
                    let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
                    let _ = terminal::disable_raw_mode();
                }
                Vec::new()
            }
            Renderer::Headless { frames, .. } => frames,
        }
    }
}

/// Encode the flushed rows into `buffer` as one write: each row starts at
/// column 0 (`\r`, required because raw mode maps `\n` to a bare line
/// feed), rows are joined with CRLF, and a trailing CRLF parks the cursor
/// below the frame (TS `TUI.stop`'s closing newline) so whatever prints
/// next — the shell prompt or the resume hint — starts on a fresh line.
fn write_flush_rows(buffer: &mut String, rows: &[crate::Line]) {
    for row in rows {
        buffer.push('\r');
        // An image-placement row is written raw (TS `applyLineResets` /
        // `paint` skip image lines): styling or padding a protocol
        // escape sequence would corrupt the placement.
        let raw: String = row.iter().map(|span| span.content.as_str()).collect();
        if crate::terminal_image::is_image_line(&raw) {
            buffer.push_str(&raw);
        } else {
            buffer.push_str(&crate::ansi::line_to_ansi(row));
        }
        buffer.push_str("\r\n");
    }
}

/// Whether the main-screen exit flush is enabled: on unless
/// `PRIME_AGENT_TUI_EXIT_FLUSH=0` opts out.
fn exit_flush_enabled() -> bool {
    std::env::var_os("PRIME_AGENT_TUI_EXIT_FLUSH").is_none_or(|value| value != "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_rows_write_crlf_and_keep_zone_markers() {
        // A marked row keeps its zero-width zone sequence inline (the
        // flushed row persists into scrollback, where absolute-position
        // marker re-emission cannot reach) and every row lands on its own
        // line with explicit CR (raw mode maps `\n` to a bare line feed).
        let mut marked = vec![crate::Span::raw("hello")];
        crate::osc133::mark_start(&mut marked);
        let styled = vec![crate::Span::styled(
            "world",
            ratatui::style::Style::default().fg(ratatui::style::Color::Indexed(1)),
        )];
        let mut buffer = String::new();
        write_flush_rows(&mut buffer, &[marked, styled]);
        let expected = format!(
            "\r{}hello\r\n\r\x1b[38;5;1mworld\x1b[0m\r\n",
            crate::osc133::ZONE_START
        );
        assert_eq!(buffer, expected);
        // No rows: no output.
        let mut empty = String::new();
        write_flush_rows(&mut empty, &[]);
        assert!(empty.is_empty());
    }

    fn options(selection: ModelSelection) -> InteractiveOptions {
        InteractiveOptions {
            socket_path: PathBuf::from("/tmp/unused.sock"),
            cwd: PathBuf::from("/tmp"),
            session_dir: None,
            script_path: None,
            model_selection: selection,
            model_catalog: Vec::new(),
            model_configured_providers: Default::default(),
            model_recent_models: Vec::new(),
            default_thinking_level: None,
            no_session: false,
            session: SessionSelection::New,
            initial_message: None,
            show_images: true,
            theme: "prime".to_string(),
            code_block_indent: "  ".to_string(),
            tree_filter_mode: String::new(),
            branch_summary_skip_prompt: false,
            version: "0.0.0".to_string(),
            onboarding: None,
            telemetry_disabled: None,
            client_auth: None,
            telemetry: None,
            keybindings: crate::keybindings::KeybindingsManager::new(),
            session_rlm_depth: None,
            session_has_children: false,
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

    #[test]
    fn resume_hint_names_a_flushed_session() {
        let dir = std::env::temp_dir().join("pa-tui-resume-hint-test");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("session.jsonl");
        std::fs::write(&file, "{}").unwrap();
        let stats = json!({
            "sessionId": "s1",
            "sessionFile": file.display().to_string(),
            "userMessages": 1
        });
        assert_eq!(
            crate::session_ui::resume_hint_from_stats(&stats),
            Some("Resume this session with: prime-agent --resume s1".to_string())
        );
        // An unflushed empty session and a missing session file are both
        // unresumable (TS omits the hint for either).
        assert_eq!(
            crate::session_ui::resume_hint_from_stats(&json!({
                "sessionId": "s1",
                "sessionFile": file.display().to_string(),
                "userMessages": 0
            })),
            None
        );
        assert_eq!(
            crate::session_ui::resume_hint_from_stats(&json!({
                "sessionId": "s1",
                "sessionFile": dir.join("missing.jsonl").display().to_string(),
                "userMessages": 3
            })),
            None
        );
    }
}
