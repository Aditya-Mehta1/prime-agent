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

use crate::agents_view_forest::{
    ancestor_session_ids, build_rows, compute_rollups, has_session_children, resolve_selection,
    scope_ancestors, scope_depth, scope_to_subtree, AgentsViewRow, RowKind, SelectionKey,
};
use crate::agents_view_state::truncate_text;
use crate::agents_view_state::{
    build_layout, filter_empty_sessions, filter_unified_sessions, parse_search_query,
    reconcile_unified_sessions, section_title, RowLayout, Section,
};

/// The scope a scoped view opened on (TS `AgentsViewScopeKey` plus the
/// display name): the view lists this session's descendants and the back
/// key returns to it.
pub use crate::agents_view_forest::AgentsViewScope;
pub use crate::agents_view_forest::SelectionKey as AgentsViewSelectionKey;
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
    /// Session ids to re-expand on open, root-most first (TS
    /// `pendingExpandedAncestorSessionIds`: returning from a drilled-in
    /// child re-opens the tree down to the row the user left).
    pub expanded_ancestors: Vec<String>,
    /// The row identity to restore the selection on (TS
    /// `persistentState.selectedRowIdentity`).
    pub selected_row_identity: Option<String>,
    /// The selection key that survives an identity flip (TS
    /// `persistentState.selectedSessionKey`).
    pub selected_key: Option<SelectionKey>,
    /// A status message the previous run left for this one (TS
    /// `persistentState.statusMessage`): the unattachable-child fallback.
    pub status_message: Option<String>,
}

/// The open action the run ended with (TS `AgentsViewRunResult`'s
/// `open`/`scope_back` arms, unified): the session the flow opens plus
/// the row metadata it carries across the view/session loop.
#[derive(Debug, Clone)]
pub struct OpenedRow {
    pub selection: SessionSelection,
    pub expanded_ancestors: Vec<String>,
    pub selected_row_identity: String,
    pub selected_key: SelectionKey,
    pub rlm_depth: Option<u32>,
    pub has_children: bool,
    pub status_message: Option<String>,
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
    /// The view exited through its parent key while scoped (TS
    /// `scope_back`): the flow pops the scope frame, so a later agents-back
    /// lands in the parent scope, not this one.
    pub scope_popped: bool,
    /// The scope root left the roster mid-run (TS
    /// `resolveAgentsViewScopeFrames` dropping a frame): the flow drops the
    /// scope frame.
    pub scope_dropped: bool,
    /// Session ids of the opened row's ancestors, root-most first (TS
    /// `expandedAncestorSessionIds`): the flow feeds the next view run so
    /// the tree re-expands to the drilled row.
    pub expanded_ancestors: Vec<String>,
    /// The opened (or scope-back) row's identity and key, for the next
    /// run's selection restore (TS `persistentState.selectedRowIdentity` /
    /// `selectedSessionKey`).
    pub selected_row_identity: Option<String>,
    pub selected_key: Option<SelectionKey>,
    /// The opened session's `rlmDepth` (TS `sessionDepth`): a drilled-in
    /// child renders its `depth N` tray label.
    pub opened_rlm_depth: Option<u32>,
    /// Whether the opened session has direct children (TS
    /// `sessionHasChildren`).
    pub opened_has_children: bool,
    /// A status message the session opener left (TS
    /// `statusMessage` on the open result): the unattachable-child
    /// fallback surfaces it in the next view run.
    pub status_message: Option<String>,
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
    /// Whether the scope root left the roster mid-run (TS
    /// `resolveAgentsViewScopeFrames` dropping the frame): reported on the
    /// outcome so the flow drops the scope.
    scope_dropped: bool,
    /// Parent row identities whose subagent lists are expanded (TS
    /// `expandedSubagentParents`).
    expanded_parents: std::collections::HashSet<String>,
    /// Session ids to expand on the next rebuild (TS
    /// `pendingExpandedAncestorSessionIds`, consumed once).
    pending_ancestors: Option<Vec<String>>,
    /// The row identity the selection restores on (TS
    /// `persistentState.selectedRowIdentity`).
    selected_identity: Option<String>,
    /// The selection key that survives an identity flip (TS
    /// `persistentState.selectedSessionKey`).
    selected_key: Option<SelectionKey>,
    /// First ctrl+c shows the exit hint; the second exits.
    exit_armed: bool,
    /// The double-Ctrl+C force-quit guard (the run's shared instance is
    /// installed by `run_agents_view` after `new`).
    exit_guard: crate::exit_guard::ExitGuard,
    pulse: usize,
    running: bool,
    /// The view exited through its parent key (TS `scope_back`): the flow
    /// pops the scope frame.
    scope_popped: bool,
    /// The open action the run ended with (`None` while the view runs).
    opened: Option<OpenedRow>,
    /// ctrl+n requested a fresh session (TS `app.agents.new`).
    new_session: bool,
}

impl AgentsViewMode {
    fn new(options: AgentsViewOptions) -> Self {
        let theme = crate::app::load_theme(&options.theme);
        let query = options.query.clone().unwrap_or_default();
        let status = options.status_message.clone();
        let pending_ancestors =
            (!options.expanded_ancestors.is_empty()).then(|| options.expanded_ancestors.clone());
        let selected_identity = options.selected_row_identity.clone();
        let selected_key = options.selected_key.clone();
        AgentsViewMode {
            options,
            theme,
            roster: Vec::new(),
            saved: Vec::new(),
            rows: Vec::new(),
            selected: 0,
            query,
            status,
            scope_depth: None,
            scope_active: false,
            scope_dropped: false,
            expanded_parents: Default::default(),
            pending_ancestors,
            selected_identity,
            selected_key,
            exit_armed: false,
            exit_guard: crate::exit_guard::ExitGuard::new(),
            pulse: 0,
            running: true,
            scope_popped: false,
            opened: None,
            new_session: false,
        }
    }

    /// The unified records the view runs on (reconciled from the live
    /// roster and the saved catalog).
    fn records(&self) -> Vec<crate::agents_view_state::UnifiedRecord> {
        reconcile_unified_sessions(&self.roster, &self.saved)
    }

    /// Rebuild rows from the current roster, catalog, and query (TS
    /// `reconcileCatalogs` + `getFilteredRecords`). A scoped run lists the
    /// scope root's subtree with the root's own row excluded (its direct
    /// children list as top-level rows); a scope root that left the roster
    /// falls back to the global list with a status message and reports the
    /// drop so the flow discards the scope.
    fn rebuild_rows(&mut self) {
        let identity = self.rows.get(self.selected).map(|row| row.identity.clone());
        let records = self.records();
        // Scope resolution (TS `resolveAgentsViewScopeFrames`): a frame
        // whose root is gone drops, with the nearest fallback surfaced as a
        // status message.
        let mut scope_active = false;
        let scoped = match &self.options.scope {
            Some(scope) if !self.scope_dropped => match scope_to_subtree(&records, scope) {
                Some(scoped) => {
                    scope_active = true;
                    self.scope_depth = scope_depth(&records, scope);
                    Some(scoped)
                }
                None => {
                    self.scope_depth = None;
                    self.scope_dropped = true;
                    self.status = Some(
                        "Scope is no longer available; returned to the global view".to_string(),
                    );
                    None
                }
            },
            _ => None,
        };
        self.scope_active = scope_active;
        let working: &[_] = match &scoped {
            Some(scoped) => scoped,
            None => &records,
        };
        // The empty-catalog filter preserves the anchor and the scope root
        // (TS `preservedSessionIds`); the search filter keeps ancestors so
        // a match never orphans its parent row.
        let mut preserved = Vec::new();
        if let Some(anchor) = self.options.anchor_session_id.as_deref() {
            preserved.push(anchor);
        }
        if let Some(scope) = self.options.scope.as_ref() {
            if let Some(session) = scope.session_id.as_deref() {
                preserved.push(session);
            }
        }
        let filtered = filter_empty_sessions(working, &preserved);
        let filtered = if self.query.trim().is_empty() {
            filtered
        } else {
            let parsed = parse_search_query(self.query.trim());
            filter_unified_sessions(&filtered, &parsed)
        };
        let rollups = compute_rollups(&filtered);
        let mut rows = build_rows(
            &filtered,
            self.options.scope.as_ref(),
            &self.expanded_parents,
            &rollups,
            self.options.anchor_session_id.as_deref(),
        );
        // Re-expand the drilled-in row's ancestors (TS
        // `applyPendingAncestorExpansion`): a nested ancestor's row only
        // appears once its own parent is expanded, so expand-and-rebuild
        // until a pass reveals nothing new.
        if let Some(wanted) = self.pending_ancestors.take() {
            let mut added = true;
            while added {
                added = false;
                for row in &rows {
                    if row.kind == RowKind::SubagentSummary {
                        continue;
                    }
                    let session_id = row.summary.get("sessionId").and_then(Value::as_str);
                    if session_id.is_some_and(|id| wanted.iter().any(|w| w == id))
                        && self.expanded_parents.insert(row.identity.clone())
                    {
                        added = true;
                    }
                }
                if added {
                    rows = build_rows(
                        &filtered,
                        self.options.scope.as_ref(),
                        &self.expanded_parents,
                        &rollups,
                        self.options.anchor_session_id.as_deref(),
                    );
                }
            }
        }
        // Keep the selection on the same row across rebuilds, falling back
        // to the carried identity/key (TS `resolveAgentsViewSelectionState`).
        self.selected = resolve_selection(
            &rows,
            self.selected,
            identity.as_deref().or(self.selected_identity.as_deref()),
            self.selected_key.as_ref(),
        );
        self.rows = rows;
        // Track the selected row's identity and key (TS
        // `syncSelectedRowState`): they survive rebuilds and view re-entry.
        if let Some(row) = self.rows.get(self.selected) {
            self.selected_identity = Some(row.identity.clone());
            self.selected_key = Some(crate::agents_view_forest::selection_key(&row.summary));
        } else {
            self.selected_identity = None;
            self.selected_key = None;
        }
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

    /// Move the selection by `delta` selectable rows (TS `moveSelection`).
    fn move_selection(&mut self, delta: isize) {
        let selectable: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.selectable())
            .map(|(index, _)| index)
            .collect();
        if selectable.is_empty() {
            self.selected = 0;
            return;
        }
        let current = selectable
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        let next = (current as isize + delta).clamp(0, selectable.len() as isize - 1) as usize;
        self.selected = selectable[next];
    }

    /// Open the selected row (TS `openSelected`): the summary row toggles
    /// its list, a nested child drills into its transcript with its
    /// ancestor chain, and a top-level agent opens its session.
    fn open_selected(&mut self) {
        let Some(row) = self.rows.get(self.selected).cloned() else {
            return;
        };
        match row.kind {
            RowKind::SubagentSummary => self.toggle_subagent_list(&row),
            RowKind::Subagent => self.open_subagent_row(&row),
            RowKind::Agent => self.open_row(&row, Vec::new()),
        }
    }

    /// Toggle the selected parent's subagent list (TS `toggleSubagentList`):
    /// alt+right and open both land here; the target is the selected row's
    /// parent for a summary row, the row itself otherwise.
    fn toggle_subagent_list(&mut self, row: &AgentsViewRow) {
        let target = match row.kind {
            RowKind::SubagentSummary => row.parent_identity.clone(),
            _ => Some(row.identity.clone()),
        };
        let Some(target) = target else {
            return;
        };
        if self.expanded_parents.remove(&target) {
            // Collapsing also hides the spawn program (TS clears
            // `programShownParents` with the expansion); the program
            // surface is not part of this lane.
        } else {
            self.expanded_parents.insert(target);
        }
        self.rebuild_rows();
    }

    /// Drill into a nested child row (TS `openSelectedSubagent`): the open
    /// result carries the child's ancestor chain, so the tree re-expands to
    /// the row when the chat returns to the view.
    fn open_subagent_row(&mut self, row: &AgentsViewRow) {
        let ancestors = ancestor_session_ids(&self.rows, row.parent_identity.as_deref());
        if row.summary.get("activeSessionId").is_some() || row.summary.get("sessionFile").is_some()
        {
            self.open_row(row, ancestors);
            return;
        }
        // The whole subagent tree belongs to its root agent's session, so a
        // child without its own runtime resolves to its top-level ancestor
        // (TS `createUnattachableChildOpenResult`): open the parent, keep
        // the child row selected, and surface why.
        let root = self.find_subagent_root_row(row);
        let Some(root) = root else {
            self.status = Some(
                "Cannot open agent without an active runtime or saved session file".to_string(),
            );
            return;
        };
        let root = root.clone();
        self.status = None;
        self.open_row_with(
            &root,
            ancestors,
            Some("Child session is unavailable; opened its parent instead".to_string()),
            Some(row.identity.clone()),
        );
    }

    /// The top-level ancestor row of a nested row (TS `findSubagentRootRow`).
    fn find_subagent_root_row(&self, row: &AgentsViewRow) -> Option<&AgentsViewRow> {
        let mut identity = row.parent_identity.clone();
        let mut guard = 0;
        while let Some(current) = identity {
            guard += 1;
            if guard > self.rows.len() {
                return None;
            }
            let parent = self
                .rows
                .iter()
                .find(|candidate| candidate.identity == current)?;
            match parent.kind {
                RowKind::Agent => return Some(parent),
                _ => identity = parent.parent_identity.clone(),
            }
        }
        None
    }

    /// Open a session row: attach a live session, or reopen the saved
    /// file (TS `finish({ type: "open" })`), carrying the row's identity,
    /// key, and depth metadata for the flow.
    fn open_row(&mut self, row: &AgentsViewRow, ancestors: Vec<String>) {
        self.open_row_with(row, ancestors, None, None);
    }

    /// The open action shared by the drill-in paths.
    fn open_row_with(
        &mut self,
        row: &AgentsViewRow,
        ancestors: Vec<String>,
        status_message: Option<String>,
        selected_identity: Option<String>,
    ) {
        let summary = &row.summary;
        if let Some(active) = summary
            .get("activeSessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            self.finish_open(
                SessionSelection::Attach(active.to_string()),
                row,
                ancestors,
                status_message,
                selected_identity,
            );
            return;
        }
        if let Some(file) = summary
            .get("sessionFile")
            .and_then(Value::as_str)
            .filter(|file| !file.is_empty())
        {
            self.finish_open(
                SessionSelection::Resume(PathBuf::from(file)),
                row,
                ancestors,
                status_message,
                selected_identity,
            );
            return;
        }
        self.status =
            Some("Cannot open agent without an active runtime or saved session file".to_string());
    }

    /// Record the open outcome (TS the run result the loop consumes): the
    /// selection plus the row metadata the flow and the session carry.
    fn finish_open(
        &mut self,
        selection: SessionSelection,
        row: &AgentsViewRow,
        ancestors: Vec<String>,
        status_message: Option<String>,
        selected_identity: Option<String>,
    ) {
        let key = crate::agents_view_forest::selection_key(&row.summary);
        let has_children = has_session_children(&self.records(), &key);
        self.opened = Some(OpenedRow {
            selection,
            expanded_ancestors: ancestors,
            selected_row_identity: selected_identity.unwrap_or_else(|| row.identity.clone()),
            selected_key: key,
            rlm_depth: row
                .summary
                .get("rlmDepth")
                .and_then(Value::as_u64)
                .map(|depth| depth as u32),
            has_children,
            status_message,
        });
        self.running = false;
    }

    /// Hand the terminal back to the scope root's session (TS
    /// `finish({ type: "scope_back" })` when `pop`: the scoped view
    /// detaches and the flow pops the scope frame — the return chat it
    /// opened from reopens, and a later agents-back lands in the parent
    /// scope. Escape reopens the same session without popping the frame.
    fn open_scope_root(&mut self, pop: bool) {
        let Some(scope) = self.options.scope.clone() else {
            return;
        };
        let Some(active) = scope.active_session_id.clone().filter(|id| !id.is_empty()) else {
            // No runtime to return to: the flow reopens the view (TS
            // scope_back without a return chat continues the loop).
            self.scope_popped = pop;
            self.running = false;
            return;
        };
        self.scope_popped = pop;
        let summary = self
            .records()
            .iter()
            .map(crate::agents_view_state::summary_for_record)
            .find(|summary| {
                summary.get("activeSessionId").and_then(Value::as_str) == Some(active.as_str())
            })
            .unwrap_or_default();
        let key = crate::agents_view_forest::selection_key(&summary);
        let has_children = has_session_children(&self.records(), &key);
        self.opened = Some(OpenedRow {
            selection: SessionSelection::Attach(active),
            expanded_ancestors: scope_ancestors(&self.records(), &scope),
            selected_row_identity: self.selected_identity.clone().unwrap_or_default(),
            selected_key: self.selected_key.clone().unwrap_or_default(),
            rlm_depth: summary
                .get("rlmDepth")
                .and_then(Value::as_u64)
                .map(|depth| depth as u32),
            has_children,
            status_message: None,
        });
        self.running = false;
    }

    /// Handle one key id. Returns the status-line override when the caller
    /// should surface one (none of the PR-2 actions do).
    fn handle_key(&mut self, key: &str) {
        self.exit_armed = false;
        let selected = self.rows.get(self.selected).cloned();
        let has_query = !self.query.is_empty();
        match key {
            "up" => self.move_selection(-1),
            "down" => self.move_selection(1),
            "pageUp" => self.move_selection(-(self.rows.len() as isize).min(10)),
            "pageDown" => self.move_selection((self.rows.len() as isize).min(10)),
            // TS `app.agents.open` (right) and the editor submit (enter)
            // both open the selection (a non-empty query still opens while
            // the cursor sits at its end — always true for this editor);
            // the summary row toggles its list instead.
            "enter" | "right" => self.open_selected(),
            // TS `app.agents.expand` (alt+right): toggle the selected
            // parent's list when it has children.
            "alt+right" if !has_query => {
                if let Some(row) = selected {
                    if row.kind == RowKind::SubagentSummary || row.descendant_count > 0 {
                        self.toggle_subagent_list(&row);
                    }
                }
            }
            // TS `app.agents.new`: ctrl+n starts a session; a plain "n" is
            // search text like any other character.
            "ctrl+n" => {
                self.opened = None;
                self.running = false;
                self.new_session = true;
            }
            // The scoped view's parent key (TS `app.agents.back`): left
            // hands the terminal back to the scope root's session and pops
            // the scope; the global view has no hierarchy parent and
            // consumes left without opening a chat.
            "left" if !has_query => {
                if self.scope_active {
                    self.open_scope_root(true);
                }
            }
            "escape" => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.rebuild_rows();
                } else if self.scope_active {
                    // TS escape reopens the last-opened session (the scope
                    // root in this flow) without touching the scope frame.
                    self.open_scope_root(false);
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
    /// Nested rows (summary rows and expanded subagents) render inside
    /// their top-level agent's section block, and the headings count
    /// top-level agents only (TS `getDisplayRowsForSection` /
    /// `countRowsBySection`).
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
            let mut block: Vec<&AgentsViewRow> = Vec::new();
            let mut include = false;
            for row in &self.rows {
                if row.depth == 0 {
                    include = row.kind == RowKind::Agent && row.section == section;
                }
                if include {
                    block.push(row);
                }
            }
            if block.is_empty() {
                continue;
            }
            if lines.len() > 2 {
                lines.push(vec![]);
            }
            let top_level = block
                .iter()
                .filter(|row| row.kind == RowKind::Agent)
                .count();
            lines.push(vec![self.theme.fg(
                ThemeColor::Muted,
                format!("{} ({})", section_title(section), top_level),
            )]);
            for row in block {
                lines.push(self.render_row(row, &layout, width));
            }
        }
        while lines.len() > max_rows {
            lines.pop();
        }
        lines
    }

    /// One session row (TS `renderRow`): the summary rows render their
    /// `▸/▾ title` cell over the full width; agent rows render icon, title
    /// (nested rows indented), model, activity, cost/age. The selected row
    /// carries the selection background.
    fn render_row(&self, row: &AgentsViewRow, layout: &RowLayout, width: usize) -> Line {
        let theme = &self.theme;
        let selected = Some(row.identity.as_str())
            == self.rows.get(self.selected).map(|r| r.identity.as_str());
        if row.kind == RowKind::SubagentSummary {
            // TS: `formatTableCell(`${indent}${expanded ? "▾" : "▸"} ${title}`, width)`.
            let indent = "  ".repeat(row.depth);
            let marker = if row.expanded { "\u{25be}" } else { "\u{25b8}" };
            let text = format!("{indent}{marker} {}", row.title);
            let mut line: Line = vec![crate::Span::raw(crate::agents_view_state::truncate_text(
                &text, width,
            ))];
            line = pad_line(line, width);
            if selected {
                return theme.bg_paint(ThemeBg::SelectedBg, line);
            }
            return line;
        }
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
        // TS `renderRow`: `${"  ".repeat(depth)}${icon} ${title}` padded to
        // the name column, then the model and activity cells, then the dim
        // cost/age details.
        let named = row
            .summary
            .get("sessionName")
            .and_then(Value::as_str)
            .map(|name| !name.trim().is_empty())
            .unwrap_or(false);
        let indent = "  ".repeat(row.depth);
        let indent_width = str_width(&indent);
        let mut line: Line = Vec::new();
        if indent_width > 0 {
            line.push(crate::Span::raw(indent));
        }
        line.push(crate::Span::styled(icon, icon_style));
        line.push(crate::Span::styled(
            " ".to_string(),
            ratatui::style::Style::default(),
        ));
        // TS `formatTableCell(title, nameWidth)`: the name cell (indent +
        // icon + title) clips to the column width, so a long session name
        // can never push the model, activity, and cost/age columns
        // off-screen. The icon and its space take the first two cells.
        let title = truncate_text(
            &row.title,
            layout.name_width.saturating_sub(2 + indent_width),
        );
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
            " ".repeat(
                layout
                    .name_width
                    .saturating_sub(str_width(&title) + 2 + indent_width),
            ),
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
        // Ctrl+N. The summary row swaps the open action for expand/collapse
        // (TS `renderHints`'s `rightAction`); the scoped view adds the
        // parent-back hint.
        let right_action = match self.rows.get(self.selected) {
            Some(row) if row.kind == RowKind::SubagentSummary => {
                if row.expanded {
                    "collapse"
                } else {
                    "expand"
                }
            }
            _ => "open",
        };
        let hints = if self.scope_active {
            format!("\u{2191}/\u{2193} navigate   Enter/\u{2192} {right_action}   \u{2190} parent   Ctrl+N new")
        } else {
            format!("\u{2191}/\u{2193} navigate   Enter/\u{2192} {right_action}   Ctrl+N new")
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
                UiInput::Settled => {}
                // The headless plan ended: the run stops here (the
                // interactive harness's `HeadlessDone` contract). A plan
                // that ends without an exit key still captures its frames
                // and returns instead of spinning forever.
                UiInput::Done => mode.running = false,
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
    let frames = renderer.finish(mode.opened.is_some() || mode.new_session);
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
    if mode.opened.is_some() || mode.new_session {
        exit_guard.cancel();
    }
    let opened = mode.opened.take();
    Ok(AgentsViewOutcome {
        selection: opened
            .as_ref()
            .map(|row| row.selection.clone())
            .or(mode.new_session.then_some(SessionSelection::New)),
        frames,
        query: (!mode.query.is_empty()).then(|| mode.query.clone()),
        scope_popped: mode.scope_popped,
        scope_dropped: mode.scope_dropped,
        expanded_ancestors: opened
            .as_ref()
            .map(|row| row.expanded_ancestors.clone())
            .unwrap_or_default(),
        selected_row_identity: opened.as_ref().map(|row| row.selected_row_identity.clone()),
        selected_key: opened.as_ref().map(|row| row.selected_key.clone()),
        opened_rlm_depth: opened.as_ref().and_then(|row| row.rlm_depth),
        opened_has_children: opened.as_ref().map(|row| row.has_children).unwrap_or(false),
        status_message: opened.as_ref().and_then(|row| row.status_message.clone()),
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
            expanded_ancestors: Vec::new(),
            selected_row_identity: None,
            selected_key: None,
            status_message: None,
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
            depth: 0,
            descendant_count: 0,
            running_subagent_count: 0,
            expanded: false,
            parent_identity: None,
            kind: RowKind::Agent,
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

    fn roster_entry(agent: &str, status: &str, summary: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "agentId": agent, "status": status, "summary": summary })
    }

    fn parent_summary(id: &str) -> serde_json::Value {
        serde_json::json!({
            "sessionId": id,
            "activeSessionId": format!("{id}-live"),
            "sessionFile": format!("/x/{id}.jsonl"),
            "runtimeKind": "top-level",
            "sessionName": format!("{id} name"),
            "messageCount": 2,
            "rlmDepth": 0,
        })
    }

    fn child_summary(id: &str, parent: &str, name: &str) -> serde_json::Value {
        serde_json::json!({
            "sessionId": id,
            "activeSessionId": format!("{id}-live"),
            "sessionFile": format!("/x/{id}.jsonl"),
            "runtimeKind": "subagent",
            "rlmChildId": format!("child-{id}"),
            "parentActiveSessionId": format!("{parent}-live"),
            "parentSessionId": parent,
            "parentSessionPath": format!("/x/{parent}.jsonl"),
            "sessionName": name,
            "messageCount": 1,
            "rlmDepth": 1,
        })
    }

    /// A mode over a live parent/child roster, no scope, fresh selection.
    fn mode_with_parent_and_child() -> AgentsViewMode {
        let mut mode = AgentsViewMode::new(AgentsViewOptions {
            socket_path: PathBuf::from("/tmp/agents-view-test.sock"),
            cwd: PathBuf::from("/tmp"),
            session_dir: None,
            theme: "prime".to_string(),
            version: "0.0.0".to_string(),
            anchor_session_id: None,
            scope: None,
            query: None,
            expanded_ancestors: Vec::new(),
            selected_row_identity: None,
            selected_key: None,
            status_message: None,
        });
        mode.roster = vec![
            roster_entry("p", "idle", parent_summary("p")),
            roster_entry("c", "running", child_summary("c", "p", "worker one")),
        ];
        mode.rebuild_rows();
        mode
    }

    #[test]
    fn alt_right_toggles_the_subagent_list() {
        let mut mode = mode_with_parent_and_child();
        // Collapsed: the parent, its summary row, nothing else.
        assert_eq!(mode.rows.len(), 2);
        assert_eq!(mode.rows[1].kind, RowKind::SubagentSummary);
        assert!(!mode.rows[1].expanded);
        // alt+right on the parent row (descendantCount > 0) expands.
        mode.handle_key("alt+right");
        assert_eq!(mode.rows.len(), 3);
        assert!(mode.rows[1].expanded);
        assert_eq!(mode.rows[2].kind, RowKind::Subagent);
        assert_eq!(mode.rows[2].depth, 1);
        // alt+right again collapses.
        mode.handle_key("alt+right");
        assert_eq!(mode.rows.len(), 2);
        assert!(!mode.rows[1].expanded);
    }

    #[test]
    fn enter_toggles_the_summary_row_and_drills_into_a_child() {
        let mut mode = mode_with_parent_and_child();
        // The selection starts on the parent; down lands on the summary
        // row, and Enter toggles it (TS `openSelected` on a summary row).
        mode.handle_key("down");
        assert_eq!(mode.rows[mode.selected].kind, RowKind::SubagentSummary);
        mode.handle_key("enter");
        assert_eq!(mode.rows.len(), 3);
        assert!(mode.rows[1].expanded);
        // Enter on the summary row again collapses.
        mode.handle_key("enter");
        assert_eq!(mode.rows.len(), 2);
        // Expand, walk to the child, drill in (TS `openSelectedSubagent`).
        mode.handle_key("enter");
        mode.handle_key("down");
        assert_eq!(mode.rows[mode.selected].kind, RowKind::Subagent);
        mode.handle_key("enter");
        let opened = mode.opened.as_ref().expect("open recorded");
        assert_eq!(
            opened.selection,
            SessionSelection::Attach("c-live".to_string())
        );
        // The drill-in carries the ancestor chain for the return
        // re-expansion and the child's depth for its tray label.
        assert_eq!(opened.expanded_ancestors, vec!["p".to_string()]);
        assert_eq!(opened.rlm_depth, Some(1));
        // The child itself has no children in this fixture.
        assert!(!opened.has_children);
        assert!(!mode.running);
    }

    #[test]
    fn pending_ancestors_expand_and_selection_restores_after_reentry() {
        // A fresh run carrying the drilled-in child's return state (TS
        // `pendingExpandedAncestorSessionIds` + the persisted selection).
        let mut mode = AgentsViewMode::new(AgentsViewOptions {
            socket_path: PathBuf::from("/tmp/agents-view-test.sock"),
            cwd: PathBuf::from("/tmp"),
            session_dir: None,
            theme: "prime".to_string(),
            version: "0.0.0".to_string(),
            anchor_session_id: None,
            scope: None,
            query: None,
            expanded_ancestors: vec!["p".to_string()],
            selected_row_identity: None,
            selected_key: Some(crate::agents_view_forest::SelectionKey {
                session_id: Some("c".to_string()),
                active_session_id: Some("c-live".to_string()),
            }),
            status_message: None,
        });
        mode.roster = vec![
            roster_entry("p", "idle", parent_summary("p")),
            roster_entry("c", "running", child_summary("c", "p", "worker one")),
        ];
        mode.rebuild_rows();
        // The ancestor expansion opened the parent's list and the child
        // row's selection restored.
        assert_eq!(mode.rows.len(), 3);
        assert!(mode.rows[1].expanded);
        assert_eq!(mode.rows[mode.selected].title, "worker one");
    }

    #[test]
    fn scoped_left_returns_the_root_and_pops_the_scope() {
        let mut mode = AgentsViewMode::new(AgentsViewOptions {
            socket_path: PathBuf::from("/tmp/agents-view-test.sock"),
            cwd: PathBuf::from("/tmp"),
            session_dir: None,
            theme: "prime".to_string(),
            version: "0.0.0".to_string(),
            anchor_session_id: None,
            scope: Some(AgentsViewScope {
                session_id: Some("p".to_string()),
                active_session_id: Some("p-live".to_string()),
                session_name: Some("p name".to_string()),
            }),
            query: None,
            expanded_ancestors: Vec::new(),
            selected_row_identity: None,
            selected_key: None,
            status_message: None,
        });
        mode.roster = vec![
            roster_entry("p", "idle", parent_summary("p")),
            roster_entry("c", "running", child_summary("c", "p", "worker one")),
        ];
        mode.rebuild_rows();
        // The scoped view lists the direct child as a top-level row.
        assert!(mode.scope_active);
        assert_eq!(mode.rows.len(), 1);
        assert_eq!(mode.rows[0].kind, RowKind::Agent);
        // The parent key hands the terminal back to the scope root and
        // marks the scope popped for the flow.
        mode.handle_key("left");
        assert!(mode.scope_popped);
        let opened = mode.opened.as_ref().expect("scope-back open");
        assert_eq!(
            opened.selection,
            SessionSelection::Attach("p-live".to_string())
        );
        // The scope root has no ancestors of its own, so nothing
        // re-expands after the return chat.
        assert!(opened.expanded_ancestors.is_empty());
    }

    #[test]
    fn unattachable_child_opens_its_root_with_a_status() {
        let mut mode = mode_with_parent_and_child();
        // A finished child with no runtime and no file resolves to its
        // top-level ancestor (TS `createUnattachableChildOpenResult`).
        let unattachable = serde_json::json!({
            "sessionId": "gc",
            "runtimeKind": "subagent",
            "rlmChildId": "child-gc",
            "rlmDepth": 2,
            "parentActiveSessionId": "c-live",
            "parentSessionId": "c",
            "sessionName": "lost grandchild",
            "messageCount": 1,
        });
        mode.roster
            .push(roster_entry("gc", "inactive", unattachable));
        mode.expanded_parents.insert("file:/x/p.jsonl".to_string());
        mode.rebuild_rows();
        // The child row's identity comes from the roster-qualified id.
        let child_identity = mode
            .rows
            .iter()
            .find(|row| row.title == "worker one")
            .expect("child row renders")
            .identity
            .clone();
        mode.expanded_parents.insert(child_identity);
        mode.rebuild_rows();
        let grandchild = mode
            .rows
            .iter()
            .position(|row| row.title == "lost grandchild")
            .expect("grandchild row renders");
        mode.selected = grandchild;
        mode.handle_key("enter");
        let opened = mode.opened.as_ref().expect("open recorded");
        // The parent chain's root session opens instead, with the child
        // row kept for the selection restore and a status message.
        assert_eq!(
            opened.selection,
            SessionSelection::Attach("p-live".to_string())
        );
        assert_eq!(
            opened.status_message.as_deref(),
            Some("Child session is unavailable; opened its parent instead")
        );
    }
}
