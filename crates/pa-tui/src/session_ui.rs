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
    ChatEntry, MessageBlock, RetryState, StatusKind, ToolCallCard, ToolResultView, WorkingState,
};
use crate::daemon_client::{DaemonClient, DaemonClientEvent};
use crate::interactive::{InteractiveOptions, ModelSelection, SessionSelection};
use crate::keys::key_event_to_id;
use crate::snapshot::{
    assistant_message_parts, attach_data_from_response, event_to_update, reconstruct, TurnUpdate,
};
use crate::view::AgentView;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Live UI state for one attached daemon session.
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
    /// Telemetry opt-out carried over from the run options; every attach to
    /// another session keeps carrying it (TS attach parity).
    telemetry_disabled: Option<bool>,
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
    /// Streaming token estimate for the loader (activity tracker).
    working_tokens: u64,
    /// The turn already surfaced its error (a failed assistant message or a
    /// retry-exhausted banner); the turn_end error stays silent then (TS
    /// renders the failure once, through the message or the retry banner).
    turn_error_shown: bool,
    pub(crate) last_assistant_text: Option<String>,
    pub(crate) exit_requested: bool,
    /// `/resume`: reopen the agents view after this session detaches.
    pub(crate) open_agents_view: bool,
    /// `/resume <selector>`: open this selection next (the run returns it).
    pub(crate) pending_selection: Option<SessionSelection>,
    /// `/mcp login` / `/mcp logout` (the composition root's auth flows).
    client_auth: Option<crate::client_auth::ClientAuthCommandsHandle>,
    pub(crate) dirty: bool,
}

impl SessionUi {
    /// Create/attach per the session selection and return the live state.
    pub(crate) async fn open(
        client: DaemonClient,
        options: &InteractiveOptions,
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
            telemetry_disabled: options.telemetry_disabled,
            pending_snapshot: None,
            pending_model: None,
            context: None,
            cost_usd: None,
            list_rows: Vec::new(),
            turn_active: false,
            streaming_index: None,
            working_tokens: 0,
            turn_error_shown: false,
            last_assistant_text: None,
            exit_requested: false,
            open_agents_view: false,
            pending_selection: None,
            client_auth: options.client_auth.clone(),
            dirty: true,
        };
        session
            .attach_session(&active_session_id)
            .await
            .with_context(|| format!("attaching session {active_session_id}"))?;
        Ok(session)
    }

    /// Detach the current session and attach `id`, rebuilding the transcript
    /// from the slim attach snapshot.
    ///
    /// The attach itself travels over a direct worker link when the
    /// supervisor issues a ticket (best effort: every failure keeps the
    /// supervisor-routed path, and a failed direct attach retries once over
    /// the supervisor).
    async fn attach_session(&mut self, active_session_id: &str) -> Result<()> {
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
        self.session_name = reconstructed.session_name;
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
        self.turn_active = false;
        self.streaming_index = None;
        Ok(())
    }

    /// Fold the pending snapshot into the view (fresh transcript, footer
    /// labels). Called after attach and after every session switch.
    pub(crate) fn rebuild_view(&mut self, view: &mut AgentView) {
        view.chat.clear();
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
        view.working = None;
        view.follow();
        self.dirty = true;
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
            .client
            .request_ok(DaemonCommand::GetSessionStats {
                id: None,
                active_session_id: self.active_session_id.clone(),
                rest: Default::default(),
            })
            .await
        else {
            return;
        };
        if let Some(usage) = data.get("contextUsage") {
            let tokens = usage.get("tokens").and_then(Value::as_u64);
            let window = usage.get("contextWindow").and_then(Value::as_u64);
            if let (Some(tokens), Some(window)) = (tokens, window) {
                self.context = Some(crate::chrome::ContextUsage {
                    tokens,
                    context_window: window,
                });
            }
        }
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
        view.push_entry(ChatEntry::Status {
            text: text.to_string(),
            kind: StatusKind::Info,
        });
        self.dirty = true;
    }

    pub(crate) async fn detach(&self) -> Result<()> {
        self.client
            .request_ok(DaemonCommand::Detach {
                id: None,
                active_session_id: Some(self.active_session_id.clone()),
                rest: Default::default(),
            })
            .await
            .map(|_| ())
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
        self.send_prompt(text, view).await
    }

    /// Send a prompt to the session and start the working loader. Session
    /// commands travel the same path — the session engine parses and
    /// executes them instead of admitting a model turn.
    async fn send_prompt(&mut self, text: &str, view: &mut AgentView) -> Result<()> {
        self.client
            .request_ok(DaemonCommand::Prompt {
                id: None,
                active_session_id: self.active_session_id.clone(),
                message: text.to_string(),
                input: pa_types::daemon::PromptInput {
                    content: None,
                    images: None,
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
            })
            .await
            .map_err(|error| anyhow!("{error:#}"))?;
        if !self.turn_active {
            self.turn_active = true;
        }
        self.start_loader(view);
        self.dirty = true;
        Ok(())
    }

    /// The working loader starts with a `Waiting` activity and a zero token
    /// count (stream events accumulate tokens and switch the label).
    fn start_loader(&mut self, view: &mut AgentView) {
        view.working = Some(WorkingState {
            activity: "Waiting",
            download: false,
            tokens: 0,
            elapsed_secs: 0,
        });
        view.working_since = Some(std::time::Instant::now());
        self.working_tokens = 0;
    }

    /// Update the loader from one provider stream event (the activity
    /// tracker: thinking/text/toolcall deltas switch the label and
    /// accumulate the token estimate at 4 chars per token).
    fn track_stream_activity(&mut self, event: &Value, view: &mut AgentView) {
        let (activity, download) = match event.get("type").and_then(Value::as_str) {
            Some("thinking_start") | Some("thinking_delta") => ("Thinking", true),
            Some("text_start") | Some("text_delta") => ("Writing", true),
            Some("toolcall_start") | Some("toolcall_delta") => ("Writing code", true),
            _ => return,
        };
        if let Some(delta) = event.get("delta").and_then(Value::as_str) {
            self.working_tokens += (delta.chars().count() as f64 / 4.0).round() as u64;
        }
        if let Some(working) = &mut view.working {
            working.activity = activity;
            working.download = download;
            working.tokens = self.working_tokens;
        }
    }

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
            // TS `handleMcpCommand`'s login/logout branches: the auth
            // flows run in the client process (the composition root's
            // hook); the other management subcommands surface through the
            // `mcp` CLI command instead of the TUI.
            "mcp" => self.handle_mcp_command(resolved, view).await?,
            other => {
                self.note(
                    &format!("/{other} is not available in this client yet"),
                    view,
                );
            }
        }
        Ok(())
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
            no_session: false,
            session: SessionSelection::New,
            initial_message: None,
            telemetry_disabled: self.telemetry_disabled,
            theme: String::new(),
            version: String::new(),
            onboarding: None,
            client_auth: self.client_auth.clone(),
        }
    }

    async fn refresh_list(&mut self, view: &mut AgentView) -> Result<()> {
        let data = self
            .client
            .request_ok(DaemonCommand::List {
                id: None,
                all: None,
                cwd: None,
                session_dir: None,
                include_client_owned: None,
                rest: Default::default(),
            })
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

    pub(crate) async fn handle_key(
        &mut self,
        key: KeyEvent,
        view: &mut AgentView,
        running: &mut bool,
    ) -> Result<()> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if view.editor.is_showing_autocomplete() {
                view.editor.cancel_autocomplete();
                return Ok(());
            }
            // Ctrl+C aborts an active turn, exits when idle (TS parity: the
            // first press cancels work, the shell exits on the second).
            if self.turn_active {
                let _ = self
                    .client
                    .request_ok(DaemonCommand::Abort {
                        id: None,
                        active_session_id: self.active_session_id.clone(),
                        rest: Default::default(),
                    })
                    .await;
                self.note("aborting the current turn", view);
                return Ok(());
            }
            *running = false;
            return Ok(());
        }
        if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
            *running = false;
            return Ok(());
        }
        if key.code == KeyCode::Esc {
            view.editor.cancel_autocomplete();
            return Ok(());
        }
        if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL) {
            // Ctrl+O cycles conversation detail (TS `app.tools.expand`):
            // overview -> details -> all -> overview.
            view.detail = view.detail.next();
            self.dirty = true;
            return Ok(());
        }
        let Some(id) = key_event_to_id(&key) else {
            return Ok(());
        };
        view.editor.handle_input(&id);
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
            DaemonClientEvent::DaemonClosing { reason } => {
                self.note(&format!("the daemon is shutting down ({reason})"), view);
            }
            // Saved-session list frames and roster pushes belong to the
            // agents-view UI; the session view only reads its own session.
            DaemonClientEvent::SessionListItem { .. }
            | DaemonClientEvent::SessionListProgress { .. }
            | DaemonClientEvent::RosterUpdate { .. } => {}
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
                self.apply_tool_start(&tool_call_id, &tool_name, args, view);
                self.set_working_activity("Executing", false, view);
            }
            TurnUpdate::ToolExecutionUpdate {
                tool_call_id,
                partial,
            } => {
                self.apply_tool_result(&tool_call_id, partial, false, true, view);
            }
            TurnUpdate::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
            } => {
                self.apply_tool_result(&tool_call_id, result, is_error, false, view);
                self.set_working_activity("Waiting", false, view);
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
            } => {
                // The retry countdown loader replaces the working loader
                // until the loop settles (TS auto_retry_start).
                view.retry = Some(RetryState {
                    attempt,
                    max_attempts,
                    ends_at: std::time::Instant::now() + std::time::Duration::from_millis(delay_ms),
                });
            }
            TurnUpdate::AutoRetryEnd {
                success: _,
                attempt,
                final_error,
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
            }
            TurnUpdate::Idle => {
                if !self.turn_active {
                    view.working = None;
                }
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
        if let Some(text) = blocks.iter().rev().find_map(|block| match block {
            MessageBlock::Text(text) => Some(text.clone()),
            _ => None,
        }) {
            self.last_assistant_text = Some(text);
        }
        let has_tool_calls = !tool_calls.is_empty();
        // A message_start always opens a new streaming message (the engine
        // emits one per provider call); later frames update it in place.
        let starts_message = matches!(
            stream_event.and_then(|event| event.get("type").and_then(Value::as_str)),
            Some("start")
        );
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
            // event flips it to running, and later frames refresh its args
            // while they stream (TS `updateArgs`).
            match view
                .chat
                .iter_mut()
                .find(|entry| matches!(entry, ChatEntry::Tool(card) if card.id == *id))
            {
                Some(ChatEntry::Tool(card)) => card.args = args.clone(),
                _ => view.push_entry(ChatEntry::Tool(Box::new(ToolCallCard {
                    id: id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                    started: false,
                    ..Default::default()
                }))),
            }
        }
        if !streaming {
            self.streaming_index = None;
            self.attach_assistant_error(message, &tool_calls, view);
        }
    }

    /// The final frame of a failed assistant message attaches its error
    /// row (TS renders it inside the assistant component): `aborted` always
    /// shows, `error` only when the message carries no tool calls (the
    /// pending cards carry the failure then).
    fn attach_assistant_error(
        &mut self,
        message: &Value,
        tool_calls: &[(String, String, Value)],
        view: &mut AgentView,
    ) {
        let stop_reason = message.get("stopReason").and_then(Value::as_str);
        let (text, aborted) = match stop_reason {
            Some("aborted") => {
                let error = message
                    .get("errorMessage")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty() && *text != "Request was aborted")
                    .unwrap_or("Operation aborted");
                (error.to_string(), true)
            }
            Some("error") if tool_calls.is_empty() => {
                let error = message
                    .get("errorMessage")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .unwrap_or("Unknown error");
                (format!("Error: {error}"), false)
            }
            _ => return,
        };
        self.turn_error_shown = true;
        // The message frame rendered before this call: attach the error to
        // the most recent assistant entry (its own final frame).
        if let Some(index) = self.streaming_index {
            if let Some(ChatEntry::Assistant(open)) = view.chat.get_mut(index) {
                open.error = Some(text);
                open.aborted = aborted;
                return;
            }
        }
        let last_assistant = view.chat.iter_mut().rev().find_map(|entry| match entry {
            ChatEntry::Assistant(message) => Some(message),
            _ => None,
        });
        if let Some(last) = last_assistant {
            last.error = Some(text);
            last.aborted = aborted;
        }
    }

    /// `tool_execution_start`: mark the matching card running (or create it
    /// when the message frame has not arrived yet).
    fn apply_tool_start(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        args: Value,
        view: &mut AgentView,
    ) {
        for entry in &mut view.chat {
            if let ChatEntry::Tool(card) = entry {
                if card.id == tool_call_id {
                    card.started = true;
                    card.started_at = Some(std::time::Instant::now());
                    if !args.is_null() {
                        card.args = args;
                    }
                    return;
                }
            }
        }
        view.push_entry(ChatEntry::Tool(Box::new(ToolCallCard {
            id: tool_call_id.to_string(),
            name: tool_name.to_string(),
            args,
            started: true,
            started_at: Some(std::time::Instant::now()),
            ..Default::default()
        })));
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
        for entry in &mut view.chat {
            if let ChatEntry::Tool(card) = entry {
                if card.id == tool_call_id {
                    card.result = Some(result);
                    card.result_partial = partial;
                    if !partial {
                        card.ended_at = Some(std::time::Instant::now());
                    }
                    return;
                }
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

/// Send a `create` command and return the new session's active id. A
/// non-empty selection picks the reopen form: `continueRecent` or an
/// explicit saved-session path.
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
