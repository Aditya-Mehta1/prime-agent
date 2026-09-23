//! The onboarding provider picker (TS `onboarding-picker.ts`): a searchable
//! list inside the onboarding block — a pinned Continue action, then the
//! matching provider entries in a scrolling viewport. Entries stay
//! selectable repeatedly, so a user can connect several providers before
//! moving on. Pure state like [`crate::onboarding::OnboardingScreen`]: keys
//! resolve through [`OnboardingPicker::handle_key`] into
//! [`OnboardingPickerAction`], and [`OnboardingPicker::render`] paints the
//! surface.
//!
//! The search field is the plain `MenuSearchInput` variant
//! ([`crate::menu_panel::search_field_plain_lines`], TS inline + plain +
//! hidePrompt): one line, no rules, no `"> "` prompt — the list marks
//! selection with its own caret, so the field hides the input prompt.

use crate::keybindings::KeybindingsManager;
use crate::menu_panel::search_field_plain_lines;
use crate::onboarding::highlight_wash;
use crate::search_input::SearchInput;
use crate::theme::{Theme, ThemeColor};
use crate::width::{spans_width, str_width, truncate_line};
use crate::{Line, Span};
use ratatui::style::{Color, Modifier};

/// The selection caret width (TS `MARKER_WIDTH`).
const MARKER_WIDTH: usize = 2;
/// The narrowest row band (TS `MIN_ROW_WIDTH`).
const MIN_ROW_WIDTH: usize = 34;
/// Room kept after the longest label (TS `ROW_TRAILING`).
const ROW_TRAILING: usize = 6;
/// Rows visible before the viewport scrolls (TS `DEFAULT_VISIBLE_ROWS`).
const DEFAULT_VISIBLE_ROWS: usize = 6;
/// The signed-in row's mark (TS `"  ✓"`): three columns.
const CONNECTED_MARK: &str = "  \u{2713}";
const CONNECTED_MARK_WIDTH: usize = 3;
const DEFAULT_SEARCH_PLACEHOLDER: &str = "Search";
const DEFAULT_CONTINUE_LABEL: &str = "Continue";

/// One pickable entry (TS `OnboardingPickerItem`): the provider's id, its
/// row label, and whether it is already signed in — a connected entry
/// carries the check mark instead of a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnboardingPickerItem {
    pub id: String,
    pub label: String,
    pub connected: bool,
}

/// Optional surface text and geometry (TS `OnboardingPickerOptions`). The TS
/// `requestRender` callback has no state here — the owner re-renders after
/// every key — and `onExit` answers through
/// [`OnboardingPickerAction::Exit`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OnboardingPickerConfig {
    /// The heading above the search field (TS falsy `prompt` skips it).
    pub prompt: Option<String>,
    /// The search field's placeholder (TS `searchPlaceholder ?? "Search"`).
    pub search_placeholder: Option<String>,
    /// The dim footer under the list (TS falsy `note` skips it).
    pub note: Option<String>,
    /// The pinned action's label (TS `continueLabel ?? "Continue"`).
    pub continue_label: Option<String>,
    /// Rows visible before the viewport scrolls (TS `visibleRows ?? 6`).
    pub visible_rows: Option<usize>,
    /// The row band width (TS `rowWidth ?? computed`; the pane still clamps
    /// it to `width - 1`).
    pub row_width: Option<usize>,
}

/// One key press while the picker owns the onboarding block (TS
/// `onSelect`/`onContinue`/`onCancel`/`onExit`); a press that only edits
/// state answers [`OnboardingPickerAction::None`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnboardingPickerAction {
    /// Enter on a provider row: the entry's id.
    Select { id: String },
    /// Enter on the pinned row: move the flow forward.
    Continue,
    /// The cancel key (`tui.select.cancel`).
    Cancel,
    /// The onboarding exit keys (`app.clear`/`app.exit`): quit while
    /// onboarding owns the pane (TS `isOnboardingExitKey`).
    Exit,
    /// Navigation or filter edit only: re-render and keep waiting.
    None,
}

/// The picker state (TS `OnboardingPickerComponent`): the query field, the
/// selection index — `0` is the pinned Continue row, `1..` the filtered
/// entries — and the viewport's first visible entry.
#[derive(Debug)]
pub struct OnboardingPicker {
    items: Vec<OnboardingPickerItem>,
    config: OnboardingPickerConfig,
    search: SearchInput,
    selected_index: usize,
    scroll_top: usize,
    focused: bool,
}

impl OnboardingPicker {
    /// Build the picker for `items` with the given surface options (TS
    /// constructor; the TS callbacks become [`OnboardingPickerAction`]).
    pub fn new(items: Vec<OnboardingPickerItem>, config: OnboardingPickerConfig) -> Self {
        OnboardingPicker {
            items,
            config,
            search: SearchInput::new(),
            selected_index: 0,
            scroll_top: 0,
            focused: false,
        }
    }

    /// Focus forwarding (TS `set focused`): the field shows its caret while
    /// the picker owns the pane.
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// One key id (TS `handleInput`). Exit keys answer first — `ctrl+c` is
    /// both `app.clear` and `tui.select.cancel`, and the exit wins — then
    /// navigation, then the confirm routing (0 = Continue, `n - 1` = the
    /// filtered entry), then cancel; every other key edits the query,
    /// clamps the selection into the new filter, and rewinds the viewport.
    pub fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> OnboardingPickerAction {
        if kb.matches(key, "app.clear") || kb.matches(key, "app.exit") {
            return OnboardingPickerAction::Exit;
        }
        if kb.matches(key, "tui.select.up") {
            self.move_selection(-1);
            return OnboardingPickerAction::None;
        }
        if kb.matches(key, "tui.select.down") {
            self.move_selection(1);
            return OnboardingPickerAction::None;
        }
        if kb.matches(key, "tui.select.confirm") {
            if self.selected_index == 0 {
                return OnboardingPickerAction::Continue;
            }
            let filtered = self.filtered();
            let Some(item) = filtered.get(self.selected_index - 1) else {
                return OnboardingPickerAction::None;
            };
            return OnboardingPickerAction::Select {
                id: item.id.clone(),
            };
        }
        if kb.matches(key, "tui.select.cancel") {
            return OnboardingPickerAction::Cancel;
        }
        self.search.handle_key(key, kb);
        self.selected_index = self.selected_index.min(self.filtered().len());
        self.scroll_top = 0;
        OnboardingPickerAction::None
    }

    /// The picker surface (TS `render`): the blank opener, the optional
    /// prompt, the search field, the pinned Continue row, the scrolling
    /// entries with their dim hint, and the optional note.
    pub fn render(&self, theme: &Theme, width: usize) -> Vec<Line> {
        let width = width.max(1);
        let filtered = self.filtered();
        let visible_rows = self.visible_rows();
        // TS `clampScroll`: the window never starts past the entries.
        let scroll_top = self
            .scroll_top
            .min(filtered.len().saturating_sub(visible_rows));
        let row_width = self.row_width(width);
        let wash = highlight_wash(theme);
        let mut lines: Vec<Line> = vec![blank_line(width)];
        if let Some(prompt) = self
            .config
            .prompt
            .as_deref()
            .filter(|text| !text.is_empty())
        {
            lines.push(surface_line(
                width,
                vec![theme.fg_span(ThemeColor::Text, prompt.to_string())],
            ));
            lines.push(blank_line(width));
        }
        lines.extend(search_field_plain_lines(
            theme,
            width,
            self.search.value(),
            self.search.cursor(),
            self.focused,
            self.config
                .search_placeholder
                .as_deref()
                .unwrap_or(DEFAULT_SEARCH_PLACEHOLDER),
        ));
        lines.push(blank_line(width));
        lines.push(render_row(
            theme,
            width,
            row_width,
            wash,
            &PickerRow {
                label: self.continue_label(),
                connected: false,
                selected: self.selected_index == 0,
            },
        ));
        let end = filtered.len().min(scroll_top.saturating_add(visible_rows));
        for index in scroll_top..end {
            let item = filtered[index];
            lines.push(render_row(
                theme,
                width,
                row_width,
                wash,
                &PickerRow {
                    label: &item.label,
                    connected: item.connected,
                    selected: self.selected_index == index + 1,
                },
            ));
        }
        let remaining = filtered.len() - end;
        if remaining > 0 || scroll_top > 0 {
            let hint = if remaining > 0 {
                format!("{remaining} more below")
            } else {
                "top of list".to_string()
            };
            lines.push(surface_line(
                width,
                vec![theme.fg_span(ThemeColor::Dim, format!("  {hint}"))],
            ));
        }
        if let Some(note) = self.config.note.as_deref().filter(|text| !text.is_empty()) {
            lines.push(blank_line(width));
            lines.push(surface_line(
                width,
                vec![theme.fg_span(ThemeColor::Dim, note.to_string())],
            ));
        }
        lines
    }

    /// The entries matching the trimmed, lowercased query over label and id
    /// (TS `getFiltered`; an empty query keeps every entry).
    fn filtered(&self) -> Vec<&OnboardingPickerItem> {
        let query = self.search.value().trim().to_lowercase();
        if query.is_empty() {
            return self.items.iter().collect();
        }
        self.items
            .iter()
            .filter(|item| {
                item.label.to_lowercase().contains(&query)
                    || item.id.to_lowercase().contains(&query)
            })
            .collect()
    }

    /// Move over the pinned row plus the filtered entries (TS `move`):
    /// clamped to `0..=filtered.len`, the window following the selection.
    fn move_selection(&mut self, delta: isize) {
        let filtered_len = self.filtered().len();
        let next = self.selected_index as isize + delta;
        if next < 0 || next > filtered_len as isize {
            return;
        }
        self.selected_index = next as usize;
        if next >= 1 {
            let item_index = (next - 1) as usize;
            let visible_rows = self.visible_rows();
            if item_index < self.scroll_top {
                self.scroll_top = item_index;
            } else if item_index >= self.scroll_top.saturating_add(visible_rows) {
                self.scroll_top = item_index + 1 - visible_rows;
            }
        }
    }

    /// The viewport height (TS `visibleRows ?? DEFAULT_VISIBLE_ROWS`).
    fn visible_rows(&self) -> usize {
        self.config.visible_rows.unwrap_or(DEFAULT_VISIBLE_ROWS)
    }

    /// The row band width (TS `getRowWidth`): the configured width, or the
    /// caret plus the longest label (mark included) plus the trailing
    /// budget; clamped to the pane.
    fn row_width(&self, width: usize) -> usize {
        let configured = self.config.row_width.unwrap_or_else(|| {
            let longest = self
                .items
                .iter()
                .map(|item| {
                    str_width(&item.label) + usize::from(item.connected) * CONNECTED_MARK_WIDTH
                })
                .max()
                .unwrap_or(0);
            MIN_ROW_WIDTH.max(MARKER_WIDTH + longest + ROW_TRAILING)
        });
        width.saturating_sub(1).max(1).min(configured)
    }

    /// The pinned action's label (TS `continueLabel ?? "Continue"`).
    fn continue_label(&self) -> &str {
        self.config
            .continue_label
            .as_deref()
            .unwrap_or(DEFAULT_CONTINUE_LABEL)
    }
}

/// One row's render inputs (TS `renderRow` arguments).
struct PickerRow<'a> {
    label: &'a str,
    connected: bool,
    selected: bool,
}

/// One row band (TS `renderRow`): the caret + label, the connected mark, and
/// the dim pad out to the band width; the selected row lifts off the canvas
/// — a bold name over the onboarding wash — while unselected rows read muted.
fn render_row(
    theme: &Theme,
    width: usize,
    row_width: usize,
    wash: Color,
    row: &PickerRow<'_>,
) -> Line {
    let caret = if row.selected { "> " } else { "  " };
    let name = format!("{caret}{}", row.label);
    // TS pads against the truncated name+mark, so the band never splits a
    // wide row unevenly.
    let content_width = str_width(&name) + usize::from(row.connected) * CONNECTED_MARK_WIDTH;
    let pad = " ".repeat(row_width.saturating_sub(content_width.min(row_width)));
    let mut content: Line = Vec::with_capacity(3);
    if row.selected {
        content.push(Span::styled(
            name,
            theme
                .fg_style(ThemeColor::Text)
                .add_modifier(Modifier::BOLD)
                .bg(wash),
        ));
    } else {
        content.push(theme.fg_span(ThemeColor::Muted, name));
    }
    if row.connected {
        let mark_style = if row.selected {
            theme.fg_style(ThemeColor::Success).bg(wash)
        } else {
            theme.fg_style(ThemeColor::Success)
        };
        content.push(Span::styled(CONNECTED_MARK, mark_style));
    }
    let pad_style = if row.selected {
        theme.fg_style(ThemeColor::Dim).bg(wash)
    } else {
        theme.fg_style(ThemeColor::Dim)
    };
    content.push(Span::styled(pad, pad_style));
    surface_line(width, content)
}

/// One content row (TS `line`): a one-column indent, truncated and padded to
/// the pane width with plain spaces.
fn surface_line(width: usize, content: Line) -> Line {
    let mut line: Line = Vec::with_capacity(content.len() + 2);
    line.push(Span::raw(" "));
    line.extend(content);
    let mut line = truncate_line(&line, width, "");
    let used = spans_width(&line);
    if used < width {
        line.push(Span::raw(" ".repeat(width - used)));
    }
    line
}

/// One blank row (TS `line(width, "")`): plain spaces across the pane.
fn blank_line(width: usize) -> Line {
    vec![Span::raw(" ".repeat(width))]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb() -> KeybindingsManager {
        KeybindingsManager::new()
    }

    fn theme() -> Theme {
        crate::theme::Theme::builtin("prime", crate::theme::ColorMode::TrueColor)
    }

    fn item(id: &str, label: &str, connected: bool) -> OnboardingPickerItem {
        OnboardingPickerItem {
            id: id.to_string(),
            label: label.to_string(),
            connected,
        }
    }

    fn providers() -> Vec<OnboardingPickerItem> {
        vec![
            item("anthropic", "Anthropic", false),
            item("openai", "OpenAI", true),
            item("xai", "xAI", false),
            item("google", "Google", false),
        ]
    }

    fn catalogue(count: usize) -> Vec<OnboardingPickerItem> {
        (0..count)
            .map(|index| item(&format!("p{index}"), &format!("Provider {index}"), false))
            .collect()
    }

    fn type_text(picker: &mut OnboardingPicker, text: &str) {
        for character in text.chars() {
            picker.handle_key(character.to_string().as_str(), &kb());
        }
    }

    fn text_of(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn the_query_filters_over_label_and_id_case_insensitively() {
        let mut items = providers();
        items.push(item("gcp", "Google Cloud", false));
        let mut picker = OnboardingPicker::new(items, OnboardingPickerConfig::default());
        assert_eq!(
            picker.filtered().len(),
            5,
            "an empty query shows every entry"
        );
        // Labels match case-insensitively.
        type_text(&mut picker, "OPEN");
        let filtered = picker.filtered();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "openai");
        // Backspace reopens the list.
        for _ in 0.."OPEN".len() {
            picker.handle_key("backspace", &kb());
        }
        assert_eq!(picker.filtered().len(), 5);
        // The id matches when the label does not.
        type_text(&mut picker, "gcp");
        let filtered = picker.filtered();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "gcp");
        // The query trims and lowercases before matching (TS getFiltered).
        let mut spaced = OnboardingPicker::new(providers(), OnboardingPickerConfig::default());
        type_text(&mut spaced, " ope");
        let filtered = spaced.filtered();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "openai");
    }

    #[test]
    fn selection_clamps_moves_and_follows_the_scroll_window() {
        let items = catalogue(8);
        let mut picker = OnboardingPicker::new(items, OnboardingPickerConfig::default());
        picker.handle_key("up", &kb());
        assert_eq!(picker.selected_index, 0, "up clamps at the pinned row");
        for _ in 0..10 {
            picker.handle_key("down", &kb());
        }
        assert_eq!(picker.selected_index, 8, "down clamps at filtered.len");
        // itemIndex 7 >= scrollTop 0 + 6 visible -> scrollTop 2.
        assert_eq!(picker.scroll_top, 2);
        let text = text_of(&picker.render(&theme(), 80));
        assert!(text.iter().any(|row| row.contains("Provider 7")));
        assert!(
            !text.iter().any(|row| row.contains("Provider 0")),
            "the window scrolled past the first entries"
        );
        // Walking back up drags the window once the selection exits it.
        for _ in 0..7 {
            picker.handle_key("up", &kb());
        }
        assert_eq!(picker.selected_index, 1);
        assert_eq!(picker.scroll_top, 0);
        // A typing press rewinds the viewport and clamps the selection into
        // the new filter (TS handleInput fall-through).
        type_text(&mut picker, "7");
        assert_eq!(picker.selected_index, 1);
        assert_eq!(picker.scroll_top, 0);
    }

    #[test]
    fn the_scroll_hints_read_below_and_top() {
        let items = catalogue(7);
        let mut picker = OnboardingPicker::new(items, OnboardingPickerConfig::default());
        let text = text_of(&picker.render(&theme(), 80));
        assert!(
            text.iter().any(|row| row.trim() == "1 more below"),
            "7 entries, 6 visible"
        );
        assert!(!text.iter().any(|row| row.contains("top of list")));
        for _ in 0..7 {
            picker.handle_key("down", &kb());
        }
        // itemIndex 6 >= 0 + 6 -> scrollTop 1: nothing remains below.
        let text = text_of(&picker.render(&theme(), 80));
        assert!(text.iter().any(|row| row.trim() == "top of list"));
        assert!(!text.iter().any(|row| row.contains("more below")));
    }

    #[test]
    fn the_pinned_continue_row_routes_first() {
        let mut picker = OnboardingPicker::new(providers(), OnboardingPickerConfig::default());
        assert_eq!(
            picker.handle_key("enter", &kb()),
            OnboardingPickerAction::Continue
        );
        picker.handle_key("down", &kb());
        assert_eq!(
            picker.handle_key("enter", &kb()),
            OnboardingPickerAction::Select {
                id: "anthropic".to_string()
            }
        );
        picker.handle_key("down", &kb());
        assert_eq!(
            picker.handle_key("enter", &kb()),
            OnboardingPickerAction::Select {
                id: "openai".to_string()
            }
        );
        // A label override renames the pinned row (TS continueLabel).
        let config = OnboardingPickerConfig {
            continue_label: Some("Done".to_string()),
            ..OnboardingPickerConfig::default()
        };
        let labeled = OnboardingPicker::new(providers(), config);
        let text = text_of(&labeled.render(&theme(), 80));
        // The fresh picker pins selection on Continue, so the caret shows.
        assert!(text.iter().any(|row| row.trim_end() == "> Done"));
    }

    #[test]
    fn enter_routes_by_selection_index_over_the_filter() {
        let mut picker = OnboardingPicker::new(providers(), OnboardingPickerConfig::default());
        type_text(&mut picker, "a");
        assert_eq!(picker.filtered().len(), 3, "anthropic, openai, xai match");
        // Index 0 still confirms the pinned row over the filter.
        assert_eq!(
            picker.handle_key("enter", &kb()),
            OnboardingPickerAction::Continue
        );
        picker.handle_key("down", &kb());
        assert_eq!(
            picker.handle_key("enter", &kb()),
            OnboardingPickerAction::Select {
                id: "anthropic".to_string()
            }
        );
        picker.handle_key("down", &kb());
        picker.handle_key("down", &kb());
        assert_eq!(
            picker.handle_key("enter", &kb()),
            OnboardingPickerAction::Select {
                id: "xai".to_string()
            }
        );
        // A filter that empties the list leaves only the pinned row.
        type_text(&mut picker, "zzz");
        assert!(picker.filtered().is_empty());
        assert_eq!(
            picker.handle_key("enter", &kb()),
            OnboardingPickerAction::Continue
        );
    }

    #[test]
    fn the_exit_keys_answer_before_cancel_and_the_search() {
        let mut picker = OnboardingPicker::new(providers(), OnboardingPickerConfig::default());
        // ctrl+c is both app.clear and tui.select.cancel: the exit wins.
        assert_eq!(
            picker.handle_key("ctrl+c", &kb()),
            OnboardingPickerAction::Exit
        );
        assert_eq!(
            picker.handle_key("ctrl+d", &kb()),
            OnboardingPickerAction::Exit
        );
        assert_eq!(
            picker.handle_key("escape", &kb()),
            OnboardingPickerAction::Cancel
        );
        assert_eq!(
            picker.search.value(),
            "",
            "the exit keys never reach the query"
        );
    }

    #[test]
    fn the_row_band_sizes_from_the_longest_label_and_the_pane() {
        let picker = OnboardingPicker::new(providers(), OnboardingPickerConfig::default());
        // Longest row: Anthropic (9) vs OpenAI + mark (6 + 3) -> floor 34.
        assert_eq!(picker.row_width(80), 34);
        let long = item("long", &"L".repeat(40), false);
        let picker = OnboardingPicker::new(vec![long], OnboardingPickerConfig::default());
        assert_eq!(picker.row_width(80), 48, "2 + 40 + 6");
        // The connected mark counts toward the longest row.
        let connected = item("c", &"L".repeat(38), true);
        let picker = OnboardingPicker::new(vec![connected], OnboardingPickerConfig::default());
        assert_eq!(picker.row_width(80), 49, "2 + 38 + 3 + 6");
        assert_eq!(picker.row_width(20), 19, "the pane clamps the band");
        assert_eq!(picker.row_width(0), 1);
        // An explicit rowWidth overrides the metrics, pane clamp intact.
        let config = OnboardingPickerConfig {
            row_width: Some(50),
            ..OnboardingPickerConfig::default()
        };
        let picker = OnboardingPicker::new(providers(), config);
        assert_eq!(picker.row_width(80), 50);
        assert_eq!(picker.row_width(40), 39);
    }

    #[test]
    fn the_selected_row_sits_on_the_wash_with_a_bold_name() {
        let theme = theme();
        let mut picker = OnboardingPicker::new(providers(), OnboardingPickerConfig::default());
        picker.handle_key("down", &kb());
        let lines = picker.render(&theme, 80);
        let wash = crate::onboarding::highlight_wash(&theme);
        // Layout: blank, field, blank, Continue, rows.
        let expected: Line = vec![
            Span::raw(" "),
            Span::styled(
                "> Anthropic".to_string(),
                theme
                    .fg_style(ThemeColor::Text)
                    .add_modifier(ratatui::style::Modifier::BOLD)
                    .bg(wash),
            ),
            Span::styled(" ".repeat(23), theme.fg_style(ThemeColor::Dim).bg(wash)),
            Span::raw(" ".repeat(80 - 35)),
        ];
        assert_eq!(lines[4], expected);
    }

    #[test]
    fn connected_rows_carry_the_success_mark_washed_when_selected() {
        let theme = theme();
        let picker = OnboardingPicker::new(providers(), OnboardingPickerConfig::default());
        let lines = picker.render(&theme, 80);
        // Unselected: muted name, success mark, dim pad, no wash.
        let expected: Line = vec![
            Span::raw(" "),
            theme.fg_span(ThemeColor::Muted, "  OpenAI".to_string()),
            theme.fg_span(ThemeColor::Success, "  \u{2713}".to_string()),
            theme.fg_span(ThemeColor::Dim, " ".repeat(23)),
            Span::raw(" ".repeat(80 - 35)),
        ];
        assert_eq!(lines[5], expected);
        // Selected: the mark rides inside the wash.
        let wash = crate::onboarding::highlight_wash(&theme);
        let mut picker = OnboardingPicker::new(providers(), OnboardingPickerConfig::default());
        picker.handle_key("down", &kb());
        picker.handle_key("down", &kb());
        let lines = picker.render(&theme, 80);
        let expected: Line = vec![
            Span::raw(" "),
            Span::styled(
                "> OpenAI".to_string(),
                theme
                    .fg_style(ThemeColor::Text)
                    .add_modifier(ratatui::style::Modifier::BOLD)
                    .bg(wash),
            ),
            Span::styled(
                "  \u{2713}".to_string(),
                theme.fg_style(ThemeColor::Success).bg(wash),
            ),
            Span::styled(" ".repeat(23), theme.fg_style(ThemeColor::Dim).bg(wash)),
            Span::raw(" ".repeat(80 - 35)),
        ];
        assert_eq!(lines[5], expected);
    }

    #[test]
    fn the_surface_lays_out_prompt_field_and_note() {
        let config = OnboardingPickerConfig {
            prompt: Some("Connect a provider".to_string()),
            note: Some("Connect later with /login.".to_string()),
            search_placeholder: Some("Find a provider".to_string()),
            ..OnboardingPickerConfig::default()
        };
        let picker = OnboardingPicker::new(providers(), config);
        let lines = picker.render(&theme(), 80);
        let text = text_of(&lines);
        assert_eq!(text[0], " ".repeat(80), "the surface opens on a blank row");
        assert!(text[1].starts_with(" Connect a provider"));
        assert!(
            text[3].trim() == "Find a provider",
            "the plain field, no rules"
        );
        assert!(!text[2].contains("> "), "no input prompt on the field");
        // Prompt set: the Continue row lands after the field's blank, and
        // the fresh picker keeps it selected.
        assert!(text[5].trim_end() == "> Continue");
        let note_index = text
            .iter()
            .position(|row| row.trim_end() == " Connect later with /login.")
            .expect("the note renders");
        assert_eq!(
            text_of(&lines)[note_index - 1].trim(),
            "",
            "a blank row separates the note"
        );
        // Empty strings read as unset (TS falsy options skip their rows).
        let config = OnboardingPickerConfig {
            prompt: Some(String::new()),
            note: Some(String::new()),
            ..OnboardingPickerConfig::default()
        };
        let picker = OnboardingPicker::new(providers(), config);
        let text = text_of(&picker.render(&theme(), 80));
        assert!(
            text[3].trim_end() == "> Continue",
            "no prompt shifts the rows"
        );
        assert_eq!(text.len(), 4 + 4, "no hint and no note rows follow");
    }

    #[test]
    fn the_field_edits_the_query_and_renders_it() {
        let theme = theme();
        let mut picker = OnboardingPicker::new(providers(), OnboardingPickerConfig::default());
        type_text(&mut picker, "op");
        assert_eq!(picker.search.value(), "op");
        let text = text_of(&picker.render(&theme, 80));
        assert!(text[1].trim_end() == " op", "the field shows the query");
        // set_focused lights the field's caret (TS focused forwarding): an
        // empty focused field reverses the first placeholder cell.
        let mut fresh = OnboardingPicker::new(providers(), OnboardingPickerConfig::default());
        fresh.set_focused(true);
        let field = &fresh.render(&theme, 80)[1];
        assert_eq!(field[1].content, "S");
        assert_eq!(
            field[1].style,
            ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::REVERSED)
        );
    }

    #[test]
    fn visible_rows_resize_the_viewport() {
        let items = catalogue(8);
        let config = OnboardingPickerConfig {
            visible_rows: Some(3),
            ..OnboardingPickerConfig::default()
        };
        let mut picker = OnboardingPicker::new(items, config);
        for _ in 0..4 {
            picker.handle_key("down", &kb());
        }
        // itemIndex 3 >= 0 + 3 -> scrollTop 1.
        assert_eq!(picker.scroll_top, 1);
        let text = text_of(&picker.render(&theme(), 80));
        assert!(text.iter().any(|row| row.contains("Provider 3")));
        assert!(!text.iter().any(|row| row.contains("Provider 0")));
        assert!(text.iter().any(|row| row.trim() == "4 more below"));
    }
}
