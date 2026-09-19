//! Terminal UI for Prime Agent.
//!
//! Rust port of the TS reference TUI (packages/tui) plus the interactive agent view
//! from `coding-agent/src/modes/interactive`. Components render styled lines;
//! the terminal layer paints them with crossterm + ratatui diffing.

pub mod agents_view;
pub mod agents_view_forest;
pub mod agents_view_state;
pub mod altscreen;
pub mod ansi;
pub mod app;
pub mod autocomplete;
pub mod chat;
pub mod chat_slash;
pub mod chrome;
pub mod client_auth;
mod clipboard_image;
pub mod code_preview;
mod compaction_row;
pub mod config_selector;
pub mod custom_message;
pub mod daemon_client;
pub mod direct_transport;
pub mod editor;
pub mod effort_picker;
pub mod error_summary;
mod exit_guard;
pub mod export_share;
pub mod fuzzy;
pub mod goal_surface;
pub mod hotkeys;
pub mod hyperlinks;
mod image_component;
mod image_load;
mod image_markers;
mod input;
pub mod interactive;
pub mod keybindings;
pub mod keys;
pub mod markdown;
pub mod markdown_table;
mod menu_panel;
pub mod model_picker;
pub(crate) mod mouse;
pub(crate) mod mouse_tracking;
pub mod onboarding;
pub mod osc133;
pub mod queued;
pub(crate) mod search_input;
pub mod session;
pub mod session_ui;
pub mod snapshot;
pub mod subagents;
mod terminal_image;
pub mod theme;
pub mod tool_card;
pub mod tree_display;
pub mod tree_list;
pub mod tree_nodes;
pub mod tree_selector;
pub mod user_message_selector;
pub mod view;
pub mod width;

use ratatui::style::Style;

/// A styled run of text. `style` applies to the whole `content`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub style: Style,
    pub content: String,
}

impl Span {
    pub fn raw(content: impl Into<String>) -> Self {
        Span {
            style: Style::default(),
            content: content.into(),
        }
    }

    pub fn styled(content: impl Into<String>, style: Style) -> Self {
        Span {
            style,
            content: content.into(),
        }
    }
}

/// A rendered line: styled spans laid out left to right.
pub type Line = Vec<Span>;

/// Render trait shared by all components.
pub trait Component {
    /// Render to lines for the given viewport width. Lines must not exceed
    /// `width` visible columns.
    fn render(&self, width: u16) -> Vec<Line>;
}
