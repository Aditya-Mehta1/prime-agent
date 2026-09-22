//! The `/mcp` inline connections view: the TS `handleMcpCommand`'s bare
//! arm — the configuration menu's MCP Connections tab (the inline
//! `OAuthSelectorComponent`) over the daemon's connection roster, plus the
//! tool listing each connected generic server offers (the
//! `get_mcp_connections` seam). Same inline geometry as the `/model`
//! picker: the bordered search field over `›`-marker rows, the selected
//! connection's detail block, and the navigate/select/close hint. Enter
//! runs the connection's login flow (TS `onSelectMcpConnection` ->
//! `authenticate`); Esc closes.

use crate::keybindings::{format_key_text, KeybindingsManager};
use crate::menu_panel::{menu_list_layout, menu_row, search_field_lines};
use crate::search_input::SearchInput;
use crate::theme::{Theme, ThemeColor};
use crate::{Line, Span};
use serde_json::Value;

/// The search field's placeholder (TS `MenuSearchInput("Search MCP
/// connections")`).
const SEARCH_PLACEHOLDER: &str = "Search MCP connections";

/// Tool detail lines the view renders before the "+N more" tail.
const TOOL_DETAIL_LINES: usize = 6;

/// One tool a connected server offers (the kernel listing's entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
}

/// One connection row (the daemon `get_mcp_connections` entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConnection {
    pub server: String,
    pub label: String,
    /// Connected: credentials present and the server enabled.
    pub connected: bool,
    /// The kind cell the row shows (TS `subscription` / `api key`, or the
    /// transport for credential-less user servers).
    pub auth_kind: String,
    /// `None` when the listing was unavailable (no kernel, session busy)
    /// or the server is not listable (skills-based built-ins).
    pub tools: Option<Vec<McpToolInfo>>,
    /// The listing's failure text for this server (timeouts, handshake
    /// errors) when one was attempted and failed.
    pub error: Option<String>,
}

impl McpConnection {
    /// Parse one daemon roster entry.
    fn from_value(value: &Value) -> Option<Self> {
        Some(McpConnection {
            server: value.get("server")?.as_str()?.to_string(),
            label: value
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            connected: value
                .get("connected")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            auth_kind: value
                .get("authKind")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            tools: value.get("tools").and_then(Value::as_array).map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| {
                        Some(McpToolInfo {
                            name: tool.get("name")?.as_str()?.to_string(),
                            description: tool
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        })
                    })
                    .collect()
            }),
            error: value
                .get("error")
                .and_then(Value::as_str)
                .filter(|error| !error.is_empty())
                .map(str::to_string),
        })
    }
}

/// One key press while the view is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpViewAction {
    /// Enter on a connection: run its login flow (the caller resolves the
    /// auth hook; TS `authenticate` on the mcp-connections tab).
    Select(String),
    /// Esc, Ctrl+C, or back: close without selecting.
    Cancel,
    /// Navigation or search editing only.
    None,
}

/// The inline MCP connections view.
#[derive(Debug)]
pub struct McpView {
    connections: Vec<McpConnection>,
    search: SearchInput,
    filtered: Vec<usize>,
    selected: usize,
    viewport_rows: usize,
    visible_items: usize,
    last_query: String,
}

impl McpView {
    /// Build the view over the daemon's `get_mcp_connections` response (the
    /// `connections` array; a failed request yields the empty roster).
    pub fn from_response(data: &Value, viewport_rows: usize) -> Self {
        let connections: Vec<McpConnection> = data
            .get("connections")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(McpConnection::from_value)
                    .collect()
            })
            .unwrap_or_default();
        let mut view = McpView {
            connections,
            search: SearchInput::new(),
            filtered: Vec::new(),
            selected: 0,
            viewport_rows,
            visible_items: 8,
            last_query: String::new(),
        };
        view.refilter();
        view
    }

    /// The selected connection's server name (Enter's target).
    pub fn selected_server(&self) -> Option<&str> {
        self.connections
            .get(*self.filtered.get(self.selected)?)
            .map(|connection| connection.server.as_str())
    }

    /// One key id (the same binding set as the `/model` picker, without the
    /// effort cluster).
    pub fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> McpViewAction {
        if key == "ctrl+c" {
            return McpViewAction::Cancel;
        }
        if kb.matches(key, "tui.select.up") || kb.matches(key, "tui.select.down") {
            let count = self.filtered.len();
            if count > 0 {
                let direction = if kb.matches(key, "tui.select.up") {
                    -1isize
                } else {
                    1
                };
                self.selected =
                    (self.selected as isize + direction).rem_euclid(count as isize) as usize;
            }
            return McpViewAction::None;
        }
        if kb.matches(key, "tui.select.pageUp") || kb.matches(key, "tui.select.pageDown") {
            let count = self.filtered.len();
            if count > 0 {
                let direction = if kb.matches(key, "tui.select.pageUp") {
                    -(self.visible_items as isize)
                } else {
                    self.visible_items as isize
                };
                self.selected =
                    (self.selected as isize + direction).clamp(0, count as isize - 1) as usize;
            }
            return McpViewAction::None;
        }
        if kb.matches(key, "tui.select.confirm") {
            return match self.selected_server() {
                Some(server) => McpViewAction::Select(server.to_string()),
                None => McpViewAction::None,
            };
        }
        if kb.matches(key, "tui.select.cancel")
            || (kb.matches(key, "app.modal.back") && self.search.cursor() == 0)
        {
            return McpViewAction::Cancel;
        }
        // Everything else edits the search field.
        let previous = self.search.value().to_string();
        self.search.handle_key(key, kb);
        if self.search.value() != previous {
            self.refilter();
        }
        McpViewAction::None
    }

    /// A bracketed paste into the search field.
    pub fn paste(&mut self, text: &str) {
        let previous = self.search.value().to_string();
        self.search.paste(text);
        if self.search.value() != previous {
            self.refilter();
        }
    }

    /// The picked frame (the inline panel: bordered search field, rows,
    /// scroll indicator, connection detail, hint).
    pub fn render(&mut self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        self.visible_items = self.list_layout();

        let mut lines = search_field_lines(
            theme,
            width,
            self.search.value(),
            self.search.cursor(),
            true,
            SEARCH_PLACEHOLDER,
        );

        let (start, end) = self.window();
        for index in start..end {
            let Some(&filtered_index) = self.filtered.get(index) else {
                continue;
            };
            let Some(connection) = self.connections.get(filtered_index) else {
                continue;
            };
            let selected = index == self.selected;
            let primary: Line = vec![Span::raw(format!(
                "{} \u{b7} {}",
                connection.label, connection.auth_kind
            ))];
            // Connected rows carry their status flush right (TS inline
            // `MenuRow` meta); a disconnected row's status shows in the
            // detail block instead.
            let trailing: Vec<&str> = if connection.connected {
                vec!["connected"]
            } else {
                Vec::new()
            };
            lines.push(menu_row(theme, width, primary, &trailing, selected));
        }

        if start > 0 || end < self.filtered.len() {
            let indicator = format!("  ({}/{})", self.selected + 1, self.filtered.len());
            lines.push(vec![theme.fg_span(ThemeColor::Muted, indicator)]);
        }

        if self.filtered.is_empty() {
            let message = if self.connections.is_empty() {
                "No MCP connections. Add servers under mcpServers in settings, then /mcp login <name>."
            } else {
                "No matching connections"
            };
            lines.push(vec![theme.fg_span(ThemeColor::Muted, message)]);
        } else if let Some(connection) = self
            .filtered
            .get(self.selected)
            .and_then(|index| self.connections.get(*index))
            .cloned()
        {
            lines.push(Vec::new());
            lines.extend(detail_lines(theme, width, &connection));
        }

        lines.push(hint_line(theme, width, kb));
        lines
    }

    /// The inline list layout (TS `getMenuListLayout` shape; the detail
    /// block reserves its tool rows).
    fn list_layout(&self) -> usize {
        menu_list_layout(
            Some(self.viewport_rows),
            8,
            self.filtered.len(),
            3 + TOOL_DETAIL_LINES,
            1,
        )
    }

    /// The visible row window centered on the selection.
    fn window(&self) -> (usize, usize) {
        let max_visible = self.visible_items.max(1);
        let selected = self.selected.min(self.filtered.len().saturating_sub(1));
        let start = selected
            .saturating_sub(max_visible / 2)
            .min(self.filtered.len().saturating_sub(max_visible));
        let end = (start + max_visible).min(self.filtered.len());
        (start, end)
    }

    /// Rebuild the filtered view: an empty query shows everything; a query
    /// fuzzy-matches the label and server name, resetting the selection.
    fn refilter(&mut self) {
        let query = self.search.value().to_string();
        let query_changed = query != self.last_query;
        self.last_query = query.clone();
        self.filtered = if query.trim().is_empty() {
            (0..self.connections.len()).collect()
        } else {
            self.connections
                .iter()
                .enumerate()
                .filter(|(_, connection)| {
                    crate::fuzzy::fuzzy_match(&query, &connection.label).is_some()
                        || crate::fuzzy::fuzzy_match(&query, &connection.server).is_some()
                })
                .map(|(index, _)| index)
                .collect()
        };
        if query_changed {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
        }
        self.visible_items = self.list_layout();
    }
}

/// The selected connection's detail block: its status, then one line per
/// tool it offers (the lane's required tool listing; the TS detail row
/// carries only the auth status).
fn detail_lines(theme: &Theme, width: usize, connection: &McpConnection) -> Vec<Line> {
    let status = if connection.connected {
        theme.fg_span(ThemeColor::Success, " connected")
    } else {
        theme.fg_span(ThemeColor::Muted, " disconnected")
    };
    let mut lines: Vec<Line> = Vec::new();
    match &connection.tools {
        Some(tools) if !tools.is_empty() => {
            let count_text = format!(
                "{} {}",
                tools.len(),
                if tools.len() == 1 { "tool" } else { "tools" }
            );
            let head = vec![
                status,
                Span::raw(" \u{b7} "),
                theme.fg_span(ThemeColor::Muted, count_text),
            ];
            lines.push(detail_line(theme, width, head));
            for tool in tools.iter().take(TOOL_DETAIL_LINES) {
                let mut line = vec![Span::raw("   "), Span::raw(tool.name.clone())];
                if !tool.description.is_empty() {
                    line.push(
                        theme.fg_span(ThemeColor::Muted, format!(" \u{2014} {}", tool.description)),
                    );
                }
                lines.push(detail_line(theme, width, line));
            }
            if tools.len() > TOOL_DETAIL_LINES {
                lines.push(detail_line(
                    theme,
                    width,
                    vec![
                        Span::raw("   "),
                        theme.fg_span(
                            ThemeColor::Muted,
                            format!("+{} more", tools.len() - TOOL_DETAIL_LINES),
                        ),
                    ],
                ));
            }
        }
        Some(_) => {
            lines.push(detail_line(
                theme,
                width,
                vec![
                    status,
                    Span::raw(" \u{b7} "),
                    theme.fg_span(ThemeColor::Muted, "no tools"),
                ],
            ));
        }
        None => {
            let mut head = vec![status];
            if let Some(error) = &connection.error {
                head.push(Span::raw(" \u{b7} "));
                head.push(
                    theme.fg_span(ThemeColor::Warning, format!("tools unavailable ({error})")),
                );
            } else if connection.connected {
                head.push(Span::raw(" \u{b7} "));
                head.push(theme.fg_span(ThemeColor::Muted, "tools unavailable"));
            }
            lines.push(detail_line(theme, width, head));
        }
    }
    lines
}

/// Truncate one detail line to the pane width and pad it to the full row.
fn detail_line(theme: &Theme, width: usize, line: Line) -> Line {
    let line = crate::width::truncate_line(&line, width, "\u{2026}");
    let used = crate::width::spans_width(&line);
    let _ = theme;
    let mut line = line;
    if used < width {
        line.push(Span::raw(" ".repeat(width - used)));
    }
    line
}

/// The trailing key hint (TS `ConfigurationMenuComponent.render`, the
/// non-model tab wording): navigate/select/close on wide panes,
/// select/close below 70 columns.
fn hint_line(theme: &Theme, width: usize, kb: &KeybindingsManager) -> Line {
    let select_key = kb
        .first_key("tui.select.confirm")
        .map(|key| format_key_text(&key))
        .unwrap_or_else(|| "Enter".to_string());
    let close_key = kb
        .first_key("tui.select.cancel")
        .map(|key| format_key_text(&key))
        .unwrap_or_else(|| "Esc".to_string());
    let hint = if width >= 70 {
        let navigate = format!(
            "{}/{}",
            kb.first_key("tui.select.up")
                .map(|key| format_key_text(&key))
                .unwrap_or_else(|| "\u{2191}".to_string()),
            kb.first_key("tui.select.down")
                .map(|key| format_key_text(&key))
                .unwrap_or_else(|| "\u{2193}".to_string())
        );
        format!("{navigate} navigate \u{b7} {select_key} select \u{b7} {close_key} close")
    } else {
        format!("{select_key} select \u{b7} {close_key} close")
    };
    let line = vec![theme.fg_span(ThemeColor::Dim, format!(" {hint}"))];
    crate::width::truncate_line(&line, width, "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::KeybindingsManager;
    use crate::theme::{ColorMode, Theme};
    use serde_json::json;

    fn theme() -> Theme {
        Theme::builtin("prime", ColorMode::TrueColor)
    }

    fn kb() -> KeybindingsManager {
        KeybindingsManager::new()
    }

    /// Rendered rows as trimmed plain text (tmux-capture shape).
    fn frame_text(view: &mut McpView) -> Vec<String> {
        view.render(&theme(), 110, &kb())
            .iter()
            .map(|line| {
                line.iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
            })
            .map(|row| row.trim_end().to_string())
            .collect()
    }

    /// The daemon response shape: a connected generic stdio server with one
    /// listed tool, plus a disconnected built-in.
    fn roster_response() -> serde_json::Value {
        json!({
            "connections": [
                {
                    "server": "fixture-echo",
                    "label": "fixture-echo",
                    "connected": true,
                    "usesOAuth": false,
                    "authKind": "stdio",
                    "transport": "stdio",
                    "userDeclared": true,
                    "generic": true,
                    "tools": [
                        {
                            "name": "echo",
                            "description": "Echoes the message argument back."
                        }
                    ],
                    "error": null
                },
                {
                    "server": "linear",
                    "label": "Linear",
                    "connected": false,
                    "usesOAuth": true,
                    "authKind": "subscription",
                    "transport": "http",
                    "userDeclared": false,
                    "generic": false,
                    "tools": null,
                    "error": null
                }
            ]
        })
    }

    #[test]
    fn renders_the_ts_inline_panel_shape() {
        let mut view = McpView::from_response(&roster_response(), 19);
        let rows = frame_text(&mut view);
        let border = "\u{2500}".repeat(110);
        assert_eq!(rows[0], border, "top rule");
        assert_eq!(rows[1], " >  Search MCP connections", "search field");
        assert_eq!(rows[2], border, "bottom rule");
        // The connected row: `›` marker, label · kind, status flush right.
        let connected = rows
            .iter()
            .find(|row| row.starts_with('\u{203a}'))
            .expect("selected row");
        assert!(
            connected.starts_with("\u{203a} fixture-echo \u{b7} stdio"),
            "row primary: {connected}"
        );
        assert!(
            connected.ends_with("connected"),
            "status flush right: {connected}"
        );
        // The detail block: status, tool count, one line per tool.
        assert!(
            rows.iter()
                .any(|row| row.contains(" connected \u{b7} 1 tool")),
            "detail status row: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("echo \u{2014} Echoes the message argument back.")),
            "tool detail line: {rows:?}"
        );
        // The key hint.
        assert!(
            rows.iter().any(
                |row| row == " \u{2191}/\u{2193} navigate \u{b7} Enter select \u{b7} Esc close"
            ),
            "hint row"
        );
    }

    #[test]
    fn the_empty_roster_renders_the_empty_message() {
        let mut view = McpView::from_response(&json!({ "connections": [] }), 19);
        let rows = frame_text(&mut view);
        assert!(rows
            .iter()
            .any(|row| row.starts_with("No MCP connections.")));
    }

    #[test]
    fn disconnected_rows_show_the_status_in_the_detail_block() {
        let mut view = McpView::from_response(&roster_response(), 19);
        // Move to the disconnected builtin.
        view.handle_key("down", &kb());
        let rows = frame_text(&mut view);
        assert!(
            rows.iter().any(|row| row.contains(" disconnected")),
            "disconnected detail: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("Linear \u{b7} subscription")),
            "builtin row: {rows:?}"
        );
    }

    #[test]
    fn enter_selects_and_escape_cancels() {
        let mut view = McpView::from_response(&roster_response(), 19);
        assert_eq!(
            view.handle_key("enter", &kb()),
            McpViewAction::Select("fixture-echo".to_string())
        );
        view.handle_key("down", &kb());
        assert_eq!(
            view.handle_key("enter", &kb()),
            McpViewAction::Select("linear".to_string())
        );
        assert_eq!(view.handle_key("escape", &kb()), McpViewAction::Cancel);
        assert_eq!(view.handle_key("ctrl+c", &kb()), McpViewAction::Cancel);
    }

    #[test]
    fn typing_filters_by_label_and_server() {
        let mut view = McpView::from_response(&roster_response(), 19);
        for character in "linear".chars() {
            view.handle_key(&character.to_string(), &kb());
        }
        let rows = frame_text(&mut view);
        assert!(
            rows.iter()
                .any(|row| row.contains("Linear \u{b7} subscription")),
            "filtered row: {rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("fixture-echo")),
            "non-match filtered out: {rows:?}"
        );
        // Enter applies the surviving match.
        assert_eq!(
            view.handle_key("enter", &kb()),
            McpViewAction::Select("linear".to_string())
        );
        // A query with no matches renders the no-match row.
        view.handle_key("backspace", &kb());
        for _ in "linear".chars() {
            view.handle_key("backspace", &kb());
        }
        for character in "zzz".chars() {
            view.handle_key(&character.to_string(), &kb());
        }
        let rows = frame_text(&mut view);
        assert!(rows.iter().any(|row| row == "No matching connections"));
    }

    #[test]
    fn a_failed_listing_reports_its_error() {
        let data = json!({
            "connections": [
                {
                    "server": "broken",
                    "label": "broken",
                    "connected": true,
                    "usesOAuth": false,
                    "authKind": "stdio",
                    "transport": "stdio",
                    "userDeclared": true,
                    "generic": true,
                    "tools": null,
                    "error": "McpStartupError: fixture failed"
                }
            ]
        });
        let mut view = McpView::from_response(&data, 19);
        let rows = frame_text(&mut view);
        assert!(
            rows.iter()
                .any(|row| row.contains("tools unavailable (McpStartupError")),
            "error surfaced: {rows:?}"
        );
    }
}
