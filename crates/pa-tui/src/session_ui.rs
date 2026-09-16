//! Live per-session UI state for the interactive loop: the daemon-client
//! side of one attached session — prompt submission, slash commands, streamed
//! event application, and session switching. Rendering itself lives in the
//! view crate modules; this module only decides what the view shows.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use pa_types::daemon::DaemonCommand;
use serde_json::Value;

use crate::daemon_client::{DaemonClient, DaemonClientEvent};
use crate::interactive::{InteractiveOptions, SessionSelection};
use crate::keys::key_event_to_id;
use crate::session::TranscriptItem;
use crate::snapshot::{attach_data_from_response, event_to_update, reconstruct, TurnUpdate};
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
    /// Snapshot transcript to fold into the view on the next rebuild.
    pending_snapshot: Option<Vec<TranscriptItem>>,
    /// Rows of the most recent `/list` (for `/switch <n>`).
    list_rows: Vec<Value>,
    pub(crate) turn_active: bool,
    streaming_index: Option<usize>,
    pub(crate) last_assistant_text: Option<String>,
    pub(crate) exit_requested: bool,
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
            pending_snapshot: None,
            list_rows: Vec::new(),
            turn_active: false,
            streaming_index: None,
            last_assistant_text: None,
            exit_requested: false,
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
    async fn attach_session(&mut self, active_session_id: &str) -> Result<()> {
        let previous = self.active_session_id.clone();
        if !previous.is_empty() && previous != active_session_id {
            let _ = self.detach().await;
        }
        let data = self
            .client
            .request_ok(DaemonCommand::Attach {
                id: None,
                active_session_id: active_session_id.to_string(),
                supports_extension_ui: None,
                client_id: None,
                capabilities: None,
                resume_cursor: None,
                telemetry_disabled: None,
                recovery_config: None,
                env: None,
                launch_env: None,
                rest: Default::default(),
            })
            .await?;
        let attach = attach_data_from_response(&data)?;
        let reconstructed = reconstruct(&attach);
        self.active_session_id = attach.active_session_id;
        self.session_id = reconstructed.session_id;
        self.session_name = reconstructed.session_name;
        self.last_assistant_text =
            reconstructed
                .transcript
                .iter()
                .rev()
                .find_map(|item| match item {
                    TranscriptItem::Assistant { text } => Some(text.clone()),
                    _ => None,
                });
        self.pending_snapshot = Some(reconstructed.transcript);
        self.turn_active = false;
        self.streaming_index = None;
        Ok(())
    }

    /// Fold the pending snapshot into the view (fresh transcript, footer
    /// labels). Called after attach and after every session switch.
    pub(crate) fn rebuild_view(&mut self, view: &mut AgentView) {
        view.transcript.clear();
        if let Some(items) = self.pending_snapshot.take() {
            for item in items {
                view.push(item);
            }
        }
        view.model_label = self.session_display();
        view.status = "idle".to_string();
        self.dirty = true;
    }

    fn session_display(&self) -> String {
        self.session_name
            .clone()
            .unwrap_or_else(|| self.active_session_id.clone())
    }

    pub(crate) fn note(&mut self, text: &str, view: &mut AgentView) {
        view.push(TranscriptItem::SystemNote {
            text: text.to_string(),
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
        view.status = "working".to_string();
        self.dirty = true;
        Ok(())
    }

    /// Slash commands: session management without a full command palette.
    async fn handle_slash(&mut self, text: &str, view: &mut AgentView) -> Result<()> {
        let mut parts = text.splitn(2, ' ');
        let command = parts.next().unwrap_or_default();
        let argument = parts.next().unwrap_or_default().trim();
        match command {
            "/help" => {
                self.note(
                    "/help           this list\n/list           live sessions\n/switch <n|id>  switch to a session from /list\n/new            start a new session\n/exit           detach and exit",
                    view,
                );
            }
            "/list" => {
                self.refresh_list(view).await?;
            }
            "/switch" => {
                if argument.is_empty() {
                    self.note("usage: /switch <n|id> (run /list first)", view);
                } else {
                    self.switch_to(argument, view).await?;
                }
            }
            "/new" => {
                let id = create_session(&self.client, &self.create_options(), None).await?;
                self.attach_session(&id).await?;
                self.rebuild_view(view);
                self.note(&format!("started session {id}"), view);
            }
            "/exit" => {
                self.exit_requested = true;
            }
            other => {
                self.note(&format!("unknown command: {other} (try /help)"), view);
            }
        }
        Ok(())
    }

    /// Options for `/new`: same socket, cwd, persistence, and script seam as
    /// the original run.
    fn create_options(&self) -> InteractiveOptions {
        InteractiveOptions {
            socket_path: self.client.socket_path().to_path_buf(),
            cwd: self.cwd.clone(),
            session_dir: self.session_dir.clone(),
            script_path: self.script_path.clone(),
            no_session: false,
            session: SessionSelection::New,
            initial_message: None,
            theme: String::new(),
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
                    view.status = "idle".to_string();
                    self.note(&format!("session closed ({reason})"), view);
                }
            }
            DaemonClientEvent::DaemonClosing { reason } => {
                self.note(&format!("the daemon is shutting down ({reason})"), view);
            }
            // Saved-session list frames belong to the session picker UI.
            DaemonClientEvent::SessionListItem { .. }
            | DaemonClientEvent::SessionListProgress { .. } => {}
        }
    }

    fn apply_update(&mut self, update: TurnUpdate, view: &mut AgentView) {
        match update {
            TurnUpdate::TurnStarted => {
                self.turn_active = true;
                view.status = "working".to_string();
            }
            TurnUpdate::UserMessage(text) => {
                view.push(TranscriptItem::UserMessage { text });
            }
            TurnUpdate::AssistantMessage { text, streaming } => {
                if !text.is_empty() {
                    self.last_assistant_text = Some(text.clone());
                }
                match self.streaming_index {
                    Some(index) => {
                        if let Some(TranscriptItem::Assistant { text: slot }) =
                            view.transcript.get_mut(index)
                        {
                            *slot = text;
                        }
                    }
                    None => {
                        view.transcript.push(TranscriptItem::Assistant { text });
                        self.streaming_index = Some(view.transcript.len() - 1);
                    }
                }
                if !streaming {
                    self.streaming_index = None;
                }
            }
            TurnUpdate::TurnEnded { error } => {
                // Only the engine's own turn_end clears the busy state:
                // trailing `agent_end` frames from the previous turn must
                // not cancel a turn admitted in between (prompt queueing).
                self.streaming_index = None;
                self.turn_active = false;
                view.status = "idle".to_string();
                if let Some(error) = error {
                    self.note(&format!("turn failed: {error}"), view);
                }
            }
            TurnUpdate::Idle => {
                if !self.turn_active {
                    view.status = "idle".to_string();
                }
            }
            TurnUpdate::StatusUpdate => {}
        }
        self.dirty = true;
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
