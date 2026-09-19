//! The agents view: the unified live-roster + saved-catalog session list
//! (TS `AgentsViewMode`). Rows group into Running/Idle/Inactive sections,
//! the inline prompt doubles as search, and the first actions are open
//! (attach a live session) and resume (reopen a saved file); `n` starts a
//! new session. Roster pushes arrive live over `roster_subscribe`; the
//! saved catalog loads once on open (TS parity: it feeds the Inactive
//! section). The reply composer, rename, delete, and kill-subagent actions
//! wait on the Stage-3 reply machinery.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use pa_types::daemon::DaemonCommand;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::agents_view_state::truncate_text;
use crate::agents_view_state::{
    build_layout, build_rows, filter_empty_sessions, filter_unified_sessions, parse_search_query,
    reconcile_unified_sessions, scope_depth, scope_to_descendants, section_title, AgentsViewRow,
    RowLayout, Section,
};

/// The scope a scoped view opened on (TS `AgentsViewScopeKey` plus the
/// display name): the view lists this session's descendants and the back
/// key returns to it.
pub use crate::agents_view_state::AgentsViewScope;
use crate::daemon_client::{DaemonClient, DaemonClientEvent};
use crate::interactive::SessionSelection;
use crate::theme::{Theme, ThemeBg, ThemeColor};
use crate::width::{pad_line, str_width};
use crate::Line;

/// Options for one agents-view run.
#[derive(Debug, Clone)]
pub struct AgentsViewOptions {
    pub socket_path: PathBuf,
    pub cwd: PathBuf,
    pub session_dir: Option<PathBuf>,
    pub theme: String,
    pub version: String,
    /// The session the view was opened from: keeps its recency slot and
    /// survives the empty-catalog filter.
    pub anchor_session_id: Option<String>,
    /// Open scoped to one session's subtree (the subagent summary line's
    /// open action; TS `scoped_agents_view`): the root lists its
    /// descendants, and the back key reopens this session.
    pub scope: Option<AgentsViewScope>,
    /// The query restored from the previous view run (TS
    /// `AgentsViewPersistentState.query`: returning from an opened chat
    /// keeps the filter typed before opening it).
    pub query: Option<String>,
}

/// How the view is driven.
pub enum AgentsViewUiMode {
    Terminal,
    /// Headless plan: typed input plus settle barriers, with rendered
    /// frames captured for the parity verifier.
    Headless(AgentsHeadlessPlan),
}

#[derive(Debug, Clone)]
pub struct AgentsHeadlessPlan {
    pub steps: Vec<AgentsStep>,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone)]
pub enum AgentsStep {
    /// Type into the search box, character by character.
    Type(String),
    /// One raw key id (e.g. "down", "enter", "ctrl+c").
    Key(String),
    /// Hold until the roster settles (or the deadline passes).
    WaitSettle { timeout_ms: u64 },
}

/// The result of one agents-view run.
#[derive(Debug, Default)]
pub struct AgentsViewOutcome {
    /// The session the user opened; `None` when the flow exits here.
    pub selection: Option<SessionSelection>,
    pub frames: Vec<String>,
    /// The query typed in this run, for the caller to restore on re-entry
    /// (TS `AgentsViewPersistentState.query`).
    pub query: Option<String>,
}

/// TS `WORKING_ICON_INTERVAL_MS`: the running-row icon frame cadence.
const PULSE_INTERVAL_MS: u64 = 250;

enum UiInput {
    Key(String),
    Settled,
    Done,
}

/// The agents view state: roster + catalog data, search, selection, and
/// the pending exit/open requests.
struct AgentsViewMode {
    options: AgentsViewOptions,
    theme: Theme,
    roster: Vec<Value>,
    saved: Vec<Value>,
    rows: Vec<AgentsViewRow>,
    selected: usize,
    query: String,
    status: Option<String>,
    /// The scope root's `depth` metadata (`rlmDepth + 1`); `None` when the
    /// scope root is not on the roster (the view falls back to the global
    /// list with a status message, TS scope-resolution fallback).
    scope_depth: Option<u32>,
    /// The scope root resolved on the last rebuild.
    scope_active: bool,
    /// First ctrl+c shows the exit hint; the second exits.
    exit_armed: bool,
    /// The double-Ctrl+C force-quit guard (the run's shared instance is
    /// installed by `run_agents_view` after `new`).
    exit_guard: crate::exit_guard::ExitGuard,
    pulse: usize,
    running: bool,
    selection: Option<SessionSelection>,
}

impl AgentsViewMode {
    fn new(options: AgentsViewOptions) -> Self {
        let theme = crate::app::load_theme(&options.theme);
        let query = options.query.clone().unwrap_or_default();
        AgentsViewMode {
            options,
            theme,
            roster: Vec::new(),
            saved: Vec::new(),
            rows: Vec::new(),
            selected: 0,
            query,
            status: None,
            scope_depth: None,
            scope_active: false,
            exit_armed: false,
            exit_guard: crate::exit_guard::ExitGuard::new(),
            pulse: 0,
            running: true,
            selection: None,
        }
    }

    /// Rebuild rows from the current roster, catalog, and query. A scoped
    /// run lists only the scope root's descendants (TS `scopeToSessionSubtree`
    /// with the root's own row excluded); a scope root that left the roster
    /// falls back to the global list with a status message.
    fn rebuild_rows(&mut self) {
        let identity = self.rows.get(self.selected).map(|row| row.identity.clone());
        let records = reconcile_unified_sessions(&self.roster, &self.saved);
        let mut scope_active = false;
        let filtered = match &self.options.scope {
            Some(scope) => {
                let empty = filter_empty_sessions(&records, None);
                match scope_to_descendants(&empty, scope) {
                    Some(scoped) => {
                        scope_active = true;
                        self.scope_depth = scope_depth(&empty, scope);
                        scoped
                    }
                    None => {
                        self.scope_depth = None;
                        self.status = Some(
                            "Scope is no longer available; returned to the global view".to_string(),
                        );
                        filter_empty_sessions(&records, self.options.anchor_session_id.as_deref())
                    }
                }
            }
            None => filter_empty_sessions(&records, self.options.anchor_session_id.as_deref()),
        };
        self.scope_active = scope_active;
        let query = self.query.trim();
        let rows = if query.is_empty() {
            build_rows(&filtered, self.options.anchor_session_id.as_deref())
        } else {
            let parsed = parse_search_query(query);
            let matching = filter_unified_sessions(&filtered, &parsed);
            build_rows(&matching, self.options.anchor_session_id.as_deref())
        };
        if let Some(identity) = identity {
            if let Some(index) = rows.iter().position(|row| row.identity == identity) {
                self.selected = index;
            } else {
                self.selected = 0;
            }
        }
        self.rows = rows;
    }

    /// Apply one roster push (`changed` upserts, `removed` deletes,
    /// `resync` replaces the whole roster).
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
        self.rebuild_rows();
    }

    /// Move the selection by `delta` rows.
    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            self.selected = 0;
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, self.rows.len() as isize - 1) as usize;
    }

    /// Open the selected row: attach a live session, or reopen the saved
    /// file. `n` requests a fresh session.
    fn open_selected(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let summary = &row.summary;
        if let Some(active) = summary
            .get("activeSessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            self.selection = Some(SessionSelection::Attach(active.to_string()));
            self.running = false;
            return;
        }
        if let Some(file) = summary
            .get("sessionFile")
            .and_then(Value::as_str)
            .filter(|file| !file.is_empty())
        {
            self.selection = Some(SessionSelection::Resume(PathBuf::from(file)));
            self.running = false;
            return;
        }
        self.status =
            Some("Cannot open agent without an active runtime or saved session file".to_string());
    }

    /// Hand the terminal back to the scope root's session (TS
    /// `finish({ type: "open", summary: backSession })`): the scoped view
    /// detaches and reattaches the session it was opened from.
    fn open_scope_root(&mut self) {
        let Some(scope) = self.options.scope.clone() else {
            return;
        };
        if let Some(active) = scope.active_session_id.clone().filter(|id| !id.is_empty()) {
            self.selection = Some(SessionSelection::Attach(active));
            self.running = false;
        }
    }

    /// Handle one key id. Returns the status-line override when the caller
    /// should surface one (none of the PR-2 actions do).
    fn handle_key(&mut self, key: &str) {
        self.exit_armed = false;
        match key {
            "up" => self.move_selection(-1),
            "down" => self.move_selection(1),
            "pageUp" => self.move_selection(-(self.rows.len() as isize).min(10)),
            "pageDown" => self.move_selection((self.rows.len() as isize).min(10)),
            // TS `app.agents.open` (right) and the editor submit (enter)
            // both open the selection; search text is never a prompt.
            "enter" | "right" => self.open_selected(),
            // TS `app.agents.new`: ctrl+n starts a session; a plain "n" is
            // search text like any other character.
            "ctrl+n" => {
                self.selection = Some(SessionSelection::New);
                self.running = false;
            }
            // The scoped view's parent key (TS `app.agents.back`): left
            // hands the terminal back to the scope root's session; the
            // global view has no hierarchy parent and consumes left
            // without opening a chat (TS onEscape handles escape alone).
            "left" => {
                if self.scope_active {
                    self.open_scope_root();
                }
            }
            "escape" => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.rebuild_rows();
                } else if self.scope_active {
                    self.open_scope_root();
                } else {
                    self.running = false;
                }
            }
            "ctrl+c" => {
                // One handled Ctrl+C press: the force-quit guard disarms
                // once the whole observed pair was handled without an
                // exit (this press armed the state); an exit re-arms from
                // the run loop's break.
                self.exit_guard.note_ctrl_c_handled();
                if self.exit_armed {
                    self.running = false;
                } else {
                    self.exit_armed = true;
                }
            }
            "backspace" => {
                self.query.pop();
                self.rebuild_rows();
            }
            "ctrl+u" => {
                self.query.clear();
                self.rebuild_rows();
            }
            other if other.chars().count() == 1 => {
                self.query.push_str(other);
                self.rebuild_rows();
            }
            _ => {}
        }
    }

    /// Compose one frame (splash, search prompt, sectioned list, hints).
    fn render_frame(&mut self, width: usize, height: usize) -> (Vec<Line>, Option<(usize, usize)>) {
        let theme = &self.theme;
        let mut lines: Vec<Line> = Vec::new();
        // TS `getAgentCountsText` rides the splash as extra metadata.
        let (running, idle, inactive) = (
            self.rows
                .iter()
                .filter(|row| row.section == Section::Running)
                .count(),
            self.rows
                .iter()
                .filter(|row| row.section == Section::Idle)
                .count(),
            self.rows
                .iter()
                .filter(|row| row.section == Section::Inactive)
                .count(),
        );
        let mut extra_metadata = vec![(
            "agents".to_string(),
            format!("{running} running, {idle} idle, {inactive} inactive"),
        )];
        if let Some(depth) = self.scope_depth {
            extra_metadata.push(("depth".to_string(), depth.to_string()));
        }
        let chrome = crate::chrome::ChromeState {
            version: self.options.version.clone(),
            cwd: self.options.cwd.to_string_lossy().to_string(),
            extra_metadata,
            splash_hide_cwd: self.scope_active,
            ..Default::default()
        };
        // `render_splash` already trails one blank row (TS renderContent's
        // `headerLines.push("")`).
        lines.extend(crate::chrome::render_splash(&chrome, theme, width));
        // The scoped view's back label (TS `<back> back · <title> ›
        // subagents`), dim, over the full width under the splash.
        if self.scope_active {
            if let Some(scope) = &self.options.scope {
                let title = scope
                    .session_name
                    .clone()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| "Untitled agent".to_string());
                let label = truncate_text(
                    &format!("\u{2190} back \u{b7} {title} \u{203a} subagents"),
                    width,
                );
                let mut row = vec![crate::Span::styled(label, theme.fg_style(ThemeColor::Dim))];
                row = crate::width::pad_line(row, width);
                lines.push(row);
                lines.push(vec![]);
            }
        }

        // Inline search prompt (TS renders the transparent editor with the
        // `> ` prefix, paddingX 2, and the dim "Search sessions" placeholder).
        let mut prompt: Line = vec![crate::Span::styled(
            " >  ".to_string(),
            theme.fg_style(ThemeColor::Muted),
        )];
        let head = truncate_text(&self.query, width.saturating_sub(5).max(1));
        prompt.push(crate::Span::styled(head, theme.fg_style(ThemeColor::Muted)));
        if self.query.is_empty() {
            prompt.push(crate::Span::styled(
                " ".to_string(),
                theme.fg_style(ThemeColor::Muted),
            ));
            prompt.push(crate::Span::styled(
                "Search sessions".to_string(),
                theme.fg_style(ThemeColor::Dim),
            ));
        }
        let cursor = Some((
            lines.len(),
            4 + str_width(&self.query).min(width.saturating_sub(4)),
        ));
        lines.push(prompt);
        lines.push(vec![]);

        let list_rows = height.saturating_sub(lines.len() + 1);
        lines.extend(self.render_list(width, list_rows));
        while lines.len() < height.saturating_sub(1) {
            lines.push(vec![]);
        }
        lines.push(self.render_hints(width));
        while lines.len() > height {
            lines.pop();
        }
        (lines, cursor)
    }

    /// The sectioned session list (legend header, section headings, rows).
    fn render_list(&mut self, width: usize, max_rows: usize) -> Vec<Line> {
        if max_rows == 0 {
            return Vec::new();
        }
        if self.rows.is_empty() {
            let text = if self.query.trim().is_empty() {
                "No sessions yet."
            } else {
                "No sessions match your search."
            };
            return vec![vec![self.theme.fg(ThemeColor::Dim, text.to_string())]];
        }
        let layout = build_layout(&self.rows, width);
        let mut lines: Vec<Line> = vec![
            vec![crate::Span::styled(
                layout.legend.clone(),
                self.theme
                    .fg_style(ThemeColor::Text)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            )],
            vec![],
        ];
        for section in [Section::Running, Section::Idle, Section::Inactive] {
            let rows: Vec<&AgentsViewRow> = self
                .rows
                .iter()
                .filter(|row| row.section == section)
                .collect();
            if rows.is_empty() {
                continue;
            }
            if lines.len() > 2 {
                lines.push(vec![]);
            }
            lines.push(vec![self.theme.fg(
                ThemeColor::Muted,
                format!("{} ({})", section_title(section), rows.len()),
            )]);
            for row in rows {
                lines.push(self.render_row(row, &layout, width));
            }
        }
        while lines.len() > max_rows {
            lines.pop();
        }
        lines
    }

    /// One session row: icon, title, model, activity, cost/age; the
    /// selected row carries the selection background.
    fn render_row(&self, row: &AgentsViewRow, layout: &RowLayout, width: usize) -> Line {
        let theme = &self.theme;
        let selected = Some(row.identity.as_str())
            == self.rows.get(self.selected).map(|r| r.identity.as_str());
        let icon = match row.section {
            Section::Running => ["\u{25c7}", "\u{25c8}", "\u{25c6}", "\u{25c8}"][self.pulse % 4],
            _ => "\u{2022}",
        };
        let icon_color = match row.section {
            Section::Running => ThemeColor::Text,
            Section::Idle => ThemeColor::Warning,
            Section::Inactive => ThemeColor::Dim,
        };
        let icon_style = theme
            .fg_style(icon_color)
            .add_modifier(ratatui::style::Modifier::BOLD);
        // TS `renderRow`: `icon title` padded to the name column, then the
        // model and activity cells, then the dim cost/age details.
        let named = row
            .summary
            .get("sessionName")
            .and_then(Value::as_str)
            .map(|name| !name.trim().is_empty())
            .unwrap_or(false);
        let mut line: Line = Vec::new();
        line.push(crate::Span::styled(icon, icon_style));
        line.push(crate::Span::styled(
            " ".to_string(),
            ratatui::style::Style::default(),
        ));
        // TS `formatTableCell(title, nameWidth)`: the name cell (icon +
        // title) clips to the column width, so a long session name can
        // never push the model, activity, and cost/age columns off-screen.
        // The icon and its space take the first two cells of the column.
        let title = truncate_text(&row.title, layout.name_width.saturating_sub(2));
        line.push(crate::Span::styled(
            title.clone(),
            if named {
                theme
                    .fg_style(ThemeColor::Text)
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                theme.fg_style(ThemeColor::Text)
            },
        ));
        line.push(crate::Span::styled(
            " ".repeat(layout.name_width.saturating_sub(str_width(&title) + 2)),
            ratatui::style::Style::default(),
        ));
        line.push(crate::Span::styled(
            "  ".to_string(),
            ratatui::style::Style::default(),
        ));
        line.push(theme.fg(ThemeColor::Muted, cell(&row.model, layout.model_width)));
        if layout.activity_width > 0 {
            line.push(crate::Span::styled(
                "  ".to_string(),
                ratatui::style::Style::default(),
            ));
            line.push(theme.fg(ThemeColor::Dim, cell(&row.activity, layout.activity_width)));
        }
        line.push(crate::Span::styled(
            "  ".to_string(),
            ratatui::style::Style::default(),
        ));
        let details = layout
            .details
            .get(&row.identity)
            .cloned()
            .unwrap_or_default();
        line.push(theme.fg(ThemeColor::Dim, details));
        if selected {
            line = pad_line(line, width);
            return theme.bg_paint(ThemeBg::SelectedBg, line);
        }
        line
    }

    /// The bottom hint/status line.
    fn render_hints(&self, width: usize) -> Line {
        let theme = &self.theme;
        if self.exit_armed {
            return truncate_line(
                vec![theme.fg(ThemeColor::Muted, "Press ctrl+c again to exit")],
                width,
            );
        }
        if let Some(status) = &self.status {
            return truncate_line(vec![theme.fg(ThemeColor::Error, status.clone())], width);
        }
        // TS keyText glyphs: up/down render as arrows, right as →, ctrl+n as
        // Ctrl+N; the scoped view adds the parent-back hint.
        let hints = if self.scope_active {
            "\u{2191}/\u{2193} navigate   Enter/\u{2192} open   \u{2190} parent   Ctrl+N new"
        } else {
            "\u{2191}/\u{2193} navigate   Enter/\u{2192} open   Ctrl+N new"
        };
        truncate_line(vec![theme.fg(ThemeColor::Muted, hints.to_string())], width)
    }
}

fn cell(value: &str, width: usize) -> String {
    let truncated = truncate_text(value, width);
    format!(
        "{truncated}{}",
        " ".repeat(width.saturating_sub(str_width(&truncated)))
    )
}

fn truncate_line(line: Line, width: usize) -> Line {
    let text = line.iter().map(|s| s.content.as_str()).collect::<String>();
    crate::width::wrap_text(&text, width.max(1))
        .into_iter()
        .next()
        .unwrap_or_default()
}

enum Renderer {
    Terminal(ratatui::Terminal<crate::hyperlinks::LinkBackend>),
    Headless {
        width: u16,
        height: u16,
        frames: Vec<String>,
    },
}

impl Renderer {
    fn setup(
        ui: AgentsViewUiMode,
        ui_tx: mpsc::UnboundedSender<UiInput>,
        exit_guard: crate::exit_guard::ExitGuard,
    ) -> Result<Renderer> {
        match ui {
            AgentsViewUiMode::Terminal => {
                crossterm::terminal::enable_raw_mode()?;
                // Adopt the alternate screen the previous surface left in
                // place (TS `pendingAltScreenHandoff`); only the first
                // surface of the process enters it, so a view switch never
                // flashes the primary screen.
                crate::altscreen::enter()?;
                // One reader thread feeds the view; the reader registry
                // joins the previous surface's reader (the chat it opened)
                // before this one starts polling. The reader also observes
                // Ctrl+C pairs for the exit guard: this thread stays alive
                // when the view loop is wedged in a daemon request, so the
                // force-quit contract holds regardless of loop state.
                crate::input::spawn_terminal_reader(move |event| match event {
                    crossterm::event::Event::Key(key) => {
                        exit_guard.observe_key(&key);
                        let id = crate::keys::key_event_to_id(&key).unwrap_or_default();
                        ui_tx.send(UiInput::Key(id)).is_ok()
                    }
                    _ => true,
                });
                let mut terminal = ratatui::Terminal::new(crate::hyperlinks::stdout_backend())?;
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
            AgentsViewUiMode::Headless(plan) => {
                let steps = plan.steps;
                tokio::spawn(async move {
                    for step in steps {
                        match step {
                            AgentsStep::Type(text) => {
                                for ch in text.chars() {
                                    if ui_tx.send(UiInput::Key(ch.to_string())).is_err() {
                                        return;
                                    }
                                }
                            }
                            AgentsStep::Key(key) => {
                                if ui_tx.send(UiInput::Key(key)).is_err() {
                                    return;
                                }
                            }
                            AgentsStep::WaitSettle { timeout_ms } => {
                                let _ = ui_tx.send(UiInput::Settled);
                                tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
                            }
                        }
                    }
                    let _ = ui_tx.send(UiInput::Done);
                });
                Ok(Renderer::Headless {
                    width: plan.width,
                    height: plan.height,
                    frames: Vec::new(),
                })
            }
        }
    }

    fn draw(&mut self, mode: &mut AgentsViewMode) -> Option<(usize, usize)> {
        match self {
            Renderer::Terminal(terminal) => {
                let area = terminal.size().expect("terminal size");
                let (lines, cursor) = mode.render_frame(area.width as usize, area.height as usize);
                crate::hyperlinks::install_frame(&lines);
                terminal
                    .draw(|f| {
                        let area = ratatui::layout::Rect::new(0, 0, area.width, area.height);
                        let rendered: Vec<ratatui::text::Line<'static>> =
                            lines.iter().map(crate::markdown::to_ratatui_line).collect();
                        f.render_widget(ratatui::text::Text::from(rendered), area);
                        if let Some((row, col)) = cursor {
                            if row < area.height as usize && col < area.width as usize {
                                f.set_cursor_position(ratatui::layout::Position::new(
                                    col as u16, row as u16,
                                ));
                            }
                        }
                    })
                    .expect("draw frame");
                None
            }
            Renderer::Headless {
                width,
                height,
                frames,
            } => {
                let (lines, _) = mode.render_frame(*width as usize, *height as usize);
                let text = lines
                    .iter()
                    .map(|line| line.iter().map(|s| s.content.as_str()).collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n");
                if frames.last().map(String::as_str) != Some(text.as_str()) {
                    frames.push(text);
                }
                None
            }
        }
    }

    /// Teardown. `preserve_alt_screen` mirrors TS `ui.stop({ preserveAltScreen })`:
    /// a handoff to the chat the view just selected keeps the alternate screen
    /// (and raw mode, so the handoff gap cannot echo into the preserved frame)
    /// for the adopting surface, hiding the cursor; a real exit releases the
    /// screen and restores the terminal. `flushFullscreen` stays false either
    /// way (TS agents-view-mode `finish`): the picker frame is never flushed
    /// onto the main screen.
    fn finish(self, preserve_alt_screen: bool) -> Vec<String> {
        match self {
            Renderer::Terminal(_) => {
                if preserve_alt_screen {
                    let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Hide);
                } else {
                    let _ = crossterm::terminal::disable_raw_mode();
                    let _ = crate::altscreen::leave();
                    let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
                }
                Vec::new()
            }
            Renderer::Headless { frames, .. } => frames,
        }
    }
}

/// Run the agents view until the user exits or opens a session.
pub async fn run_agents_view(
    options: AgentsViewOptions,
    ui: AgentsViewUiMode,
) -> Result<AgentsViewOutcome> {
    crossterm::style::force_color_output(true);
    let (client, mut events) = DaemonClient::connect(&options.socket_path)
        .await
        .with_context(|| "the agents view could not attach to the daemon")?;

    // The double-Ctrl+C force-quit guard: same contract as the session
    // loop (see `interactive::run_interactive`).
    let exit_guard = crate::exit_guard::ExitGuard::new();
    let mut mode = AgentsViewMode::new(options.clone());
    mode.exit_guard = exit_guard.clone();

    // The roster snapshot precedes streaming pushes; updates that race the
    // snapshot apply on top (idempotent by agent id, TS roster-store).
    let snapshot = client
        .request(DaemonCommand::RosterSubscribe {
            id: None,
            rest: Default::default(),
        })
        .await?;
    if !snapshot.success {
        client.close();
        anyhow::bail!(
            "roster_subscribe failed: {}",
            snapshot.error.unwrap_or_default()
        );
    }
    mode.roster = snapshot
        .data
        .as_ref()
        .and_then(|data| data.get("roster"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // The saved catalog feeds the Inactive section (cwd + sessionDir scope).
    let saved = client
        .request(DaemonCommand::ListSavedSessions {
            id: None,
            cwd: Some(mode.options.cwd.to_string_lossy().to_string()),
            session_dir: mode
                .options
                .session_dir
                .as_ref()
                .map(|dir| dir.to_string_lossy().to_string()),
            active_session_id: None,
            scope: Value::Null,
            rest: Default::default(),
        })
        .await?;
    if saved.success {
        mode.saved = saved
            .data
            .as_ref()
            .and_then(|data| data.get("sessions"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
    } else if let Some(error) = saved.error {
        mode.status = Some(format!("Saved sessions unavailable: {error}"));
    }
    mode.rebuild_rows();

    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiInput>();
    let mut renderer = Renderer::setup(ui, ui_tx, exit_guard.clone())?;
    let mut pending: Vec<UiInput> = Vec::new();
    let mut last_pulse = tokio::time::Instant::now();

    while mode.running {
        if let Some(input) = first_input(&mut pending) {
            match input {
                UiInput::Key(key) => mode.handle_key(&key),
                UiInput::Settled | UiInput::Done => {}
            }
            if let Renderer::Terminal(_) = renderer {
                renderer.draw(&mut mode);
            }
        }
        if !mode.running {
            break;
        }
        tokio::select! {
            maybe_event = events.recv() => {
                match maybe_event {
                    Some(DaemonClientEvent::RosterUpdate { changed, removed, resync }) => {
                        mode.apply_roster_update(changed, removed, resync);
                    }
                    Some(_) => {}
                    None => {
                        mode.status = Some("the daemon connection closed".to_string());
                        mode.running = false;
                    }
                }
            }
            maybe_input = ui_rx.recv() => {
                if let Some(input) = maybe_input {
                    pending.push(input);
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        // The running-row icon animates at the TS working-icon cadence.
        if mode.rows.iter().any(|row| row.section == Section::Running)
            && last_pulse.elapsed() >= Duration::from_millis(PULSE_INTERVAL_MS)
        {
            last_pulse = tokio::time::Instant::now();
            mode.pulse = mode.pulse.wrapping_add(1);
        }
        renderer.draw(&mut mode);
    }

    // The view decided to leave: arm the force-quit deadline so the
    // teardown below (terminal restore, roster unsubscribe over a possibly
    // dead daemon) is best-effort and cannot hold the process open.
    if matches!(renderer, Renderer::Terminal(_)) {
        exit_guard.arm_for_exit();
    }
    // A selection hands the pane to the chat it opened (TS `result.type !== "exit"`);
    // exiting releases the alternate screen.
    let frames = renderer.finish(mode.selection.is_some());
    let _ = client
        .request(DaemonCommand::RosterUnsubscribe {
            id: None,
            rest: Default::default(),
        })
        .await;
    client.close();
    // A selection hands the terminal to a session run: the process keeps
    // going, so retire the watchdog. A selection-less exit ends the
    // process, where the deadline dies with it — or fires if it wedged.
    if mode.selection.is_some() {
        exit_guard.cancel();
    }
    Ok(AgentsViewOutcome {
        selection: mode.selection,
        frames,
        query: (!mode.query.is_empty()).then(|| mode.query.clone()),
    })
}

/// Pop the next queued input, or `None` when the queue is empty.
fn first_input(pending: &mut Vec<UiInput>) -> Option<UiInput> {
    if pending.is_empty() {
        None
    } else {
        Some(pending.remove(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One idle row under test plus a holder row that keeps the selection,
    /// with the given title and one model id. The activity text and cost/age
    /// stay fixed so the expected rows are exact.
    fn mode_with_row(title: &str, model: &str) -> (AgentsViewMode, usize) {
        let mut mode = AgentsViewMode::new(AgentsViewOptions {
            socket_path: PathBuf::from("/tmp/agents-view-test.sock"),
            cwd: PathBuf::from("/tmp"),
            session_dir: None,
            theme: "prime".to_string(),
            version: "0.0.0".to_string(),
            anchor_session_id: None,
            scope: None,
            query: None,
        });
        let row = |title: &str| AgentsViewRow {
            section: Section::Idle,
            identity: title.to_string(),
            summary: serde_json::json!({ "sessionName": title }),
            title: title.to_string(),
            status_label: String::new(),
            model: model.to_string(),
            activity: "idle now".to_string(),
            cost: 0.0,
            age: "1s".to_string(),
        };
        mode.rows = vec![row("holder"), row(title)];
        (mode, 1)
    }

    fn flat(line: &Line) -> String {
        line.iter().map(|s| s.content.as_str()).collect()
    }

    /// The exact expected idle-row text: name cell (icon + title, clipped or
    /// padded to `name_width`), model and activity cells padded to their
    /// columns, then the cost/age details.
    fn expected_row(title_cell: &str, layout: &RowLayout) -> String {
        let bullet = "\u{2022}";
        format!(
            "{bullet} {title_cell}  {}  {}  $0.00   1s",
            cell("mock-1", layout.model_width),
            cell("idle now", layout.activity_width),
        )
    }

    #[test]
    fn long_session_names_clip_to_the_name_column() {
        let (mode, index) = mode_with_row(&"a".repeat(100), "mock-1");
        let layout = build_layout(&mode.rows, 120);
        // TS `buildCompactAgentsViewLayout` at width 120 with these rows.
        assert_eq!(layout.name_width, 28);
        assert_eq!(layout.model_width, 12);
        assert_eq!(layout.activity_width, 64);
        let line = mode.render_row(&mode.rows[index], &layout, 120);
        let text = flat(&line);
        // TS `formatTableCell` clips with an empty ellipsis marker: the
        // name cell keeps the icon and space plus 26 name characters.
        assert_eq!(text, expected_row(&"a".repeat(26), &layout));
        // Every column still renders after the clipped name.
        let model_at = text.find("mock-1").expect("model column present");
        assert_eq!(str_width(&text[..model_at]), 28 + 2);
        assert!(text.ends_with("$0.00   1s"));
    }

    #[test]
    fn short_session_names_pad_to_the_name_column() {
        let (mode, index) = mode_with_row("short name", "mock-1");
        let layout = build_layout(&mode.rows, 120);
        assert_eq!(layout.name_width, 28);
        let line = mode.render_row(&mode.rows[index], &layout, 120);
        let text = flat(&line);
        let name_cell = format!("short name{}", " ".repeat(28 - 2 - 10));
        assert_eq!(text, expected_row(&name_cell, &layout));
    }
}
