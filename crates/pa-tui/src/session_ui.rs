//! Live per-session UI state for the interactive loop: the daemon-client
//! side of one attached session — prompt submission, slash commands, streamed
//! event application, and session switching. Rendering itself lives in the
//! view crate modules; this module only decides what the view shows.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use pa_types::daemon::DaemonCommand;
use pa_types::slash_commands::{SlashCommandExecution, SlashCommandRegistry};
use serde_json::Value;

use crate::chat::{
    ChatEntry, CompactionReason, CompactionState, MessageBlock, RetryState, StatusKind,
    ToolResultView, WorkingState,
};
use crate::daemon_client::{DaemonClient, DaemonClientEvent};
use crate::effort_picker::{self, EffortPickerAction};
use crate::export_share::{self, GhAuthStatus, GistOutcome};
use crate::goal_surface::{format_goal_status, tray_goal_label, GoalView};
use crate::image_load::LoadedImage;
use crate::image_markers::{
    collect_marked_images, evict_images_to_budget, format_image_marker, image_marker_ids,
};
use crate::interactive::{InteractiveOptions, ModelSelection, SessionSelection};
use crate::keys::key_event_to_id;
use crate::model_picker::{CurrentModel, ModelPicker, ModelPickerAction, ModelPickerOptions};
use crate::snapshot::{
    assistant_message_parts, attach_data_from_response, event_to_update, reconstruct, TurnUpdate,
};
use crate::tree_selector::{TreeSelector, TreeSelectorAction};
use crate::user_message_selector::{UserMessageSelector, UserMessageSelectorAction};
use crate::view::{AgentView, ShareLoader};

use crossterm::event::KeyEvent;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// How long the Ctrl+C exit hint arms the second-press exit (TS
/// `EXIT_HINT_DURATION_MS`).
const CTRL_C_EXIT_HINT_MS: u64 = 2_000;
/// Cap on any daemon request awaited on the key-handling path: the UI loop
/// must stay responsive to Ctrl+C while a submission travels (the TS loop
/// never blocks on these — aborts are fire-and-forget, submissions resolve
/// off the render path).
const UI_REQUEST_TIMEOUT_MS: u64 = 10_000;

/// How long a fetched model catalog stays fresh (TS
/// `MODEL_CATALOG_REFRESH_TTL_MS`); a `/model` open past it refreshes
/// again in the background.
const MODEL_CATALOG_REFRESH_TTL: std::time::Duration = std::time::Duration::from_secs(60);
/// Cap on the detach request during the exit path: the client must exit
/// promptly even when the worker socket is wedged.
const EXIT_DETACH_TIMEOUT_MS: u64 = 600;
/// The double-Escape repeat window (TS `ESCAPE_REPEAT_WINDOW_MS`).
const ESCAPE_REPEAT_WINDOW_MS: std::time::Duration = std::time::Duration::from_millis(500);
/// Cap on the exit-path session-stats fetch (TS `formatResumeHint` inputs):
/// best-effort like the detach, never able to hold the exit open.
const EXIT_STATS_TIMEOUT_MS: u64 = 500;

/// Live UI state for one attached daemon session.
/// The `/share` upload task's report: the created gist or the failure
/// message (TS resolves the same promise from the gh process result).
pub(crate) type ShareNote = Result<GistOutcome, String>;

/// A landed `get_model_catalog` refresh: the full catalog and the providers
/// with configured auth (TS `AgentConnectionModelCatalog`).
pub(crate) struct ModelCatalogUpdate {
    pub models: Vec<pa_types::ai::Model>,
    pub configured_providers: std::collections::HashSet<String>,
}

/// A `/share` upload in flight: the abortable task and the temp export.
pub(crate) struct ShareRun {
    /// The upload task; aborting it kills `gh` (`kill_on_drop`).
    task: tokio::task::JoinHandle<()>,
    /// The temp HTML export `gh gist create` uploads (removed on settle).
    tmp_file: std::path::PathBuf,
}

pub(crate) struct SessionUi {
    pub(crate) client: DaemonClient,
    pub(crate) active_session_id: String,
    pub(crate) session_id: String,
    session_name: Option<String>,
    /// Config carried over from the run options; `/new` sessions reuse it.
    cwd: PathBuf,
    session_dir: Option<PathBuf>,
    script_path: Option<PathBuf>,
    model_selection: ModelSelection,
    /// The model catalog for the `/model` picker: a startup snapshot from
    /// the composition root (the bundled fallback), replaced by the
    /// daemon's `get_model_catalog` response once it lands.
    model_catalog: Vec<pa_types::ai::Model>,
    /// Providers with configured auth (the daemon catalog's
    /// `configuredProviders`); the picker marks the rest "require sign in".
    model_configured_providers: std::collections::HashSet<String>,
    /// The settings recent-model list (`provider/id` keys, newest first).
    model_recent_models: Vec<String>,
    /// The settings default thinking level (TS `getDefaultThinkingLevel`)
    /// — the picker's effort seed for non-reasoning current models.
    default_thinking_level: Option<String>,
    /// When the daemon catalog was last refreshed (TS
    /// `connectionModelsFetchedAt`; the refresh is TTL-gated).
    models_fetched_at: Option<std::time::Instant>,
    /// Pasted images held for their editor markers, keyed by marker id
    /// (TS `pastedImages`). Insertion order is paste order.
    pasted_images: std::collections::BTreeMap<u64, LoadedImage>,
    /// The next `[image #N]` marker id (TS `nextImageMarkerId`).
    next_image_marker_id: u64,
    /// Telemetry opt-out carried over from the run options; every attach to
    /// another session keeps carrying it (TS attach parity).
    telemetry_disabled: Option<bool>,
    /// The chat markdown fenced-code indent from the run's options
    /// (`markdown.codeBlockIndent`); `/new` re-opens with the same value
    /// instead of resetting it to the default.
    code_block_indent: String,
    /// The `/tree` selector's initial filter mode (the `treeFilterMode`
    /// setting).
    tree_filter_mode: crate::tree_list::FilterMode,
    /// The `branchSummary.skipPrompt` setting: navigation skips the
    /// summarize question.
    branch_summary_skip_prompt: bool,
    /// The transcript index of the last `note` status row (TS `showStatus`
    /// tracks its previous row for the back-to-back in-place rewrite; any
    /// later entry invalidates it through the length check).
    last_status_index: Option<usize>,
    /// The `terminal.showImages` setting, carried into `/new` runs.
    show_images: bool,
    /// The `terminal.fullscreenMouse` setting: whether the interactive
    /// surface enables mouse tracking; carried into `/new` runs.
    fullscreen_mouse: bool,
    /// A `/share` gist upload in flight (TS `BorderedLoader` + the gh
    /// spawn): aborting the task kills `gh` (kill-on-drop).
    share: Option<ShareRun>,
    /// Where the upload task reports its outcome (the run loop folds it
    /// into the transcript).
    share_notes: mpsc::UnboundedSender<ShareNote>,
    /// Where the background catalog refresh delivers `get_model_catalog`
    /// responses (the run loop folds them into the picker catalog).
    catalog_updates: mpsc::UnboundedSender<ModelCatalogUpdate>,
    /// Snapshot chat entries to fold into the view on the next rebuild.
    pending_snapshot: Option<Vec<ChatEntry>>,
    /// Snapshot labels (model) for the next rebuild.
    pending_model: Option<String>,
    /// Context usage + cost refreshed from `get_session_stats`.
    context: Option<crate::chrome::ContextUsage>,
    cost_usd: Option<f64>,
    /// Rows of the most recent `/list` (for `/switch <n>`).
    list_rows: Vec<Value>,
    pub(crate) turn_active: bool,
    /// The chat index of the assistant message still streaming.
    streaming_index: Option<usize>,
    /// The loader's token accounting (TS `AgentActivityTracker`), reported
    /// monotonically within a run.
    working_tokens: LoaderTokenTracker,
    /// The turn already surfaced its error (a failed assistant message or a
    /// retry-exhausted banner); the turn_end error stays silent then (TS
    /// renders the failure once, through the message or the retry banner).
    turn_error_shown: bool,
    pub(crate) last_assistant_text: Option<String>,
    pub(crate) exit_requested: bool,
    /// `/resume` or the agents-back key: reopen the agents view after this
    /// session detaches.
    pub(crate) open_agents_view: bool,
    /// The subagent summary line opened the scoped agents view: the scope
    /// the view opens on (this session's subtree).
    pub(crate) scoped_agents_view: Option<crate::agents_view::AgentsViewScope>,
    /// The live agent roster (the session view's `roster_subscribe`
    /// subscription, TS `rosterBar`): drives the subagent summary counts.
    roster: Vec<Value>,
    /// The subagent summary line holds keyboard focus.
    subagents_focused: bool,
    /// The last computed descendant counts (selectability reads them between
    /// roster updates).
    subagent_counts: crate::subagents::SubagentCounts,
    /// This session's persisted file path (family identity of the
    /// subagent linkage; `None` for unpersisted sessions).
    session_file: Option<String>,
    /// `/resume <selector>`: open this selection next (the run returns it).
    pub(crate) pending_selection: Option<SessionSelection>,
    /// Whether this run may hand the terminal back to the agents view (TS
    /// `returnToAgentsView`): true for every daemon-hosted session, false
    /// only for `--no-session` runs.
    pub(crate) return_to_agents_view: bool,
    /// `/mcp login` / `/mcp logout` (the composition root's auth flows).
    client_auth: Option<crate::client_auth::ClientAuthCommandsHandle>,
    /// The effective keybindings (user `keybindings.json` over the TS
    /// defaults): hint labels and app-level handlers dispatch through this
    /// set, and `/new` runs carry it forward.
    keybindings: crate::keybindings::KeybindingsManager,
    pub(crate) dirty: bool,
    /// The Ctrl+C exit hint (TS `ctrlCExitHintExpiresAt`): a second press
    /// inside the window terminates the client, regardless of turn state.
    ctrl_c_hint_until: Option<Instant>,
    /// The session's thread-goal view state (current `goal_update` state,
    /// announcement dedupe, tray label bookkeeping).
    pub(crate) goal_view: GoalView,
    /// Notes surfacing from background tasks (the async abort result) into
    /// the UI loop.
    notes: mpsc::UnboundedSender<String>,
    /// A succeeded compaction replaced the durable transcript (TS
    /// `rebuildChatFromMessages`): the next loop pass re-fetches it.
    pub(crate) transcript_stale: bool,
    /// Adoption telemetry (`tui scroll used` / `tui exit`); `None` drops
    /// events.
    pub(crate) telemetry: Option<std::sync::Arc<dyn crate::interactive::InteractionTelemetry>>,
    /// Whether this run already reported its first scroll action.
    scroll_adoption_emitted: bool,
    /// How the client run ended (the `tui exit` reason).
    pub(crate) exit_reason: &'static str,
    /// The §10 reattach contract: set when a `daemon_closing` update frame
    /// arrived; the interactive loop drives the reconnect from it.
    pub(crate) reconnect: Option<crate::daemon_client::DaemonClosingUpdate>,
    /// The session whose direct worker link just died; the interactive
    /// loop arms the re-attach driver from it (TS `connection_status:
    /// "reconnecting"`).
    pub(crate) transport_lost: Option<String>,
    /// The re-attach window expired (TS terminal close after
    /// `DAEMON_RECONNECT_TIMEOUT_MS`): dispatch is blocked and submits
    /// surface the error instead of leaving the UI on a silent spinner.
    pub(crate) reconnection_failed: Option<String>,
    /// The double-Ctrl+C force-quit guard (the run's shared instance is
    /// installed by the interactive loop after `open`).
    pub(crate) exit_guard: crate::exit_guard::ExitGuard,
    /// The armed double-Escape action (TS `escapeRepeatAction`): "tree" or
    /// "clear", taken by the second press inside the 500ms window.
    escape_repeat_action: Option<&'static str>,
    escape_repeat_until: Option<Instant>,
}

impl SessionUi {
    /// Create/attach per the session selection and return the live state.
    pub(crate) async fn open(
        client: DaemonClient,
        options: &InteractiveOptions,
        notes: mpsc::UnboundedSender<String>,
        share_notes: mpsc::UnboundedSender<ShareNote>,
        catalog_updates: mpsc::UnboundedSender<ModelCatalogUpdate>,
    ) -> Result<SessionUi> {
        let active_session_id = match &options.session {
            SessionSelection::New => create_session(&client, options, None).await?,
            SessionSelection::Attach(id) => id.clone(),
            SessionSelection::ContinueRecent | SessionSelection::Resume(_) => {
                create_session(&client, options, Some(&options.session)).await?
            }
        };
        let mut session = SessionUi {
            client,
            active_session_id: String::new(),
            session_id: String::new(),
            session_name: None,
            cwd: options.cwd.clone(),
            session_dir: options.session_dir.clone(),
            script_path: options.script_path.clone(),
            model_selection: options.model_selection.clone(),
            model_catalog: options.model_catalog.clone(),
            model_configured_providers: options.model_configured_providers.clone(),
            model_recent_models: options.model_recent_models.clone(),
            default_thinking_level: options.default_thinking_level.clone(),
            models_fetched_at: None,
            catalog_updates,
            telemetry_disabled: options.telemetry_disabled,
            code_block_indent: options.code_block_indent.clone(),
            tree_filter_mode: crate::tree_list::filter_mode_from_str(&options.tree_filter_mode),
            branch_summary_skip_prompt: options.branch_summary_skip_prompt,
            last_status_index: None,
            show_images: options.show_images,
            fullscreen_mouse: options.fullscreen_mouse,
            share: None,
            share_notes,
            pasted_images: Default::default(),
            next_image_marker_id: 1,
            pending_snapshot: None,
            pending_model: None,
            context: None,
            cost_usd: None,
            list_rows: Vec::new(),
            turn_active: false,
            streaming_index: None,
            working_tokens: LoaderTokenTracker::default(),
            turn_error_shown: false,
            last_assistant_text: None,
            exit_requested: false,
            open_agents_view: false,
            scoped_agents_view: None,
            roster: Vec::new(),
            subagents_focused: false,
            subagent_counts: crate::subagents::SubagentCounts::default(),
            session_file: None,
            pending_selection: None,
            return_to_agents_view: !options.no_session,
            client_auth: options.client_auth.clone(),
            keybindings: options.keybindings.clone(),
            dirty: true,
            ctrl_c_hint_until: None,
            goal_view: GoalView::new(),
            notes,
            transcript_stale: false,
            telemetry: options.telemetry.clone(),
            scroll_adoption_emitted: false,
            exit_reason: "daemon_closed",
            reconnect: None,
            transport_lost: None,
            reconnection_failed: None,
            exit_guard: crate::exit_guard::ExitGuard::new(),
            escape_repeat_action: None,
            escape_repeat_until: None,
        };
        session
            .attach_session(&active_session_id)
            .await
            .with_context(|| format!("attaching session {active_session_id}"))?;
        Ok(session)
    }

    /// Spec §10.2-§10.5: reattach after an update restart. The fresh client
    /// (connected to the successor supervisor) replaces the dead one; the
    /// attach goes by DURABLE session id, so the slice-5 queued-attach
    /// contract absorbs any restore still in flight (the §10.3 hello's
    /// `update_resume.complete` is surfaced as a banner line). The
    /// transcript rebuilds from the attach snapshot - the same machinery
    /// `/switch` uses - and the resumed-work banner lands after it.
    pub(crate) async fn reattach_after_update(
        &mut self,
        client: DaemonClient,
        view: &mut AgentView,
    ) -> Result<()> {
        let hello_resume = client
            .hello()
            .get("updateResume")
            .cloned()
            .unwrap_or(Value::Null);
        let complete = hello_resume.get("complete").and_then(Value::as_bool);
        let update_id = hello_resume
            .get("updateId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.client = client;
        let durable = self.session_id.clone();
        if durable.is_empty() {
            anyhow::bail!("the session's durable id is unknown; cannot reattach");
        }
        self.attach_session(&durable)
            .await
            .with_context(|| format!("reattaching session {durable} after the update"))?;
        // Flush the attach snapshot BEFORE the banner lands: `rebuild_view`
        // replaces the transcript from the snapshot, so the banner must come
        // after it to survive the rebuild (§10.5's visible end state).
        self.rebuild_view(view);
        match complete {
            Some(false) => view.push_entry(crate::chat::ChatEntry::Status {
                text: "Reconnected — the daemon is finishing its restore; queued work resumes when the session comes up.".to_string(),
                kind: crate::chat::StatusKind::Info,
            }),
            _ => view.push_entry(crate::chat::ChatEntry::Status {
                text: format!(
                    "Reconnected to Prime Agent (update {update_id}) — your session and queued work resumed."
                ),
                kind: crate::chat::StatusKind::Info,
            }),
        }
        self.dirty = true;
        Ok(())
    }

    /// Detach the current session and attach `id`, rebuilding the transcript
    /// from the slim attach snapshot.
    ///
    /// The attach itself travels over a direct worker link when the
    /// supervisor issues a ticket (best effort: every failure keeps the
    /// supervisor-routed path, and a failed direct attach retries once over
    /// the supervisor).
    pub(crate) async fn attach_session(&mut self, active_session_id: &str) -> Result<()> {
        let previous = self.active_session_id.clone();
        if !previous.is_empty() && previous != active_session_id {
            let _ = self.detach().await;
        }
        // A direct link is bound to one session: drop it when switching.
        if self
            .client
            .direct_session_id()
            .is_some_and(|direct| direct != active_session_id)
        {
            self.client.drop_direct();
        }
        let attach_command = |session_id: &str| DaemonCommand::Attach {
            id: None,
            active_session_id: session_id.to_string(),
            supports_extension_ui: None,
            client_id: None,
            capabilities: None,
            resume_cursor: None,
            telemetry_disabled: self.telemetry_disabled.filter(|disabled| *disabled),
            recovery_config: None,
            env: None,
            launch_env: None,
            rest: Default::default(),
        };
        let direct_attached = self
            .client
            .upgrade_direct(active_session_id)
            .await
            .unwrap_or(false);
        let attached = match self
            .client
            .request_ok(attach_command(active_session_id))
            .await
        {
            Ok(data) => data,
            Err(error) => {
                if !direct_attached {
                    return Err(error);
                }
                // The direct attach failed: one supervisor-routed retry
                // (TS `DaemonAgentConnection.attach` fallback).
                self.client.drop_direct();
                self.client
                    .request_ok(attach_command(active_session_id))
                    .await?
            }
        };
        let data = attached;
        let attach = attach_data_from_response(&data)?;
        let reconstructed = reconstruct(&attach);
        self.active_session_id = attach.active_session_id;
        self.session_id = reconstructed.session_id;
        self.session_name = reconstructed.session_name.clone();
        self.session_file = attach
            .snapshot
            .get("state")
            .and_then(|state| state.get("sessionFile"))
            .and_then(Value::as_str)
            .filter(|file| !file.is_empty())
            .map(str::to_string);
        // The subagent summary follows the fresh session's family: the old
        // roster belongs to the previous session, and the focus returns to
        // the editor (TS `resetSubagentSummary` on rebind).
        self.roster.clear();
        self.subagents_focused = false;
        self.subscribe_roster().await;
        self.pending_model = reconstructed.model_id;
        self.last_assistant_text = reconstructed
            .chat
            .iter()
            .rev()
            .find_map(|entry| match entry {
                ChatEntry::Assistant(message) => {
                    message.blocks.iter().rev().find_map(|block| match block {
                        MessageBlock::Text(text) => Some(text.clone()),
                        _ => None,
                    })
                }
                _ => None,
            });
        self.pending_snapshot = Some(reconstructed.chat);
        self.goal_view.seed(reconstructed.goal.unwrap_or_default());
        // The resynced state owns the loader (TS `renderResyncedSession`
        // rebuilds from the snapshot): a turn that is still live behind the
        // re-attach keeps the spinner, one that died with the old link (or
        // never ran) does not.
        let streaming = attach
            .snapshot
            .get("state")
            .map(|state| {
                ["isStreaming", "isCompacting"]
                    .iter()
                    .any(|flag| state.get(flag).and_then(Value::as_bool).unwrap_or(false))
            })
            .unwrap_or(false);
        self.turn_active = streaming;
        self.streaming_index = None;
        Ok(())
    }

    /// Subscribe this client to the live agent roster (TS
    /// `subscribeAgentRoster`): the snapshot seeds the subagent summary
    /// counts, `roster_update` pushes keep them live. A failed
    /// subscription degrades to no counts (TS `rosterBar = undefined`).
    async fn subscribe_roster(&mut self) {
        let snapshot = self
            .client
            .request(DaemonCommand::RosterSubscribe {
                id: None,
                rest: Default::default(),
            })
            .await;
        if let Ok(response) = snapshot {
            if response.success {
                self.roster = response
                    .data
                    .as_ref()
                    .and_then(|data| data.get("roster"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
            }
        }
    }

    /// Apply one roster push (`changed` upsert by agent id, `removed`
    /// deletes, `resync` replaces the whole roster; TS `roster-store`).
    fn apply_roster_update(&mut self, changed: Vec<Value>, removed: Vec<String>, resync: bool) {
        if resync {
            self.roster.clear();
        }
        for entry in changed {
            let Some(agent_id) = entry.get("agentId").and_then(Value::as_str) else {
                continue;
            };
            if let Some(existing) = self
                .roster
                .iter_mut()
                .find(|row| row.get("agentId").and_then(Value::as_str) == Some(agent_id))
            {
                *existing = entry;
            } else {
                self.roster.push(entry);
            }
        }
        for agent_id in removed {
            self.roster.retain(|row| {
                row.get("agentId").and_then(Value::as_str) != Some(agent_id.as_str())
            });
        }
    }

    /// Recompute the subagent summary box from the roster (TS
    /// `updateSubagentSummaryLine`): live counts over this session's
    /// descendants, focused when the summary line holds the focus.
    fn update_subagent_summary(&mut self, view: &mut AgentView) {
        let identity = crate::subagents::SessionIdentity::new(
            (!self.active_session_id.is_empty()).then(|| self.active_session_id.clone()),
            (!self.session_id.is_empty()).then(|| self.session_id.clone()),
            self.session_file.clone(),
        );
        let counts = crate::subagents::count_descendants(&self.roster, &identity);
        self.subagent_counts = counts;
        if counts.total == 0 {
            self.subagents_focused = false;
            view.chrome.subagents = None;
            return;
        }
        view.chrome.subagents = Some(crate::chrome::SubagentSummary {
            running: counts.running,
            idle: counts.idle,
            inactive: counts.inactive,
            focused: self.subagents_focused,
            openable: self.return_to_agents_view,
        });
    }

    /// Whether the subagent summary line may take focus (TS `isSelectable`):
    /// children exist and the run can open the scoped agents view.
    fn subagents_selectable(&self) -> bool {
        self.return_to_agents_view && self.subagent_counts.total > 0
    }

    /// Open the scoped agents view from the focused summary line (TS
    /// `openScopedAgentsView` -> `returnToAgentsView("scoped_agents_view")`):
    /// the session detaches and the agents view reopens scoped to this
    /// session's subtree, anchored on it.
    fn open_scoped_agents_view(&mut self, view: &mut AgentView) {
        self.subagents_focused = false;
        self.update_subagent_summary(view);
        // `tui subagents open`: fire-and-forget like the scroll adoption
        // event - the keypress never waits on the telemetry flush.
        if let Some(telemetry) = self.telemetry.clone() {
            let children_total = self.subagent_counts.total as u64;
            tokio::spawn(async move {
                telemetry.subagents_view_opened(children_total).await;
            });
        }
        self.scoped_agents_view = Some(crate::agents_view::AgentsViewScope {
            active_session_id: Some(self.active_session_id.clone()),
            session_id: Some(self.session_id.clone()),
            session_name: self.session_name.clone(),
        });
        self.open_agents_view = true;
        self.exit_requested = true;
        self.dirty = true;
    }

    /// Fold the pending snapshot into the view (fresh transcript, footer
    /// labels). Called after attach and after every session switch.
    pub(crate) fn rebuild_view(&mut self, view: &mut AgentView) {
        view.clear_chat();
        // The rebuilt transcript invalidates the tracked status row.
        self.last_status_index = None;
        if let Some(items) = self.pending_snapshot.take() {
            for entry in items {
                view.push_entry(entry);
            }
        }
        if let Some(model) = self.pending_model.take() {
            view.chrome.model_id = Some(model);
        }
        view.chrome.chat_name = self.session_display();
        view.chrome.context = self.context;
        view.chrome.cost_usd = self.cost_usd;
        self.update_subagent_summary(view);
        // The rebuilt transcript invalidates the announcement row tracking;
        // the goal state itself carries over (seeded at attach).
        self.goal_view.reset_row_tracking();
        self.sync_goal_tray(view);
        // The rebuilt chat follows the session's live state: an attached
        // turn that survived the re-attach keeps its loader (TS
        // `renderResyncedSession`), and no stale loader survives a rebuild.
        if self.turn_active {
            self.start_loader(view);
        } else {
            view.working = None;
        }
        view.follow();
        self.dirty = true;
    }

    /// Materialize parked editor autocomplete requests once the input
    /// queue drains (the editor defers dropdown materialization past the
    /// keystroke batch; TS resolves suggestions asynchronously). The
    /// single-item Tab auto-apply mutates the editor text, so the change
    /// events dispatch here.
    pub(crate) fn materialize_editor_autocomplete(&mut self, view: &mut AgentView) {
        let was_showing = view.editor.is_showing_autocomplete();
        view.editor.materialize_autocomplete();
        for event in view.editor.take_events() {
            if let crate::editor::EditorEvent::Changed(text) = event {
                // TS onChange: the exit hint clears as soon as the editor
                // carries text.
                if !text.is_empty() {
                    self.clear_ctrl_c_hint();
                }
                self.dirty = true;
            }
        }
        if view.editor.is_showing_autocomplete() != was_showing {
            self.dirty = true;
        }
    }

    /// One `goal_update` session event (TS `handleGoalUpdate`): store the
    /// state, announce as a status row when the dedupe rules say so, and
    /// sync the tray goal label.
    fn apply_goal_update(&mut self, goal: Value, view: &mut AgentView) {
        let Ok(goal) = serde_json::from_value::<pa_types::goal::GoalState>(goal) else {
            return;
        };
        let announce = self.goal_view.apply_update(goal);
        if announce {
            self.announce_goal_status(view);
        }
        self.sync_goal_tray(view);
    }

    /// The goal status row (TS `showStatus` via `formatGoalStatus`): a
    /// consecutive announcement rewrites the previous status row in place
    /// while it is still the transcript's last entry.
    fn announce_goal_status(&mut self, view: &mut AgentView) {
        let columns = terminal_columns();
        let text = format_goal_status(&self.goal_view.goal, columns);
        let updated_in_place = match self.goal_view.last_status_index {
            Some(index) if index + 1 == view.chat_len() => {
                view.update_status_row(index, &text, StatusKind::Info)
            }
            _ => false,
        };
        if !updated_in_place {
            view.push_entry(ChatEntry::Status {
                text,
                kind: StatusKind::Info,
            });
            self.goal_view.last_status_index = Some(view.chat_len() - 1);
        }
        self.dirty = true;
    }

    /// The tray goal label follows the current goal state (TS
    /// `syncGoalTray`; the label itself is `getTrayGoalLabel`).
    pub(crate) fn sync_goal_tray(&mut self, view: &mut AgentView) {
        let label = tray_goal_label(&self.goal_view.goal);
        if view.chrome.goal_label != label {
            view.chrome.goal_label = label;
            self.dirty = true;
        }
    }

    fn session_display(&self) -> String {
        self.session_name
            .clone()
            .unwrap_or_else(|| crate::chrome::display_name(&self.cwd.to_string_lossy()))
    }

    /// Refresh context usage and session spend from `get_session_stats`
    /// (the TS tray's connection refresh): tokens, context window, percent,
    /// and the branch total cost.
    pub(crate) async fn refresh_stats(&mut self) {
        let Ok(data) = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::GetSessionStats {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    rest: Default::default(),
                },
            )
            .await
        else {
            return;
        };
        // TS `patchConnectionState` REPLACES the context usage snapshot:
        // unknown usage (tokens null right after a compaction, or a
        // response without the field) clears the tray display instead of
        // keeping the stale one.
        self.context = data.get("contextUsage").and_then(|usage| {
            let tokens = usage.get("tokens").and_then(Value::as_u64)?;
            let window = usage.get("contextWindow").and_then(Value::as_u64)?;
            Some(crate::chrome::ContextUsage {
                tokens,
                context_window: window,
            })
        });
        self.cost_usd = data.get("cost").and_then(Value::as_f64);
        self.dirty = true;
    }

    /// Re-apply the refreshed context usage and cost to the chrome state.
    pub(crate) fn rebuild_tray(&mut self, view: &mut AgentView) {
        view.chrome.context = self.context;
        view.chrome.cost_usd = self.cost_usd;
        view.chrome.chat_name = self.session_display();
        self.dirty = true;
    }

    pub(crate) fn note(&mut self, text: &str, view: &mut AgentView) {
        self.note_as(text, StatusKind::Info, view);
    }

    /// TS `showStatus` with a tone: the same back-to-back in-place rewrite
    /// as [`Self::note`], with the row's kind following the TS tone.
    pub(crate) fn note_as(&mut self, text: &str, kind: StatusKind, view: &mut AgentView) {
        // TS `showStatus`: a status emitted back-to-back (nothing else
        // reached the chat since the previous one) rewrites the previous
        // status row in place instead of appending a new one.
        let updated_in_place = match self.last_status_index {
            Some(index) if index + 1 == view.chat_len() => {
                view.update_status_row(index, text, kind.clone())
            }
            _ => false,
        };
        if !updated_in_place {
            view.push_entry(ChatEntry::Status {
                text: text.to_string(),
                kind,
            });
            self.last_status_index = Some(view.chat_len() - 1);
        }
        self.dirty = true;
    }

    pub(crate) async fn detach(&self) -> Result<()> {
        self.bounded_request(
            Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
            DaemonCommand::Detach {
                id: None,
                active_session_id: Some(self.active_session_id.clone()),
                rest: Default::default(),
            },
        )
        .await
        .map(|_| ())
    }

    /// Detach during the exit path, bounded hard: a wedged worker socket can
    /// never hold the client open (the exit contract is exit-within-1s even
    /// then, so this must stay well under the bound).
    pub(crate) async fn detach_for_exit(&self) {
        let _ = tokio::time::timeout(
            Duration::from_millis(EXIT_DETACH_TIMEOUT_MS),
            self.client.request_ok(DaemonCommand::Detach {
                id: None,
                active_session_id: Some(self.active_session_id.clone()),
                rest: Default::default(),
            }),
        )
        .await;
    }

    /// Fetch the exit resume hint (TS `shutdown` fetches `getSessionStats`
    /// while the connection is alive, then prints `formatResumeHint` after
    /// teardown). Bounded best-effort: a wedged worker or a dead connection
    /// yields no hint instead of holding the exit open.
    pub(crate) async fn exit_resume_hint(&self) -> Option<String> {
        let stats = tokio::time::timeout(
            Duration::from_millis(EXIT_STATS_TIMEOUT_MS),
            self.client.request_ok(DaemonCommand::GetSessionStats {
                id: None,
                active_session_id: self.active_session_id.clone(),
                rest: Default::default(),
            }),
        )
        .await;
        resume_hint_from_stats(&stats.ok()?.ok()?)
    }

    /// One daemon request with a hard await cap: the UI loop only ever
    /// blocks this long on it.
    async fn bounded_request(&self, timeout: Duration, command: DaemonCommand) -> Result<Value> {
        tokio::time::timeout(timeout, self.client.request_ok(command))
            .await
            .map_err(|_| {
                anyhow!(
                    "timed out after {}ms waiting for the Prime Agent daemon response",
                    timeout.as_millis()
                )
            })?
    }

    /// The retained-bytes budget for pasted images (TS
    /// `MAX_PASTED_IMAGE_BYTES`): the registry evicts oldest entries once
    /// the retained base64 payload exceeds it.
    const MAX_PASTED_IMAGE_BYTES: usize = 64 * 1024 * 1024;

    /// Read the clipboard image and register it behind a new editor
    /// marker (TS `handleClipboardImagePaste`). A clipboard without a
    /// supported image is a no-op; clipboard errors are silently ignored
    /// (the clipboard may lack permissions), matching the TS catch.
    async fn handle_clipboard_image_paste(&mut self, view: &mut AgentView) {
        let Some(attachment) = crate::clipboard_image::read_clipboard_image().await else {
            return;
        };
        let marker_id = self.next_image_marker_id;
        self.next_image_marker_id += 1;
        let mime_type = attachment.mime_type.clone();
        self.remember_pasted_image(marker_id, attachment, view);
        view.editor
            .insert_text_at_cursor(&format_image_marker(marker_id));
        if let Some(telemetry) = self.telemetry.clone() {
            tokio::spawn(async move {
                telemetry.image_pasted(&mime_type).await;
            });
        }
        if !self.model_supports_images(view) {
            self.note(
                "Current model does not support images; the attachment will be omitted.",
                view,
            );
        }
        self.dirty = true;
    }

    /// Record a pasted image, evicting the oldest entries once the
    /// retained bytes exceed [`Self::MAX_PASTED_IMAGE_BYTES`] (TS
    /// `rememberPastedImage`). The just-added image and every image whose
    /// marker is still reachable are never evicted, so a live marker never
    /// loses its image.
    fn remember_pasted_image(&mut self, id: u64, image: LoadedImage, view: &AgentView) {
        self.pasted_images.insert(id, image);
        let mut keep = Self::live_image_marker_ids(&view.editor);
        keep.insert(id);
        let mut images = std::mem::take(&mut self.pasted_images);
        evict_images_to_budget(
            &mut images,
            |image: &LoadedImage| image.data.len(),
            Self::MAX_PASTED_IMAGE_BYTES,
            &keep,
        );
        self.pasted_images = images;
    }

    /// Marker ids still reachable - current editor text and prompt history
    /// (recallable with the up arrow) - which are never evicted so a
    /// recall never finds a marker with no image. The TS version also
    /// scans the compaction/connection queues, which live daemon-side
    /// here.
    fn live_image_marker_ids(editor: &crate::editor::Editor) -> std::collections::BTreeSet<u64> {
        let mut ids = std::collections::BTreeSet::new();
        ids.extend(image_marker_ids(&editor.get_text()));
        ids.extend(
            editor
                .get_history()
                .iter()
                .flat_map(|text| image_marker_ids(text)),
        );
        ids
    }

    /// Whether the current model takes image input (TS
    /// `model.input.includes("image")`), when the model is known from the
    /// startup catalog; unknown models are assumed capable (the daemon
    /// re-checks against the resolved model anyway).
    fn model_supports_images(&self, view: &AgentView) -> bool {
        let Some(model_id) = view.chrome.model_id.as_deref() else {
            return true;
        };
        let Some(model) = self.model_catalog.iter().find(|model| model.id == model_id) else {
            return true;
        };
        model.input.contains(&pa_types::ai::ModelInput::Image)
    }

    /// Submit a prompt. The user message arrives back as a `message_start`
    /// session event (no local echo), and prompts sent while a turn is active
    /// queue on the daemon side.
    pub(crate) async fn submit_prompt(&mut self, text: &str, view: &mut AgentView) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        if text.starts_with('/') {
            return self.handle_slash(text, view).await;
        }
        // TS `clearShortcutGuide`: every prompt submission dismisses the
        // `?` quick-shortcut guide (slash commands keep it).
        view.shortcut_guide = None;
        self.send_prompt(text, view).await
    }

    /// Send a prompt to the session and start the working loader. Session
    /// commands travel the same path — the session engine parses and
    /// executes them instead of admitting a model turn.
    /// The images whose markers are present in `text`, or `None` when
    /// there are none (TS `collectImagesFor`). Resolved against the
    /// current model: when it has no image input the attachments are
    /// dropped here, matching the paste-time hint.
    fn collect_images_for(&self, text: &str, view: &AgentView) -> Option<serde_json::Value> {
        if !self.model_supports_images(view) {
            return None;
        }
        let images: Vec<&LoadedImage> = collect_marked_images(&self.pasted_images, text)
            .into_iter()
            .map(|(_, image)| image)
            .collect();
        if images.is_empty() {
            return None;
        }
        Some(serde_json::Value::Array(
            images
                .iter()
                .map(|image| {
                    serde_json::json!({
                        "type": "image",
                        "data": image.data,
                        "mimeType": image.mime_type,
                    })
                })
                .collect(),
        ))
    }

    async fn send_prompt(&mut self, text: &str, view: &mut AgentView) -> Result<()> {
        if let Some(error) = self.reconnection_failed.clone() {
            // The re-attach window expired (TS terminal close): the session
            // connection is closed, so nothing dispatches. The error row
            // surfaces the terminal cause and the draft returns to the
            // editor (TS keeps the input buffer on a failed submit).
            self.error_row(&format!("Daemon reconnection failed: {error}"), view);
            view.editor.set_text(text);
            return Ok(());
        }
        let images = self.collect_images_for(text, view);
        self.bounded_request(
            Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
            DaemonCommand::Prompt {
                id: None,
                active_session_id: self.active_session_id.clone(),
                message: text.to_string(),
                input: pa_types::daemon::PromptInput {
                    content: None,
                    images,
                    streaming_behavior: None,
                    queue_if_busy: None,
                    expand_prompt_templates: None,
                    source: None,
                    agent_message_id: None,
                    custom_message: None,
                    queue_key: None,
                    prefix_messages: None,
                    admission_id: None,
                },
                rest: Default::default(),
            },
        )
        .await
        .map_err(|error| anyhow!("{error:#}"))?;
        if !self.turn_active {
            self.turn_active = true;
        }
        self.start_loader(view);
        self.dirty = true;
        Ok(())
    }

    /// The working loader starts with a `Waiting` activity and a zero
    /// token count (TS `agent_start` resets the tracker).
    fn start_loader(&mut self, view: &mut AgentView) {
        view.working = Some(WorkingState {
            activity: "Waiting",
            message: None,
            download: false,
            tokens: 0,
            elapsed_secs: 0,
        });
        view.working_since = Some(std::time::Instant::now());
        self.working_tokens.reset();
    }

    /// Update the loader's activity label from one provider stream event
    /// (TS `AgentActivityTracker`: thinking/text/toolcall events switch the
    /// label and direction). Token counting lives in
    /// [`Self::track_stream_tokens`]: the event's own delta is only the
    /// last of possibly many coalesced provider deltas, so the message —
    /// not the delta — carries the token truth.
    fn track_stream_activity(&mut self, event: &Value, view: &mut AgentView) {
        let (activity, download) = match event.get("type").and_then(Value::as_str) {
            Some("thinking_start") | Some("thinking_delta") => ("Thinking", true),
            Some("text_start") | Some("text_delta") => ("Writing", true),
            Some("toolcall_start") | Some("toolcall_delta") => ("Writing code", true),
            _ => return,
        };
        if let Some(working) = &mut view.working {
            working.activity = activity;
            working.download = download;
        }
    }
}

/// The loader's token accounting (TS `AgentActivityTracker`): the live
/// count is completed-message output tokens plus max(reported usage, the
/// content estimate at 4 chars per token), reported monotonically within a
/// run. The live count derives from the streamed message itself — never
/// from per-delta sums — because the worker coalesces provider deltas into
/// latest-snapshot frames and a delta sum would undercount.
#[derive(Debug, Default)]
struct LoaderTokenTracker {
    /// Settled-message output tokens, banked at `message_end` (TS
    /// `completedTokens`).
    completed_tokens: u64,
    /// The streaming message's reported `usage.output` (TS
    /// `streamingUsageTokens`).
    streaming_usage: u64,
    /// The streaming message's content size in chars (TS accumulates the
    /// same value as a delta sum; the snapshot message carries it directly).
    streaming_chars: u64,
}

impl LoaderTokenTracker {
    /// TS `agent_start`/`reset`: a fresh run counts from zero.
    fn reset(&mut self) {
        self.completed_tokens = 0;
        self.start_message();
    }

    /// TS `message_start` (assistant): the new message's live state starts
    /// empty — its reported usage only counts from the first update.
    fn start_message(&mut self) {
        self.streaming_usage = 0;
        self.streaming_chars = 0;
    }

    /// TS `message_update`: adopt the message's reported usage and size,
    /// returning the live count.
    fn apply_streaming(&mut self, usage_output: u64, content_chars: u64) -> u64 {
        self.streaming_usage = usage_output;
        self.streaming_chars = content_chars;
        self.current()
    }

    /// TS `message_end`: bank the message's tokens into the completed
    /// count (authoritative usage when reported, else the live estimate)
    /// and clear the live state.
    fn settle(&mut self, usage_output: u64) {
        let estimate = (self.streaming_chars as f64 / 4.0).round() as u64;
        self.completed_tokens += if usage_output > 0 {
            usage_output
        } else {
            estimate
        };
        self.start_message();
    }

    /// TS `currentTokens`: completed tokens plus max(reported usage, the
    /// chars/4 estimate).
    fn current(&self) -> u64 {
        let estimate = (self.streaming_chars as f64 / 4.0).round() as u64;
        self.completed_tokens + self.streaming_usage.max(estimate)
    }
}

impl SessionUi {
    /// Slash-command dispatch (the TS interactive submission ladder reduced
    /// to this client's surface): local client commands run here, builtin
    /// client commands without a UI yet report unavailability, session
    /// commands (`compact`/`refine`/`goal`/`autonomous`) forward to the
    /// session, and unknown commands get the TS suggestion error — anything
    /// without a suggestion passes through as a prompt.
    async fn handle_slash(&mut self, text: &str, view: &mut AgentView) -> Result<()> {
        let registry = SlashCommandRegistry::builtin();
        let (name, args) = pa_types::slash_commands::parse_slash_command(text)
            .unwrap_or_else(|| (String::new(), String::new()));

        // Client-local commands this build implements (not TS builtins).
        match name.as_str() {
            "help" => {
                self.note(
                    "/help           this list\n/list           live sessions\n/switch <n|id>  switch to a session from /list\n/new            start a new session\n/exit           detach and exit",
                    view,
                );
                return Ok(());
            }
            "list" => {
                self.refresh_list(view).await?;
                return Ok(());
            }
            "switch" => {
                if args.is_empty() {
                    self.note("usage: /switch <n|id> (run /list first)", view);
                } else {
                    self.switch_to(&args, view).await?;
                }
                return Ok(());
            }
            "exit" => {
                self.exit_requested = true;
                return Ok(());
            }
            _ => {}
        }

        let Some(resolved) = registry.parse(text) else {
            // Oversized names are prompts (TS `_throwIfUnknownSlashCommand`
            // bails out before fuzzy matching). Close typos get the exact TS
            // error; everything else passes through to the model.
            if name.chars().count() > 64 {
                return self.send_prompt(text, view).await;
            }
            let candidates = registry.suggestion_candidates();
            return match pa_types::slash_commands::find_slash_command_suggestion(&name, &candidates)
            {
                Some(suggestion) => {
                    self.note(
                        &format!("Unknown command: /{name}. Did you mean /{suggestion}?"),
                        view,
                    );
                    Ok(())
                }
                None => self.send_prompt(text, view).await,
            };
        };

        let command = registry
            .get(resolved.name)
            .expect("resolved name is builtin");
        match command.execution {
            SlashCommandExecution::Session => self.send_prompt(text, view).await,
            SlashCommandExecution::Client => self.dispatch_client_command(&resolved, view).await,
        }
    }

    /// A builtin client command. Only the implemented subset runs locally;
    /// commands whose UI does not exist yet report unavailability.
    async fn dispatch_client_command(
        &mut self,
        resolved: &pa_types::slash_commands::ResolvedSlashCommand,
        view: &mut AgentView,
    ) -> Result<()> {
        match resolved.name {
            // `/clear` stays the no-argument compatibility alias of `/new`
            // (TS refuses arguments to it).
            "new" if resolved.original_name == "clear" && !resolved.args.is_empty() => {
                self.note("Usage: /clear", view);
            }
            "new" => {
                let id = create_session(&self.client, &self.create_options(), None).await?;
                self.attach_session(&id).await?;
                self.rebuild_view(view);
                self.note(&format!("started session {id}"), view);
            }
            // TS `/quit` shuts the client down; this build's exit detaches
            // and exits (the session keeps running in the daemon).
            "quit" => {
                self.exit_requested = true;
            }
            // `/resume` (TS: open the agents view, or resume a session by
            // id or path). Both paths detach this session first; the CLI
            // loop then opens the agents view or the resolved selection.
            "resume" => {
                if resolved.args.is_empty() {
                    self.open_agents_view = true;
                    self.exit_requested = true;
                } else {
                    match self.resolve_resume_selector(&resolved.args) {
                        Some(selection) => {
                            self.pending_selection = Some(selection);
                            self.exit_requested = true;
                        }
                        None => {
                            self.note(
                                &format!("could not resolve session \"{}\"", resolved.args),
                                view,
                            );
                        }
                    }
                }
            }
            // `/model [search]` (TS `handleModelCommand` →
            // `showConfigurationMenu("models")`): open the inline menu
            // panel over the cached catalog, the search term prefilled as
            // its filter; a refresh fires in the background when the
            // snapshot is stale (forced when a search argument rides the
            // command) and lands into the open picker.
            "model" => {
                self.track_command_used("model");
                let current = self.current_model(view);
                let thinking_level = self
                    .picker_initial_thinking_level(current.as_ref(), view)
                    .await;
                let options = ModelPickerOptions {
                    models: self.model_catalog.clone(),
                    current,
                    configured_providers: self.model_configured_providers.clone(),
                    recent_models: self.model_recent_models.clone(),
                    thinking_level,
                    viewport_rows: picker_viewport_rows(view.terminal_rows()),
                };
                // TS `handleModelCommand` always opens the menu (an empty
                // catalog renders the empty panel).
                let crate::model_picker::ModelCommandOutcome::Open(picker) =
                    ModelPicker::open(options, &resolved.args);
                view.model_picker = Some(*picker);
                // TS `refreshModels(initialModelSearch !== undefined)`.
                let force = !resolved.args.trim().is_empty();
                if self.model_refresh_due(force) {
                    self.spawn_model_catalog_refresh();
                }
            }
            // `/effort [level]` (TS `handleEffortCommand`): the
            // session's thinking levels drive the outcome — a model
            // without reasoning reports the TS note, a missing argument
            // opens the picker, and a valid argument applies directly.
            "effort" => {
                self.track_command_used("effort");
                let Some(state) = self.connection_state(view).await else {
                    return Ok(());
                };
                let levels: Vec<String> = state
                    .get("availableThinkingLevels")
                    .and_then(Value::as_array)
                    .map(|levels| {
                        levels
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                // TS `getAvailableThinkingLevels`: an "off"-only list is
                // no thinking surface.
                let levels: Vec<String> = if levels.len() == 1 && levels[0] == "off" {
                    Vec::new()
                } else {
                    levels
                };
                let current = state
                    .get("thinkingLevel")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                match effort_picker::effort_command(&levels, current.as_deref(), &resolved.args) {
                    effort_picker::EffortCommandOutcome::Open(picker) => {
                        view.effort_picker = Some(picker);
                    }
                    effort_picker::EffortCommandOutcome::Unsupported => {
                        self.note("Current model does not support thinking", view);
                    }
                    effort_picker::EffortCommandOutcome::Unknown { requested, levels } => {
                        // TS `showError`: the ⚠ Error row, not the muted note.
                        view.push_entry(ChatEntry::Status {
                            text: format!(
                                "\u{26a0} Error: Unknown thinking level '{requested}'. Available: {}",
                                levels.join(", ")
                            ),
                            kind: StatusKind::Error,
                        });
                        self.dirty = true;
                    }
                    effort_picker::EffortCommandOutcome::Apply { level } => {
                        self.apply_thinking_level(&level, view).await;
                    }
                }
            }
            // `/tree` (TS `showTreeSelector`): the session-tree navigator.
            "tree" => {
                if !resolved.args.is_empty() {
                    self.note("Usage: /tree", view);
                } else {
                    self.track_command_used("tree");
                    self.open_tree_selector(view, None).await?;
                }
            }
            // `/fork` (TS `showUserMessageSelector`): fork from a user
            // message into a new session.
            "fork" => {
                if !resolved.args.is_empty() {
                    self.note("Usage: /fork", view);
                } else {
                    self.track_command_used("fork");
                    self.open_fork_selector(view).await?;
                }
            }
            // `/clone` (TS `handleCloneCommand`): duplicate the session at
            // the current position.
            "clone" => {
                if !resolved.args.is_empty() {
                    self.note("Usage: /clone", view);
                } else {
                    self.track_command_used("clone");
                    self.handle_clone_command(view).await?;
                }
            }
            // TS `handleMcpCommand`'s login/logout branches: the auth
            // flows run in the client process (the composition root's
            // hook); the other management subcommands surface through the
            // `mcp` CLI command instead of the TUI.
            "mcp" => self.handle_mcp_command(resolved, view).await?,
            // TS `handleExportCommand`: an explicit `.jsonl` path exports
            // the current branch; anything else (including no argument)
            // exports HTML.
            "export" => {
                self.track_command_used("export");
                self.handle_export_command(resolved, view).await?;
            }
            // TS `handleShareCommand`: an argument is the usage error (the
            // text stays in the editor); otherwise the session exports to a
            // temp file and uploads as a secret gist.
            "share" => {
                if !resolved.args.is_empty() {
                    view.editor
                        .set_text(&format!("/{} {}", resolved.original_name, resolved.args));
                    self.error_row("Usage: /share", view);
                } else {
                    self.track_command_used("share");
                    self.handle_share_command(view).await?;
                }
            }
            // `/hotkeys` (TS `handleHotkeysCommand` after
            // `echoLocalCommand`): the typed command echoes as a user
            // message block, then the full keyboard-shortcut reference
            // renders from the EFFECTIVE bindings so user
            // `keybindings.json` overrides show their keys. Client-side
            // rows only, never durable session entries.
            "hotkeys" => {
                self.track_command_used("hotkeys");
                if !resolved.args.is_empty() {
                    // TS keeps the text in the editor on the usage error.
                    view.editor
                        .set_text(&format!("/{} {}", resolved.original_name, resolved.args));
                    self.error_row("Usage: /hotkeys", view);
                    return Ok(());
                }
                view.push_entry(ChatEntry::User {
                    text: "/hotkeys".to_string(),
                });
                view.push_entry(ChatEntry::ClientMarkdown {
                    text: crate::hotkeys::hotkeys_guide(view.editor.keybindings()),
                });
                self.dirty = true;
            }
            other => {
                self.note(
                    &format!("/{other} is not available in this client yet"),
                    view,
                );
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Session export and share (/export, /share)
    // ------------------------------------------------------------------

    /// The TS `showError` row: `⚠ Error: <message>` in the error color.
    pub(crate) fn error_row(&mut self, message: &str, view: &mut AgentView) {
        view.push_entry(ChatEntry::Status {
            text: format!("\u{26a0} Error: {message}"),
            kind: StatusKind::Error,
        });
        self.dirty = true;
    }

    /// `/export [path]` (TS `handleExportCommand`): export the session to
    /// HTML — or, for an explicit `.jsonl` path, the current branch as a
    /// JSONL file — and report the written path. The daemon owns the
    /// export; failures surface as the TS error row.
    async fn handle_export_command(
        &mut self,
        resolved: &pa_types::slash_commands::ResolvedSlashCommand,
        view: &mut AgentView,
    ) -> Result<()> {
        let command_text = if resolved.args.is_empty() {
            "/export".to_string()
        } else {
            format!("/export {}", resolved.args)
        };
        let output_path = export_share::path_command_argument(&command_text, "/export");
        let request = if output_path
            .as_deref()
            .is_some_and(|path| path.ends_with(".jsonl"))
        {
            DaemonCommand::ExportJsonl {
                id: None,
                active_session_id: self.active_session_id.clone(),
                output_path,
                rest: Default::default(),
            }
        } else {
            DaemonCommand::ExportHtml {
                id: None,
                active_session_id: self.active_session_id.clone(),
                output_path,
                rest: Default::default(),
            }
        };
        match self
            .bounded_request(Duration::from_millis(UI_REQUEST_TIMEOUT_MS), request)
            .await
        {
            Ok(data) => {
                let path = data.get("path").and_then(Value::as_str).unwrap_or_default();
                self.note(&format!("Session exported to: {path}"), view);
            }
            Err(error) => {
                self.error_row(&format!("Failed to export session: {error:#}"), view);
            }
        }
        Ok(())
    }

    /// `/share` (TS `handleShareCommand`): gate on the GitHub CLI, export
    /// the session to a temp file, then upload it as a secret gist while
    /// the cancellable loader replaces the editor.
    async fn handle_share_command(&mut self, view: &mut AgentView) -> Result<()> {
        match export_share::probe_gh_auth() {
            GhAuthStatus::NotLoggedIn => {
                self.error_row(
                    "GitHub CLI is not logged in. Run 'gh auth login' first.",
                    view,
                );
                return Ok(());
            }
            GhAuthStatus::NotInstalled => {
                self.error_row(
                    "GitHub CLI (gh) is not installed. Install it from https://cli.github.com/",
                    view,
                );
                return Ok(());
            }
            GhAuthStatus::Ok => {}
        }
        // The temp export `gh` uploads (TS `os.tmpdir()/session.html`).
        let tmp_file = std::env::temp_dir().join("session.html");
        let export = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::ExportHtml {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    output_path: Some(tmp_file.to_string_lossy().into_owned()),
                    rest: Default::default(),
                },
            )
            .await;
        if let Err(error) = export {
            self.error_row(&format!("Failed to export session: {error:#}"), view);
            return Ok(());
        }
        // The upload runs in the background: the loader keeps the UI live,
        // the run loop folds the outcome in when it lands.
        let child = match export_share::spawn_gist_create(&tmp_file) {
            Ok(child) => child,
            Err(error) => {
                let _ = std::fs::remove_file(&tmp_file);
                self.error_row(&format!("Failed to create gist: {error}"), view);
                return Ok(());
            }
        };
        let notes = self.share_notes.clone();
        let task = tokio::spawn(async move {
            let outcome = export_share::gist_outcome(child).await;
            let _ = notes.send(outcome);
        });
        self.share = Some(ShareRun {
            task,
            tmp_file: tmp_file.clone(),
        });
        view.share_loader = Some(ShareLoader::new());
        self.dirty = true;
        Ok(())
    }

    /// Whether a `/share` upload is in flight (the run loop must not end
    /// before its outcome row lands).
    pub(crate) fn share_pending(&self) -> bool {
        self.share.is_some()
    }

    /// A `/share` upload settled: drop the loader, clean the temp file, and
    /// surface the TS rows — the share URL, or the failure. A late outcome
    /// after a cancel is ignored (the run is gone, the cancel showed its
    /// own row).
    pub(crate) fn apply_share_outcome(&mut self, outcome: ShareNote, view: &mut AgentView) {
        let Some(run) = self.share.take() else {
            return;
        };
        view.share_loader = None;
        let _ = std::fs::remove_file(&run.tmp_file);
        match outcome {
            Ok(gist) => {
                self.note(
                    &format!("Share URL: {}\nGist: {}", gist.preview_url, gist.gist_url),
                    view,
                );
            }
            Err(message) => {
                self.error_row(&format!("Failed to create gist: {message}"), view);
            }
        }
        self.dirty = true;
    }

    /// One key press while the `/share` loader is open (TS
    /// `CancellableLoader`): the cancel binding aborts the upload, every
    /// other key is the loader's.
    async fn handle_share_loader_key(&mut self, key: KeyEvent, view: &mut AgentView) -> Result<()> {
        if let Some(id) = key_event_to_id(&key) {
            let kb = view.editor.keybindings();
            if kb.matches(&id, "tui.select.cancel") {
                if let Some(run) = self.share.take() {
                    // Aborting the task drops the child and kills `gh`
                    // (kill-on-drop); the temp file goes with the run.
                    run.task.abort();
                    let _ = std::fs::remove_file(&run.tmp_file);
                }
                view.share_loader = None;
                self.note("Share cancelled", view);
            }
        }
        self.dirty = true;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Session-tree navigation (/tree, /fork, /clone)
    // ------------------------------------------------------------------

    /// Open the `/tree` selector over the session tree (TS
    /// `showTreeSelector`); `initial_selected` re-opens with the previous
    /// selection after a cancelled branch summary.
    async fn open_tree_selector(
        &mut self,
        view: &mut AgentView,
        initial_selected: Option<&str>,
    ) -> Result<()> {
        let data = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::GetSessionTree {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    rest: Default::default(),
                },
            )
            .await?;
        if data
            .get("flatNodes")
            .and_then(Value::as_array)
            .is_none_or(|nodes| nodes.is_empty())
        {
            self.note("No entries in session", view);
            return Ok(());
        }
        match TreeSelector::new(
            &data,
            view.terminal_rows(),
            self.branch_summary_skip_prompt,
            self.tree_filter_mode,
        ) {
            Some(mut selector) => {
                selector.set_initial_selection(initial_selected);
                view.tree_selector = Some(selector);
            }
            None => self.note("No entries in session", view),
        }
        self.dirty = true;
        Ok(())
    }

    /// One key press while the tree selector is open.
    async fn handle_tree_selector_key(
        &mut self,
        key: KeyEvent,
        view: &mut AgentView,
    ) -> Result<()> {
        let Some(id) = key_event_to_id(&key) else {
            return Ok(());
        };
        let action = {
            let Some(selector) = view.tree_selector.as_mut() else {
                return Ok(());
            };
            let kb = view.editor.keybindings();
            selector.handle_key(kb, &id)
        };
        match action {
            TreeSelectorAction::None => {}
            TreeSelectorAction::Cancel => {
                view.tree_selector = None;
            }
            TreeSelectorAction::LabelChange { entry_id, label } => {
                let request = DaemonCommand::SetSessionEntryLabel {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    entry_id: entry_id.clone(),
                    label: label.clone(),
                    rest: Default::default(),
                };
                match self
                    .bounded_request(Duration::from_millis(UI_REQUEST_TIMEOUT_MS), request)
                    .await
                {
                    Ok(_) => {
                        if let Some(selector) = view.tree_selector.as_mut() {
                            selector.update_label(&entry_id, label.as_deref());
                        }
                    }
                    Err(error) => self.note(&format!("{error:#}"), view),
                }
            }
            TreeSelectorAction::Navigate {
                target_id,
                summarize,
                custom_instructions,
            } => {
                // Selecting the current leaf is a no-op (TS).
                let leaf = view
                    .tree_selector
                    .as_ref()
                    .and_then(|selector| selector.current_leaf_id().map(str::to_string));
                view.tree_selector = None;
                if leaf.as_deref() == Some(target_id.as_str()) {
                    self.note("Already at this point", view);
                    self.dirty = true;
                    return Ok(());
                }
                self.navigate_tree(&target_id, summarize, custom_instructions, view)
                    .await?;
            }
        }
        self.dirty = true;
        Ok(())
    }

    /// The `navigate_tree` request and its rendering (TS
    /// `_navigateTreeUnderPause` + `renderTreeNavigation`).
    async fn navigate_tree(
        &mut self,
        target_id: &str,
        summarize: bool,
        custom_instructions: Option<String>,
        view: &mut AgentView,
    ) -> Result<()> {
        let result = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS * 3),
                DaemonCommand::NavigateTree {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    target_id: target_id.to_string(),
                    summarize: Some(summarize),
                    custom_instructions,
                    replace_instructions: None,
                    label: None,
                    rest: Default::default(),
                },
            )
            .await;
        let data = match result {
            Ok(data) => data,
            Err(error) => {
                self.note(&format!("{error:#}"), view);
                return Ok(());
            }
        };
        if data.get("aborted").and_then(Value::as_bool) == Some(true) {
            // The branch summary was cancelled: re-open the tree selector
            // with the same selection (TS).
            self.note("Branch summarization cancelled", view);
            return self.open_tree_selector(view, Some(target_id)).await;
        }
        if data.get("cancelled").and_then(Value::as_bool) == Some(true) {
            self.note("Navigation cancelled", view);
            return Ok(());
        }
        self.rebuild_transcript(view).await;
        // A user-message target re-enters its text in the editor when it is
        // empty (TS `renderTreeNavigation`).
        if let Some(editor_text) = data.get("editorText").and_then(Value::as_str) {
            if view.editor.get_text().trim().is_empty() {
                view.editor.set_text(editor_text);
            }
        }
        self.note("Navigated to selected point", view);
        self.dirty = true;
        Ok(())
    }

    /// Open the `/fork` selector over the session's user messages (TS
    /// `showUserMessageSelector`).
    async fn open_fork_selector(&mut self, view: &mut AgentView) -> Result<()> {
        let data = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::GetUserMessagesForForking {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    rest: Default::default(),
                },
            )
            .await?;
        let messages: Vec<crate::user_message_selector::UserMessageItem> = data
            .get("messages")
            .and_then(Value::as_array)
            .map(|messages| {
                messages
                    .iter()
                    .filter_map(|message| {
                        Some(crate::user_message_selector::UserMessageItem {
                            id: message.get("entryId")?.as_str()?.to_string(),
                            text: message.get("text")?.as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if messages.is_empty() {
            self.note("No messages to fork from", view);
            return Ok(());
        }
        view.fork_selector = Some(UserMessageSelector::new(messages));
        self.dirty = true;
        Ok(())
    }

    /// One key press while the fork selector is open.
    async fn handle_fork_selector_key(
        &mut self,
        key: KeyEvent,
        view: &mut AgentView,
    ) -> Result<()> {
        let Some(id) = key_event_to_id(&key) else {
            return Ok(());
        };
        let action = {
            let Some(selector) = view.fork_selector.as_mut() else {
                return Ok(());
            };
            let kb = view.editor.keybindings();
            selector.handle_key(kb, &id)
        };
        match action {
            UserMessageSelectorAction::Select(entry_id) => {
                view.fork_selector = None;
                self.fork(&entry_id, None, view).await?;
            }
            UserMessageSelectorAction::Cancel => {
                view.fork_selector = None;
            }
            UserMessageSelectorAction::None => {}
        }
        self.dirty = true;
        Ok(())
    }

    /// The `fork` request (TS `AgentSessionRuntime.fork`): the worker
    /// copies the path into a new session and switches to it.
    async fn fork(
        &mut self,
        entry_id: &str,
        position: Option<pa_types::daemon::ForkPosition>,
        view: &mut AgentView,
    ) -> Result<()> {
        let data = match self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS * 3),
                DaemonCommand::Fork {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    entry_id: entry_id.to_string(),
                    position,
                    rest: Default::default(),
                },
            )
            .await
        {
            Ok(data) => data,
            Err(error) => {
                self.note(&format!("fork failed: {error:#}"), view);
                return Ok(());
            }
        };
        if data.get("cancelled").and_then(Value::as_bool) == Some(true) {
            return Ok(());
        }
        self.rebuild_transcript(view).await;
        let selected_text = data.get("selectedText").and_then(Value::as_str);
        match selected_text {
            Some(text) => view.editor.set_text(text),
            None => view.editor.set_text(""),
        }
        self.note("Forked to new session", view);
        self.dirty = true;
        Ok(())
    }

    /// `/clone` (TS `handleCloneCommand`): fork at the current leaf.
    async fn handle_clone_command(&mut self, view: &mut AgentView) -> Result<()> {
        let data = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::GetSessionTree {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    rest: Default::default(),
                },
            )
            .await?;
        let Some(leaf_id) = data.get("leafId").and_then(Value::as_str) else {
            self.note("Nothing to clone yet", view);
            return Ok(());
        };
        if leaf_id.is_empty() {
            self.note("Nothing to clone yet", view);
            return Ok(());
        }
        self.fork(leaf_id, Some(pa_types::daemon::ForkPosition::At), view)
            .await?;
        self.note("Cloned to new session", view);
        self.dirty = true;
        Ok(())
    }

    /// The armed double-Escape action, taken once inside the window (TS
    /// `takeEscapeRepeatAction`).
    fn take_escape_repeat_action(&mut self) -> Option<&'static str> {
        let action = self.escape_repeat_action;
        if let Some(until) = self.escape_repeat_until {
            if Instant::now() < until {
                self.escape_repeat_action = None;
                self.escape_repeat_until = None;
                return action;
            }
        }
        self.escape_repeat_action = None;
        self.escape_repeat_until = None;
        None
    }

    /// Arm the double-Escape action for 500ms (TS `armEscapeRepeat`): the
    /// tree when the session is idle or the editor empty, the clear action
    /// otherwise.
    fn arm_escape_repeat(&mut self, action: &'static str) {
        self.escape_repeat_action = Some(action);
        self.escape_repeat_until = Some(Instant::now() + ESCAPE_REPEAT_WINDOW_MS);
    }

    /// `/mcp <login|logout> <name>` (TS `handleMcpCommand`): usage errors,
    /// then the composition root's auth flow. Only the login prompts on
    /// the terminal, so `needs_terminal_suspension` covers it.
    async fn handle_mcp_command(
        &mut self,
        resolved: &pa_types::slash_commands::ResolvedSlashCommand,
        view: &mut AgentView,
    ) -> Result<()> {
        let Some(auth) = self.client_auth.clone() else {
            self.note("/mcp is not available in this client yet", view);
            return Ok(());
        };
        let note = crate::client_auth::run_mcp_auth_command(auth.0.as_ref(), &resolved.args).await;
        self.note(&note, view);
        Ok(())
    }

    /// Whether dispatching this input needs the terminal handed over
    /// (raw-mode off, alternate screen left) so the auth flow can prompt.
    pub(crate) fn needs_terminal_suspension(&self, text: &str) -> bool {
        let Some((name, args)) = pa_types::slash_commands::parse_slash_command(text) else {
            return false;
        };
        if name != "mcp" || self.client_auth.is_none() {
            return false;
        }
        matches!(args.split_whitespace().next(), Some("login"))
    }

    /// `/resume <selector>`: a session file path, an `<id>.jsonl` under the
    /// sessions dir, or a live daemon session id (attach). Mirrors the CLI
    /// selector resolution in `interactive_mode.rs`.
    fn resolve_resume_selector(&self, selector: &str) -> Option<SessionSelection> {
        let selector = selector.trim();
        if selector.is_empty() {
            return None;
        }
        let path = std::path::Path::new(selector);
        if path.is_file() {
            return Some(SessionSelection::Resume(path.to_path_buf()));
        }
        if let Some(dir) = &self.session_dir {
            let candidate = dir.join(format!("{selector}.jsonl"));
            if candidate.is_file() {
                return Some(SessionSelection::Resume(candidate));
            }
        }
        Some(SessionSelection::Attach(selector.to_string()))
    }

    /// Options for `/new`: same socket, cwd, persistence, and script seam as
    /// the original run.
    fn create_options(&self) -> InteractiveOptions {
        InteractiveOptions {
            socket_path: self.client.socket_path().to_path_buf(),
            cwd: self.cwd.clone(),
            session_dir: self.session_dir.clone(),
            script_path: self.script_path.clone(),
            model_selection: self.model_selection.clone(),
            model_catalog: self.model_catalog.clone(),
            model_configured_providers: self.model_configured_providers.clone(),
            model_recent_models: self.model_recent_models.clone(),
            default_thinking_level: self.default_thinking_level.clone(),
            no_session: false,
            session: SessionSelection::New,
            initial_message: None,
            telemetry_disabled: self.telemetry_disabled,
            theme: String::new(),
            code_block_indent: self.code_block_indent.clone(),
            show_images: self.show_images,
            fullscreen_mouse: self.fullscreen_mouse,
            tree_filter_mode: self.tree_filter_mode.wire_name().to_string(),
            branch_summary_skip_prompt: self.branch_summary_skip_prompt,
            version: String::new(),
            onboarding: None,
            client_auth: self.client_auth.clone(),
            telemetry: self.telemetry.clone(),
            keybindings: self.keybindings.clone(),
            // `/new` starts a fresh root session: no depth label.
            session_rlm_depth: None,
            session_has_children: false,
        }
    }

    async fn refresh_list(&mut self, view: &mut AgentView) -> Result<()> {
        let data = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::List {
                    id: None,
                    all: None,
                    cwd: None,
                    session_dir: None,
                    include_client_owned: None,
                    rest: Default::default(),
                },
            )
            .await?;
        let sessions = data
            .get("sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.list_rows = sorted_session_rows(sessions);
        let sessions = &self.list_rows;
        let mut lines = String::from("live sessions:");
        if sessions.is_empty() {
            lines.push_str("\n  (none)");
        }
        for (index, row) in sessions.iter().enumerate() {
            let id = row.get("id").and_then(Value::as_str).unwrap_or_default();
            let current = if id == self.active_session_id {
                "*"
            } else {
                " "
            };
            let name = row
                .get("sessionName")
                .and_then(Value::as_str)
                .or_else(|| row.get("sessionId").and_then(Value::as_str))
                .unwrap_or_default();
            let activity = row
                .get("activity")
                .and_then(Value::as_str)
                .unwrap_or("idle");
            let cwd = row.get("cwd").and_then(Value::as_str).unwrap_or_default();
            lines.push_str(&format!(
                "\n{current} {}. {name} ({id}) {activity} {cwd}",
                index + 1
            ));
        }
        lines.push_str("\nswitch with /switch <n|id>");
        self.note(&lines, view);
        Ok(())
    }

    /// `/switch`: resolve the argument against the cached `/list` rows (1-based
    /// index or session id), then reattach.
    async fn switch_to(&mut self, target: &str, view: &mut AgentView) -> Result<()> {
        let id = match target.parse::<usize>() {
            Ok(index) => self
                .list_rows
                .get(index.wrapping_sub(1))
                .and_then(|row| row.get("id").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| target.to_string()),
            Err(_) => target.to_string(),
        };
        if id == self.active_session_id {
            self.note("already attached to that session", view);
            return Ok(());
        }
        match self.attach_session(&id).await {
            Ok(()) => {
                self.rebuild_view(view);
                self.note(&format!("switched to session {id}"), view);
            }
            Err(error) => {
                self.note(&format!("switch to {id} failed: {error:#}"), view);
            }
        }
        Ok(())
    }

    /// The Ctrl+C exit hint is armed (TS `isCtrlCExitHintVisible`): a
    /// second press inside the window terminates the client.
    fn ctrl_c_hint_visible(&self) -> bool {
        self.ctrl_c_hint_until
            .is_some_and(|until| Instant::now() < until)
    }

    /// Arm the Ctrl+C exit hint (TS `showCtrlCExitHint`).
    fn show_ctrl_c_hint(&mut self) {
        self.ctrl_c_hint_until = Some(Instant::now() + Duration::from_millis(CTRL_C_EXIT_HINT_MS));
    }

    /// Disarm the hint (TS `clearCtrlCExitHint`: escape, editing text, or
    /// shutdown).
    fn clear_ctrl_c_hint(&mut self) {
        self.ctrl_c_hint_until = None;
    }

    /// The tray override label while the exit hint is armed (TS
    /// `getTrayOverrideLabel`: `Press Ctrl+C again to exit`).
    pub(crate) fn tray_override(&self) -> Option<String> {
        if !self.ctrl_c_hint_visible() {
            return None;
        }
        let key = self
            .keybindings
            .first_key("app.clear")
            .map(|key| crate::keybindings::format_key_text(&key))
            .unwrap_or_else(|| "Ctrl+C".to_string());
        Some(format!("Press {key} again to exit"))
    }

    /// One key press while the `/model` picker is open: Esc/Ctrl+C close
    /// it without applying; Enter applies the selection.
    async fn handle_model_picker_key(&mut self, key: KeyEvent, view: &mut AgentView) -> Result<()> {
        let Some(id) = key_event_to_id(&key) else {
            return Ok(());
        };
        // The picker consumes Ctrl+C (close, not exit): report the handled
        // press so the force-quit guard can disarm once the whole pair was
        // consumed with TS semantics.
        if id == "ctrl+c" {
            self.exit_guard.note_ctrl_c_handled();
        }
        let action = view
            .model_picker
            .as_mut()
            .map(|picker| picker.handle_key(&id, view.editor.keybindings()));
        match action {
            Some(ModelPickerAction::None) => {}
            Some(ModelPickerAction::Cancel) => {
                view.model_picker = None;
                self.dirty = true;
            }
            Some(ModelPickerAction::Apply(applied)) => {
                view.model_picker = None;
                self.apply_model_selection(&applied.provider, &applied.model_id, view)
                    .await;
                // A user-edited effort applies after the model switch (TS
                // `completeModelSelection`: `setModel`, then
                // `applyThinkingLevel` — the level row only on success).
                if let Some(level) = applied.effort {
                    self.apply_thinking_level(&level, view).await;
                }
            }
            None => {}
        }
        Ok(())
    }

    /// A mouse report (TS `handleFullscreenInput`'s wheel branch): wheel
    /// turns scroll the transcript window by three lines; while a picker,
    /// selector, or loader owns the frame (the TS overlay-focus gate) or
    /// tracking is inactive, reports are consumed without scrolling. Click,
    /// drag, and release reports are consumed too — the selection surface is
    /// not ported yet.
    pub(crate) fn handle_mouse(&mut self, event: crate::mouse::MouseEvent, view: &mut AgentView) {
        if !crate::mouse_tracking::active() {
            return;
        }
        // TS `isFullscreenOverlayFocused`: the wheel never scrolls while an
        // overlay (the `/model` and `/effort` pickers, the `/tree` and
        // `/fork` selectors, the `/share` loader) owns the frame.
        if view.model_picker.is_some()
            || view.effort_picker.is_some()
            || view.tree_selector.is_some()
            || view.fork_selector.is_some()
            || view.share_loader.is_some()
        {
            return;
        }
        if let Some(delta) = crate::mouse::wheel_scroll_delta(&event) {
            view.scroll_by(delta);
            self.dirty = true;
        }
    }

    /// A bracketed paste (TS routes terminal paste into the focused input):
    /// an open `/model` picker pastes into its search field; otherwise the
    /// editor takes it.
    pub(crate) fn handle_paste(&mut self, text: &str, view: &mut AgentView) {
        if let Some(picker) = view.model_picker.as_mut() {
            picker.paste(text);
            self.dirty = true;
            return;
        }
        let _ = view.editor.handle_paste(text);
    }

    /// One key press while the `/effort` picker is open: Esc/Ctrl+C close
    /// it without applying; Enter applies the picked level.
    async fn handle_effort_picker_key(
        &mut self,
        key: KeyEvent,
        view: &mut AgentView,
    ) -> Result<()> {
        let Some(id) = key_event_to_id(&key) else {
            return Ok(());
        };
        // The picker consumes Ctrl+C (close, not exit): report the handled
        // press so the force-quit guard can disarm once the whole pair was
        // consumed with TS semantics.
        if id == "ctrl+c" {
            self.exit_guard.note_ctrl_c_handled();
        }
        let action = view
            .effort_picker
            .as_mut()
            .map(|picker| picker.handle_key(&id, view.editor.keybindings()));
        match action {
            Some(EffortPickerAction::None) => {}
            Some(EffortPickerAction::Cancel) => {
                view.effort_picker = None;
                self.dirty = true;
            }
            Some(EffortPickerAction::Apply { level }) => {
                view.effort_picker = None;
                self.apply_thinking_level(&level, view).await;
            }
            None => {}
        }
        Ok(())
    }

    /// The session's current model, matched against the picker catalog (the
    /// daemon state reports the id; the catalog entry supplies the
    /// provider).
    fn current_model(&self, view: &AgentView) -> Option<CurrentModel> {
        let model_id = view.chrome.model_id.as_deref()?;
        let model = self
            .model_catalog
            .iter()
            .find(|model| model.id == model_id)?;
        Some(CurrentModel {
            provider: model.provider.clone(),
            model_id: model.id.clone(),
        })
    }

    /// Fire a background `get_model_catalog` refresh (TS
    /// `getModelSelectorRefreshPromise` + `getConnectionAvailableModels`):
    /// the response lands through the run loop's channel, and failures
    /// leave the current snapshot alone.
    pub(crate) fn spawn_model_catalog_refresh(&self) {
        let client = self.client.clone();
        let active_session_id = self.active_session_id.clone();
        let updates = self.catalog_updates.clone();
        tokio::spawn(async move {
            let Ok(value) = client
                .request_ok(DaemonCommand::GetModelCatalog {
                    id: None,
                    active_session_id,
                    rest: Default::default(),
                })
                .await
            else {
                // TS startup fetches fail silently (`getModelCandidates`
                // catches); the menu-open refresh surfaces the error only
                // while the menu is open, and the picker catalogs stay as
                // they are.
                return;
            };
            let models: Vec<pa_types::ai::Model> = value
                .get("models")
                .cloned()
                .and_then(|models| serde_json::from_value(models).ok())
                .unwrap_or_default();
            let configured_providers: std::collections::HashSet<String> = value
                .get("configuredProviders")
                .and_then(Value::as_array)
                .map(|providers| {
                    providers
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let _ = updates.send(ModelCatalogUpdate {
                models,
                configured_providers,
            });
        });
    }

    /// Whether the catalog refresh is due (TS `getModelSelectorRefreshPromise`:
    /// forced, never fetched, or older than the TTL).
    pub(crate) fn model_refresh_due(&self, force: bool) -> bool {
        force
            || match self.models_fetched_at {
                None => true,
                Some(fetched) => fetched.elapsed() > MODEL_CATALOG_REFRESH_TTL,
            }
    }

    /// Fold a landed catalog refresh into the session and any open picker
    /// (TS `applyConnectionModelCatalog` + the menu's `updateModels`).
    pub(crate) fn apply_model_catalog(&mut self, update: ModelCatalogUpdate, view: &mut AgentView) {
        self.model_catalog = update.models;
        self.model_configured_providers = update.configured_providers;
        self.models_fetched_at = Some(std::time::Instant::now());
        let current = self.current_model(view);
        if let Some(picker) = view.model_picker.as_mut() {
            picker.update_state(
                current,
                self.model_catalog.clone(),
                self.model_configured_providers.clone(),
            );
        }
        self.dirty = true;
    }

    /// The picker's effort seed (TS `showConfigurationMenu`'s `thinkingLevel`
    /// option): the session's live level for a reasoning current model,
    /// else the settings default (`"medium"` when unset).
    async fn picker_initial_thinking_level(
        &mut self,
        current: Option<&CurrentModel>,
        view: &mut AgentView,
    ) -> Option<pa_types::ai::ModelThinkingLevel> {
        let reasoning = current.and_then(|current| {
            self.model_catalog
                .iter()
                .find(|model| model.provider == current.provider && model.id == current.model_id)
                .map(|model| model.reasoning)
        });
        if reasoning == Some(true) {
            let level = self
                .connection_state(view)
                .await
                .as_ref()
                .and_then(|state| state.get("thinkingLevel"))
                .and_then(Value::as_str)
                .and_then(pa_types::ai::thinking_level_from_str);
            return level;
        }
        self.default_thinking_level
            .as_deref()
            .and_then(pa_types::ai::thinking_level_from_str)
            .or(Some(pa_types::ai::ModelThinkingLevel::Medium))
    }

    /// Apply a picked model (TS `applySelectedModel` + the
    /// `completeModelSelection` status row): the daemon `set_model` command
    /// switches the live session — the agent, the provider target, and the
    /// session's settings default follow — then the client refreshes its
    /// model label and records the `Model: <id>` status row. A failure
    /// surfaces as the error note instead.
    async fn apply_model_selection(
        &mut self,
        provider: &str,
        model_id: &str,
        view: &mut AgentView,
    ) {
        let switched = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::SetModel {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    provider: provider.to_string(),
                    model_id: model_id.to_string(),
                    rest: Default::default(),
                },
            )
            .await;
        match switched {
            Ok(_) => {
                // The create path's runtime config carries the picked model,
                // so `/new` sessions start on it too (TS settings default).
                self.model_selection.provider = Some(provider.to_string());
                self.model_selection.model = Some(model_id.to_string());
                self.refresh_model_label(view).await;
                self.note(&format!("Model: {model_id}"), view);
            }
            Err(error) => {
                // TS `showError`: the ⚠ Error row with the error tone.
                view.push_entry(ChatEntry::Status {
                    text: format!("\u{26a0} Error: {error:#}"),
                    kind: StatusKind::Error,
                });
                self.dirty = true;
            }
        }
    }

    /// Apply a thinking level (TS `applyThinkingLevel`): the daemon
    /// `set_thinking_level` command switches the session's level (durable
    /// row and settings default included), then the client records the
    /// `Thinking level: <level>` status row.
    async fn apply_thinking_level(&mut self, level: &str, view: &mut AgentView) {
        let switched = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::SetThinkingLevel {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    level: level.to_string(),
                    rest: Default::default(),
                },
            )
            .await;
        match switched {
            Ok(_) => self.note(&format!("Thinking level: {level}"), view),
            Err(error) => {
                // TS `showError`: the ⚠ Error row with the error tone.
                view.push_entry(ChatEntry::Status {
                    text: format!("\u{26a0} Error: {error:#}"),
                    kind: StatusKind::Error,
                });
                self.dirty = true;
            }
        }
    }

    /// The session's connection state (TS `AgentConnectionState`): the
    /// worker's `get_state` response. `None` surfaces the failure as a
    /// note; callers keep the transcript unchanged then.
    async fn connection_state(&mut self, view: &mut AgentView) -> Option<Value> {
        match self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::GetState {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    rest: Default::default(),
                },
            )
            .await
        {
            Ok(data) => Some(data),
            Err(error) => {
                self.note(&format!("{error:#}"), view);
                None
            }
        }
    }

    /// Refresh the chrome model label after a live switch (TS
    /// `applyModelSwitchUiState` reads the state and patches the footer).
    async fn refresh_model_label(&mut self, view: &mut AgentView) {
        let state = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::GetState {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    rest: Default::default(),
                },
            )
            .await;
        if let Ok(data) = state {
            if let Some(model_id) = data
                .get("model")
                .and_then(|model| model.get("id"))
                .and_then(Value::as_str)
            {
                view.chrome.model_id = Some(model_id.to_string());
            }
            self.dirty = true;
        }
    }

    /// Abort the active turn off the UI loop (TS `interruptOrClearInput`
    /// fires `void abort()`): the request never blocks key handling, and a
    /// failure surfaces later as a transcript note.
    fn abort_turn(&self) {
        let client = self.client.clone();
        let active_session_id = self.active_session_id.clone();
        let notes = self.notes.clone();
        tokio::spawn(async move {
            let result = client
                .request_ok(DaemonCommand::Abort {
                    id: None,
                    active_session_id,
                    rest: Default::default(),
                })
                .await;
            if let Err(error) = result {
                let _ = notes.send(format!("the abort failed: {error:#}"));
            }
        });
    }

    /// Apply one background note (a failed abort request) to the transcript.
    pub(crate) fn apply_background_note(&mut self, text: &str, view: &mut AgentView) {
        self.note(text, view);
    }

    /// A compaction succeeded and the durable transcript was rebuilt (it
    /// now starts at the compaction summary): re-fetch it and replace the
    /// view's chat (TS `rebuildChatFromMessages`). Best effort — a failed
    /// fetch keeps the pushed outcome row instead of an empty transcript.
    pub(crate) async fn rebuild_transcript(&mut self, view: &mut AgentView) {
        self.transcript_stale = false;
        let Ok(data) = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::GetMessages {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    rest: Default::default(),
                },
            )
            .await
        else {
            return;
        };
        let Some(messages) = data.get("messages").and_then(Value::as_array) else {
            return;
        };
        let entries = crate::snapshot::transcript_to_entries(messages);
        view.clear_chat();
        for entry in entries {
            view.push_entry(entry);
        }
        view.follow();
        self.dirty = true;
    }

    /// The `tui exit` reason recorded at the point the loop stopped.
    pub(crate) fn exit_reason(&self) -> &'static str {
        self.exit_reason
    }

    /// Report the first scroll action of the run (`tui scroll used`),
    /// fire-and-forget so the keypress never waits on the telemetry flush.
    fn track_scroll(&mut self, action: &'static str, resumed_following: bool) {
        if self.scroll_adoption_emitted {
            return;
        }
        self.scroll_adoption_emitted = true;
        if let Some(telemetry) = self.telemetry.clone() {
            tokio::spawn(async move {
                telemetry.scroll_used(action, resumed_following).await;
            });
        }
    }

    /// Report a builtin client-command submission (`agent command used`),
    /// fire-and-forget like the scroll event: the command's handling never
    /// waits on the telemetry flush.
    fn track_command_used(&mut self, command: &'static str) {
        if let Some(telemetry) = self.telemetry.clone() {
            tokio::spawn(async move {
                telemetry.command_used(command).await;
            });
        }
    }

    pub(crate) async fn handle_key(
        &mut self,
        key: KeyEvent,
        view: &mut AgentView,
        running: &mut bool,
    ) -> Result<()> {
        // The `/model` picker owns the frame while open: every key goes to
        // it, before the editor, the viewport keys, or Ctrl+C (which
        // cancels the picker instead of aborting a turn).
        if view.model_picker.is_some() {
            return self.handle_model_picker_key(key, view).await;
        }
        // The `/effort` picker owns the frame the same way.
        if view.effort_picker.is_some() {
            return self.handle_effort_picker_key(key, view).await;
        }
        // The `/tree` and `/fork` selectors own the frame the same way.
        if view.tree_selector.is_some() {
            return self.handle_tree_selector_key(key, view).await;
        }
        if view.fork_selector.is_some() {
            return self.handle_fork_selector_key(key, view).await;
        }
        // The `/share` loader owns the frame while an upload runs (TS the
        // loader takes focus): the cancel binding aborts, other keys are
        // the loader's.
        if view.share_loader.is_some() {
            return self.handle_share_loader_key(key, view).await;
        }
        let Some(id) = key_event_to_id(&key) else {
            return Ok(());
        };
        // The dispatch order below mirrors TS `CustomEditor.handleInput`:
        // paste image, then `app.input.clear`, then `app.exit` (only when
        // the editor is empty; otherwise ctrl+d falls through to the
        // editor's delete-char-forward), then the app actions in
        // registration order (`app.clear` first, `app.tools.expand` next).
        // Every match goes through the effective bindings, so a user
        // `keybindings.json` override moves both the handler and the hint.
        // Image paste (TS `app.clipboard.pasteImage`, default ctrl+v):
        // reads the clipboard image and inserts its marker into the
        // editor. The editor's own ctrl+v is unbound otherwise, so the
        // match is exact before any editor motion.
        if view
            .editor
            .keybindings()
            .matches(&id, "app.clipboard.pasteImage")
        {
            self.handle_clipboard_image_paste(view).await;
            return Ok(());
        }
        if view.editor.keybindings().matches(&id, "app.input.clear") {
            view.editor.cancel_autocomplete();
            self.clear_ctrl_c_hint();
            // Double-Escape (TS `handleEscape`'s repeat window): the second
            // press within 500ms opens the tree when the session is idle or
            // the editor empty, and clears the input otherwise.
            if let Some(action) = self.take_escape_repeat_action() {
                if action == "tree" {
                    self.open_tree_selector(view, None).await?;
                } else {
                    view.editor.set_text("");
                }
                self.dirty = true;
                return Ok(());
            }
            let action = if self.turn_active || view.editor.get_text().trim().is_empty() {
                "tree"
            } else {
                "clear"
            };
            self.arm_escape_repeat(action);
            return Ok(());
        }
        if view.editor.keybindings().matches(&id, "app.exit") && view.editor.get_text().is_empty() {
            self.exit_reason = "ctrl_d";
            *running = false;
            return Ok(());
        }
        if view.editor.keybindings().matches(&id, "app.clear") {
            // One handled Ctrl+C press: the force-quit guard disarms once
            // every observed press of the pair was handled without an exit
            // (abort / autocomplete cancel, TS `handleCtrlC`); an exit keeps
            // the deadline and re-arms it on the loop break.
            if id == "ctrl+c" {
                self.exit_guard.note_ctrl_c_handled();
            }
            if view.editor.is_showing_autocomplete() {
                view.editor.cancel_autocomplete();
                self.clear_ctrl_c_hint();
                return Ok(());
            }
            // TS `handleCtrlC`: the first press interrupts (aborting an
            // active turn, showing the exit hint); a second press inside
            // the hint window shuts down unconditionally — no turn wait,
            // no abort wait — so the client always exits promptly.
            if self.ctrl_c_hint_visible() {
                self.exit_reason = "ctrl_c_twice";
                *running = false;
                return Ok(());
            }
            if self.turn_active {
                self.abort_turn();
                self.note("aborting the current turn", view);
            }
            self.show_ctrl_c_hint();
            self.dirty = true;
            return Ok(());
        }
        // TS `app.shortcuts` (default `?`, empty editor only — the action
        // loop's `getText().length === 0` gate): mount the quick-shortcut
        // guide above the dock until the next submission.
        if view.editor.keybindings().matches(&id, "app.shortcuts")
            && view.editor.get_text().is_empty()
        {
            view.shortcut_guide = Some(crate::hotkeys::shortcut_guide(view.editor.keybindings()));
            self.dirty = true;
            return Ok(());
        }
        if view.editor.keybindings().matches(&id, "app.tools.expand") {
            // TS `app.tools.expand` (default ctrl+o) cycles conversation
            // detail: overview -> details -> all -> overview.
            view.detail = view.detail.next();
            self.dirty = true;
            return Ok(());
        }
        // Transcript viewport keys (TS tui.ts consumes them before the
        // editor in fullscreen): page scroll, top, follow.
        let (page_up, page_down, to_top, follow) = {
            let kb = view.editor.keybindings();
            (
                kb.matches(&id, "tui.viewport.pageUp"),
                kb.matches(&id, "tui.viewport.pageDown"),
                kb.matches(&id, "tui.viewport.top"),
                kb.matches(&id, "tui.viewport.follow"),
            )
        };
        if page_up {
            view.scroll_by(-(view.page_size() as isize));
            self.track_scroll("page_up", view.is_following());
            self.dirty = true;
            return Ok(());
        }
        if page_down {
            view.scroll_by(view.page_size() as isize);
            self.track_scroll("page_down", view.is_following());
            self.dirty = true;
            return Ok(());
        }
        if to_top {
            view.scroll_to_top();
            self.track_scroll("top", view.is_following());
            self.dirty = true;
            return Ok(());
        }
        if follow {
            view.scroll_to_bottom();
            self.track_scroll("follow", view.is_following());
            self.dirty = true;
            return Ok(());
        }
        // The subagent summary line owns focus while focused (TS
        // `SubagentSummaryLine.handleInput`): confirm/open opens the scoped
        // agents view, up/cancel/back returns to the editor, expand cycles
        // the conversation detail, and every other key falls through to the
        // editor after releasing the focus.
        if self.subagents_focused {
            let kb = view.editor.keybindings();
            if kb.matches(&id, "tui.select.confirm") || kb.matches(&id, "app.agents.open") {
                self.open_scoped_agents_view(view);
                return Ok(());
            }
            if kb.matches(&id, "tui.select.up")
                || kb.matches(&id, "tui.select.cancel")
                || kb.matches(&id, "app.agents.back")
            {
                self.subagents_focused = false;
                self.update_subagent_summary(view);
                self.dirty = true;
                return Ok(());
            }
            if kb.matches(&id, "app.tools.expand") {
                view.detail = view.detail.next();
                self.subagents_focused = false;
                self.dirty = true;
                return Ok(());
            }
            self.subagents_focused = false;
            self.update_subagent_summary(view);
        }
        // TS `app.subagents.focus` (default alt+a): the summary line takes
        // focus when it is selectable.
        if view
            .editor
            .keybindings()
            .matches(&id, "app.subagents.focus")
        {
            if self.subagents_selectable() {
                self.subagents_focused = true;
                self.update_subagent_summary(view);
            }
            self.dirty = true;
            return Ok(());
        }
        // Agents-back (TS `custom-editor.ts` onAgentsBack): with an empty
        // editor the bound key (default left) hands the terminal to the
        // agents view instead of moving the cursor; with text in the editor
        // the key stays an editor cursor motion. A `--no-session` run has
        // no daemon fleet to browse, so the key stays consumed but only
        // reports that (TS `requestAgentsView` status).
        if view.editor.keybindings().matches(&id, "app.agents.back")
            && view.editor.get_text().trim().is_empty()
        {
            if self.return_to_agents_view {
                self.open_agents_view = true;
                self.exit_requested = true;
            } else {
                self.note(
                    "The agents view needs a daemon-hosted session; start normally (without --no-session) to browse sessions",
                    view,
                );
            }
            self.dirty = true;
            return Ok(());
        }
        // `app.session.tree` / `app.session.fork` (TS editor actions): the
        // bound keys open the surfaces when the editor is empty.
        if view.editor.get_text().trim().is_empty() {
            let kb = view.editor.keybindings();
            if kb.matches(&id, "app.session.tree") {
                self.open_tree_selector(view, None).await?;
                self.dirty = true;
                return Ok(());
            }
            if kb.matches(&id, "app.session.fork") {
                self.open_fork_selector(view).await?;
                self.dirty = true;
                return Ok(());
            }
        }
        view.editor.handle_input(&id);
        // TS clears the exit hint as soon as the editor carries text: the
        // `Press Ctrl+C again to exit` row belongs to the empty prompt.
        if !view.editor.get_text().is_empty() {
            self.clear_ctrl_c_hint();
        }
        for event in view.editor.take_events() {
            if let crate::editor::EditorEvent::Submitted(text) = event {
                view.editor.add_to_history(&text);
                self.submit_prompt(&text, view).await?;
            }
        }
        self.dirty = true;
        Ok(())
    }

    pub(crate) fn apply_client_event(&mut self, event: DaemonClientEvent, view: &mut AgentView) {
        match event {
            DaemonClientEvent::SessionEvent {
                active_session_id,
                event,
            } => {
                if active_session_id != self.active_session_id {
                    return;
                }
                if let Some(update) = event_to_update(&event) {
                    self.apply_update(update, view);
                }
            }
            DaemonClientEvent::SessionClosed {
                active_session_id,
                reason,
            } => {
                if active_session_id == self.active_session_id {
                    self.turn_active = false;
                    view.working = None;
                    self.note(&format!("session closed ({reason})"), view);
                }
            }
            DaemonClientEvent::DirectLinkLost { active_session_id } => {
                // TS `handleTransportClose`: a direct-transport loss is
                // never itself a session loss — the interactive loop
                // re-attaches through the supervisor. Only the active
                // session arms the loop; a replaced link (session switch)
                // reports its old session and is ignored here.
                if active_session_id == self.active_session_id {
                    self.transport_lost = Some(active_session_id);
                }
            }
            DaemonClientEvent::DaemonClosing { reason, update } => {
                match update {
                    Some(update) => {
                        // Spec §10: reattach is the default end state. The
                        // banner carries the resume contract; the reconnect
                        // loop in the interactive run drives the rest (UI
                        // stays mounted, retry with backoff up to 10 min,
                        // reattach by durable id once the successor serves).
                        let names = update
                            .sessions
                            .iter()
                            .map(|row| {
                                row.get("name")
                                    .and_then(|value| value.as_str())
                                    .filter(|name| !name.is_empty())
                                    .unwrap_or_else(|| {
                                        row.get("sessionId")
                                            .and_then(|value| value.as_str())
                                            .unwrap_or_default()
                                    })
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        view.push_entry(crate::chat::ChatEntry::Status {
                            text: format!(
                                "Prime Agent is updating — restarting the daemon (about {}s). {} will resume automatically.",
                                update.est_seconds.max(1),
                                if names.is_empty() { "Your session".to_string() } else { names }
                            ),
                            kind: crate::chat::StatusKind::Info,
                        });
                        self.reconnect = Some(update);
                    }
                    None => {
                        self.note(&format!("the daemon is shutting down ({reason})"), view);
                    }
                }
            }
            // The roster push keeps the subagent summary counts live (TS
            // `subscribeAgentRoster` -> `updateSubagentSummaryLine`).
            DaemonClientEvent::RosterUpdate {
                changed,
                removed,
                resync,
            } => {
                self.apply_roster_update(changed, removed, resync);
                self.update_subagent_summary(view);
                self.dirty = true;
            }
            // Saved-session list frames belong to the agents-view UI; the
            // session view only reads its own session.
            DaemonClientEvent::SessionListItem { .. }
            | DaemonClientEvent::SessionListProgress { .. } => {}
        }
    }

    fn apply_update(&mut self, update: TurnUpdate, view: &mut AgentView) {
        match update {
            TurnUpdate::TurnStarted => {
                self.turn_active = true;
                self.turn_error_shown = false;
                self.start_loader(view);
            }
            TurnUpdate::UserMessage(text) => {
                view.push_entry(ChatEntry::User { text });
            }
            TurnUpdate::CustomRow(entry) => {
                view.push_entry(entry);
            }
            TurnUpdate::AssistantMessage {
                message,
                streaming,
                stream_event,
            } => {
                self.apply_assistant_message(&message, streaming, stream_event.as_ref(), view);
            }
            TurnUpdate::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                crate::snapshot::apply_tool_execution_start(view, &tool_call_id, &tool_name, args);
                self.set_working_activity("Executing", false, view);
            }
            TurnUpdate::ToolExecutionUpdate {
                tool_call_id,
                partial,
            } => {
                // A `starting` partial (python-kernel bootstrap) owns the
                // loader note (TS `setWorkingMessage`); other updates leave
                // any current note alone.
                if let Some(message) = crate::snapshot::working_message_from_update(&partial) {
                    if let Some(working) = &mut view.working {
                        working.message = Some(message);
                    }
                }
                self.apply_tool_result(&tool_call_id, partial, false, true, view);
            }
            TurnUpdate::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
            } => {
                self.apply_tool_result(&tool_call_id, result, is_error, false, view);
                self.set_working_activity("Waiting", false, view);
                // The tool that owned the loader note finished executing
                // (TS clears `workingMessage` in the tool's `finally`).
                if let Some(working) = &mut view.working {
                    working.message = None;
                }
            }
            TurnUpdate::TurnEnded { error } => {
                // Only the engine's own turn_end clears the busy state:
                // trailing `agent_end` frames from the previous turn must
                // not cancel a turn admitted in between (prompt queueing).
                self.streaming_index = None;
                self.turn_active = false;
                view.working = None;
                view.working_since = None;
                view.retry = None;
                // A provider failure already surfaced through the failed
                // assistant message and/or the retry-exhausted banner; the
                // turn result error is only a silent-failure backstop.
                if let (Some(error), false) = (error, self.turn_error_shown) {
                    view.push_entry(ChatEntry::Status {
                        text: format!("turn failed: {error}"),
                        kind: StatusKind::Warning,
                    });
                }
            }
            TurnUpdate::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
                reason,
            } => {
                // The retry countdown loader replaces the working loader
                // until the loop settles (TS auto_retry_start). A backup
                // reason is a provider-failover switch: the loader names
                // the backup provider the turn re-routes to (no countdown;
                // the switch re-issues immediately).
                view.retry = Some(RetryState {
                    attempt,
                    max_attempts,
                    ends_at: std::time::Instant::now() + std::time::Duration::from_millis(delay_ms),
                    error_message,
                    reason,
                });
            }
            TurnUpdate::AutoRetryEnd {
                success: _,
                attempt,
                final_error,
                restored_model,
            } => {
                view.retry = None;
                if let Some(final_error) = final_error {
                    self.turn_error_shown = true;
                    view.push_entry(ChatEntry::Status {
                        text: format!(
                            "\u{26a0} Error: Retry failed after {attempt} attempts: {final_error}"
                        ),
                        kind: StatusKind::Error,
                    });
                }
                // A settled switch restores the primary provider (TS
                // `restoredModel` status line).
                if let Some(restored_model) = restored_model {
                    view.push_entry(ChatEntry::Status {
                        text: format!("Primary provider recovered — back on {restored_model}"),
                        kind: StatusKind::Info,
                    });
                }
            }
            TurnUpdate::CompactionStart {
                reason,
                custom_instructions,
            } => {
                // TS `startCompactionLoader`: the compaction loader fully
                // replaces the working loader for the run's duration.
                view.working = None;
                view.compaction = Some(CompactionState {
                    reason: CompactionReason::parse(&reason),
                    custom_instructions,
                });
            }
            TurnUpdate::CompactionEnd {
                reason,
                result,
                custom_instructions,
                aborted,
                error_message,
                error_severity,
            } => {
                view.compaction = None;
                if let Some(result) = result {
                    // A succeeded compaction rebuilt the durable transcript
                    // (it now starts at the compaction summary): show the
                    // outcome row immediately, then re-fetch the transcript
                    // so the compacted-away rows drop (TS
                    // `rebuildChatFromMessages`).
                    view.push_entry(ChatEntry::CompactionSummary {
                        summary: result
                            .get("summary")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        tokens_before: result
                            .get("tokensBefore")
                            .and_then(Value::as_u64)
                            .unwrap_or_default(),
                        custom_instructions,
                    });
                    self.transcript_stale = true;
                } else if reason == "manual" {
                    // TS `compaction_end` handling: aborts and error
                    // messages surface only for user-issued compactions,
                    // through `showError`/`showWarning` (whose rows carry
                    // the `⚠ Error: ` / `⚠ ` prefixes).
                    if aborted {
                        view.push_entry(ChatEntry::Status {
                            text: "\u{26a0} Error: Compaction cancelled".to_string(),
                            kind: StatusKind::Error,
                        });
                    } else if let Some(message) = error_message {
                        if error_severity.as_deref() == Some("warning") {
                            view.push_entry(ChatEntry::Status {
                                text: format!("\u{26a0} {message}"),
                                kind: StatusKind::Warning,
                            });
                        } else {
                            view.push_entry(ChatEntry::Status {
                                text: format!("\u{26a0} Error: {message}"),
                                kind: StatusKind::Error,
                            });
                        }
                    }
                }
            }
            TurnUpdate::Idle => {
                if !self.turn_active {
                    view.working = None;
                }
            }
            TurnUpdate::GoalUpdate(goal) => {
                self.apply_goal_update(goal, view);
            }
            TurnUpdate::StatusUpdate => {}
        }
        self.dirty = true;
    }

    /// Apply an assistant message frame: an open streaming message is
    /// updated in place; otherwise the message expands into a chat component
    /// plus a card per tool call.
    fn apply_assistant_message(
        &mut self,
        message: &Value,
        streaming: bool,
        stream_event: Option<&Value>,
        view: &mut AgentView,
    ) {
        if let Some(event) = stream_event {
            self.track_stream_activity(event, view);
        }
        let (blocks, tool_calls) = assistant_message_parts(message);
        // A message_start always opens a new streaming message (the engine
        // emits one per provider call); later frames update it in place.
        let starts_message = stream_event
            .and_then(|event| event.get("type"))
            .and_then(Value::as_str)
            == Some("start");
        let usage_output = message
            .get("usage")
            .and_then(|usage| usage.get("output"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if streaming {
            if starts_message {
                self.working_tokens.start_message();
            }
            let content_chars: u64 = blocks
                .iter()
                .map(|block| match block {
                    MessageBlock::Text(text) | MessageBlock::Thinking(text) => {
                        text.chars().count() as u64
                    }
                })
                .sum();
            let current = self
                .working_tokens
                .apply_streaming(usage_output, content_chars);
            if let Some(working) = &mut view.working {
                working.tokens = working.tokens.max(current);
            }
        } else {
            self.working_tokens.settle(usage_output);
        }
        if let Some(text) = blocks.iter().rev().find_map(|block| match block {
            MessageBlock::Text(text) => Some(text.clone()),
            _ => None,
        }) {
            self.last_assistant_text = Some(text);
        }
        let has_tool_calls = !tool_calls.is_empty();
        // A message_start always opens a new streaming message (the engine
        // emits one per provider call); later frames update it in place.
        if starts_message {
            self.streaming_index = None;
        }
        match self.streaming_index {
            Some(index) => {
                if let Some(ChatEntry::Assistant(open)) = view.chat.get_mut(index) {
                    open.blocks = blocks;
                    open.has_tool_calls = has_tool_calls;
                    open.streaming = streaming;
                }
                view.mark_entry_stale(index);
            }
            None => {
                if !blocks.is_empty() {
                    view.push_entry(ChatEntry::Assistant(Box::new(
                        crate::chat::AssistantMessage {
                            blocks,
                            has_tool_calls,
                            streaming,
                            error: None,
                            aborted: false,
                        },
                    )));
                    self.streaming_index = Some(view.chat.len() - 1);
                }
            }
        }
        for (id, name, args) in &tool_calls {
            // A streamed tool call first appears queued; the execution start
            // event flips it to running, and later frames refresh its name
            // and args while they stream (TS `updateArgs` + the latest
            // streaming call winning at component creation).
            crate::snapshot::apply_streamed_tool_card(view, id, name, args);
        }
        if !streaming {
            let open = self.streaming_index.take();
            self.finalize_assistant_error(message, &tool_calls, open, view);
        }
    }

    /// The final frame of a failed assistant message renders its error row
    /// (TS renders it inside the assistant component): `aborted` always
    /// shows, `error` only when the message carries no tool calls (the
    /// pending cards carry the failure then). The row renders inside the
    /// message's own component — the open streaming entry when one exists,
    /// otherwise a fresh entry: TS creates one component per assistant
    /// message (`message_start`), so a content-less provider failure stacks
    /// its own error row per failed attempt instead of decorating the
    /// previous reply (TS `AssistantMessageComponent.rebuild`).
    fn finalize_assistant_error(
        &mut self,
        message: &Value,
        tool_calls: &[(String, String, Value)],
        open: Option<usize>,
        view: &mut AgentView,
    ) {
        let Some(error) = crate::snapshot::assistant_error_row(message, tool_calls) else {
            return;
        };
        self.turn_error_shown = true;
        if let Some(index) = open {
            if let Some(ChatEntry::Assistant(entry)) = view.chat.get_mut(index) {
                entry.error = Some(error.text);
                entry.aborted = error.aborted;
                view.mark_entry_stale(index);
                return;
            }
        }
        // The message rendered no component (empty content): TS still
        // renders the message's own error component, so the failure stacks
        // as a separate row instead of attaching to the last assistant.
        view.push_entry(ChatEntry::Assistant(Box::new(
            crate::chat::AssistantMessage {
                blocks: Vec::new(),
                has_tool_calls: false,
                streaming: false,
                error: Some(error.text),
                aborted: error.aborted,
            },
        )));
    }

    /// Attach a (partial or final) tool result to the matching card.
    fn apply_tool_result(
        &mut self,
        tool_call_id: &str,
        result: Value,
        is_error: bool,
        partial: bool,
        view: &mut AgentView,
    ) {
        let result = ToolResultView {
            content: result
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            details: result.get("details").cloned().unwrap_or(Value::Null),
            is_error,
        };
        let card_index = view
            .chat
            .iter()
            .position(|entry| matches!(entry, ChatEntry::Tool(card) if card.id == tool_call_id));
        if let Some(index) = card_index {
            if let Some(ChatEntry::Tool(card)) = view.chat.get_mut(index) {
                card.result = Some(result);
                card.result_partial = partial;
                if !partial {
                    card.ended_at = Some(std::time::Instant::now());
                }
                view.mark_entry_stale(index);
            }
        }
    }

    /// Update the loader activity label (agent-activity tracker subset).
    fn set_working_activity(
        &mut self,
        activity: &'static str,
        download: bool,
        view: &mut AgentView,
    ) {
        if let Some(working) = &mut view.working {
            working.activity = activity;
            working.download = download;
        }
    }
}

/// Deterministic list order: most recently active first (missing activity
/// timestamps last), so `/switch <n>` targets are stable between `/list`
/// renders.
/// The terminal's current column count (TS `this.ui.terminal.columns` for
/// the goal status detail suffix); 80 when the size is unavailable.
fn terminal_columns() -> usize {
    crossterm::terminal::size()
        .map(|(columns, _)| columns as usize)
        .unwrap_or(80)
}

fn sorted_session_rows(mut sessions: Vec<Value>) -> Vec<Value> {
    let activity_of = |row: &Value| -> String {
        row.get("lastActivityAt")
            .or_else(|| row.get("modified"))
            .or_else(|| row.get("created"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    sessions.sort_by(|left, right| {
        let (left, right) = (activity_of(left), activity_of(right));
        if left.is_empty() && right.is_empty() {
            return std::cmp::Ordering::Equal;
        }
        if left.is_empty() {
            return std::cmp::Ordering::Greater;
        }
        if right.is_empty() {
            return std::cmp::Ordering::Less;
        }
        right.cmp(&left)
    });
    sessions
}

/// TS `formatResumeHint` (resume-hint.ts): the post-exit hint names how to
/// resume the session just left. Ephemeral (no session file) and unflushed
/// empty sessions are omitted — neither can be resumed. Persistence is
/// lazy: a file that does not exist on disk cannot be resumed either.
pub(crate) fn resume_hint_from_stats(stats: &Value) -> Option<String> {
    let session_id = stats.get("sessionId").and_then(Value::as_str)?;
    let session_file = stats.get("sessionFile").and_then(Value::as_str)?;
    let user_messages = stats
        .get("userMessages")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if session_file.is_empty() || user_messages == 0 || !std::path::Path::new(session_file).exists()
    {
        return None;
    }
    Some(format!(
        "Resume this session with: prime-agent --resume {session_id}"
    ))
}

/// Send a `create` command and return the new session's active id. A
/// non-empty selection picks the reopen form: `continueRecent` or an
/// explicit saved-session path.
/// The picker's viewport row budget (TS `showConfigurationMenu` passes
/// `min(20, rows - 3)` and `ConfigurationMenuComponent` subtracts one more
/// row for its hint).
fn picker_viewport_rows(terminal_rows: u16) -> usize {
    let terminal_rows = terminal_rows as usize;
    let menu_rows = 20.min(terminal_rows.saturating_sub(3).max(1));
    menu_rows.saturating_sub(1).max(1)
}

async fn create_session(
    client: &DaemonClient,
    options: &InteractiveOptions,
    selection: Option<&SessionSelection>,
) -> Result<String> {
    let continue_recent = matches!(selection, Some(SessionSelection::ContinueRecent));
    let session_path = match selection {
        Some(SessionSelection::Resume(path)) => Some(path.to_string_lossy().to_string()),
        _ => None,
    };
    let data = client
        .request_ok(DaemonCommand::Create {
            id: None,
            session_path,
            continue_recent: continue_recent.then_some(true),
            no_session: options.no_session.then_some(true),
            name: None,
            config: Some(options.create_config()),
            telemetry_disabled: options.telemetry_disabled.filter(|disabled| *disabled),
            runtime_metadata: None,
            lifecycle: None,
            env: None,
            launch_env: None,
            rest: Default::default(),
        })
        .await?;
    data.get("activeSessionId")
        .or_else(|| data.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the daemon did not report a session id for the new session"))
}

#[cfg(test)]
mod loader_token_tests {
    use super::LoaderTokenTracker;

    /// The live count derives from the streamed message, so coalesced
    /// frames (one latest-snapshot wire frame per flush tick) count the
    /// full streamed size — a per-delta sum would undercount them ~20x.
    #[test]
    fn coalesced_frames_count_from_the_message_not_deltas() {
        let mut tracker = LoaderTokenTracker::default();
        tracker.reset();
        // A message streams to 400 chars; the coalesced wire frame carries
        // the full snapshot but only the final provider delta.
        assert_eq!(tracker.apply_streaming(0, 400), 100);
        // A provider that reports usage upfront wins over the estimate.
        assert_eq!(tracker.apply_streaming(600, 400), 600);
        // Settle banks the reported usage, then the live state is empty.
        tracker.settle(600);
        assert_eq!(tracker.current(), 600);
    }

    /// Settling without reported usage banks the live estimate (TS
    /// `usage.output > 0 ? usage.output : estimatedStreamingTokens()`).
    #[test]
    fn settle_without_usage_banks_the_estimate() {
        let mut tracker = LoaderTokenTracker::default();
        tracker.reset();
        assert_eq!(tracker.apply_streaming(0, 404), 101);
        tracker.settle(0);
        assert_eq!(tracker.current(), 101);
    }

    /// A new message resets the live state but keeps the run's completed
    /// count; `agent_start` resets the whole tracker (TS `reset`).
    #[test]
    fn message_start_resets_the_live_state_and_agent_start_the_run() {
        let mut tracker = LoaderTokenTracker::default();
        tracker.reset();
        assert_eq!(tracker.apply_streaming(0, 800), 200);
        tracker.settle(0);
        tracker.start_message();
        // The live count rides on the run's completed count: 200 banked
        // plus the new message's reported 50.
        assert_eq!(tracker.apply_streaming(50, 8), 250);
        tracker.settle(50);
        assert_eq!(tracker.current(), 250);
        tracker.reset();
        assert_eq!(tracker.current(), 0);
    }
}
