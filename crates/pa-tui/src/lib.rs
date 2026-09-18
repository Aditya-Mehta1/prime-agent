//! Terminal UI for Prime Agent.
//!
//! Rust port of the TS reference TUI (packages/tui) plus the interactive agent view
//! from `coding-agent/src/modes/interactive`. Components render styled lines;
//! the terminal layer paints them with crossterm + ratatui diffing.

pub mod agents_view;
pub mod agents_view_state;
pub mod ansi;
pub mod app;
pub mod autocomplete;
pub mod chat;
pub mod chat_slash;
pub mod chrome;
pub mod client_auth;
pub mod code_preview;
pub mod config_selector;
pub mod daemon_client;
pub mod direct_transport;
pub mod editor;
pub mod error_summary;
pub mod fuzzy;
pub mod interactive;
pub mod keybindings;
pub mod keys;
pub mod markdown;
pub mod onboarding;
pub mod session;
pub mod session_ui;
pub mod snapshot;
pub mod theme;
pub mod tool_card;
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
