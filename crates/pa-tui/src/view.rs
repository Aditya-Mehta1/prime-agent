//! Interactive agent view: fullscreen chat frame composed like the TS
//! interactive mode — a pinned top bar, a scrollable transcript window
//! (splash, chat rows, loader), and a dock (prompt-context line, editor
//! surface, tray). The session loop folds events into the view; this module
//! owns row geometry and scroll behavior only.

use crate::chat::{
    render_assistant, render_loader, render_text_rows, render_user_block, ChatEntry,
    CompactionState, Detail, WorkingState,
};
use crate::chrome::{
    conversation_detail_status, render_prompt_context, render_splash, render_top_bar, render_tray,
    ChromeState,
};
use crate::editor::Editor;
use crate::session::TranscriptItem;
use crate::theme::{Theme, ThemeBg, ThemeColor};
use crate::width::str_width;
use crate::{Line, Span};
use ratatui::style::{Modifier, Style};

/// Minimum transcript rows when the dock would crowd them out
/// (TS `FULLSCREEN_MIN_TRANSCRIPT_ROWS`).
pub const FULLSCREEN_MIN_TRANSCRIPT_ROWS: usize = 3;

/// TS `getSpacingContent`: an assistant message's conversation-spacing
/// classification at the current detail level.
enum SpacingContent {
    Visible,
    ToolOnly,
    Hidden,
}

/// TS `isCompactAgentMessageNeighbor`: agent messages, tool calls, and
/// shell completions render flush against each other.
fn is_compact_neighbor(entry: &ChatEntry) -> bool {
    matches!(
        entry,
        ChatEntry::Tool(_) | ChatEntry::AgentMessage(_) | ChatEntry::ShellCompletion(_)
    )
}

pub struct AgentView {
    pub theme: Theme,
    /// The chat markdown fenced-code indent (`markdown.codeBlockIndent`,
    /// TS `getMarkdownThemeWithSettings`; default two spaces).
    pub code_block_indent: String,
    pub editor: Editor,
    pub chrome: ChromeState,
    pub chat: Vec<ChatEntry>,
    pub detail: Detail,
    pub working: Option<WorkingState>,
    /// A compaction run in flight (TS `autoCompactionLoader`): replaces the
    /// working loader from `compaction_start` to `compaction_end`.
    pub compaction: Option<CompactionState>,
    /// Animation frame for spinners and the working icon.
    pub pulse_frame: usize,
    /// When the current working loader started (elapsed label).
    pub working_since: Option<std::time::Instant>,
    /// An active provider auto-retry (replaces the working loader while
    /// the retry loop waits, TS `retryLoader`).
    pub retry: Option<crate::chat::RetryState>,
    /// The first-run onboarding pane (TS `runStartupOnboarding`): while
    /// set, it owns the whole frame.
    pub onboarding: Option<crate::onboarding::OnboardingScreen>,
    /// The `/model` inline picker (TS `ModelSelectorComponent` seam):
    /// while set, it owns the whole frame like the onboarding pane.
    pub model_picker: Option<crate::model_picker::ModelPicker>,
    /// The `/effort` inline picker (TS `ThinkingSelectorComponent` seam):
    /// while set, it owns the whole frame like the model picker.
    pub effort_picker: Option<crate::effort_picker::EffortPicker>,
    scroll_top: usize,
    following: bool,
    /// The transcript-tail offset of the last composed frame (TS
    /// `lastMaxScroll`): scroll deltas page from here, not from zero.
    last_max_scroll: usize,
    /// Rows of the terminal the editor should lay out against.
    terminal_rows: u16,
    /// Cursor cell within the last dock render: (dock row, column).
    dock_cursor: Option<(usize, usize)>,
    /// Window height of the last composed frame (cursor positioning).
    window_rows: usize,
    /// Plain text of the last frame's rows: OSC zone-marker emission only
    /// re-emits rows whose content changed (mirroring the TS renderer,
    /// which writes a row's marker sequences when it rewrites that row).
    osc_last_rows: Vec<String>,
    /// Rendered rows per chat entry (incremental layout): a frame re-renders
    /// only entries invalidated since the last frame; settled entries clone
    /// their cached rows instead of re-running markdown and code previews.
    /// A transcript-scale frame pays full layout cost once per entry, not
    /// once per draw.
    entry_layout: Vec<Option<Vec<Line>>>,
    /// The width the cached rows were laid out for.
    layout_width: usize,
    /// The conversation-detail mode the cached rows were laid out for.
    layout_detail: Detail,
    /// Row texts of the inline frame at the last main-screen flush (TS
    /// `exitFullscreen`'s inline repaint): the next flush diffs against
    /// this, so suspend/resume/exit cycles never duplicate the transcript
    /// in terminal scrollback.
    flushed_frame: Vec<String>,
}

impl AgentView {
    pub fn new(theme: Theme) -> Self {
        Self {
            theme,
            code_block_indent: "  ".to_string(),
            editor: Editor::new(),
            chrome: ChromeState::default(),
            chat: Vec::new(),
            detail: Detail::Overview,
            working: None,
            compaction: None,
            pulse_frame: 0,
            working_since: None,
            retry: None,
            onboarding: None,
            model_picker: None,
            effort_picker: None,
            scroll_top: 0,
            following: true,
            last_max_scroll: 0,
            terminal_rows: 24,
            dock_cursor: None,
            window_rows: 0,
            osc_last_rows: Vec::new(),
            entry_layout: Vec::new(),
            layout_width: 0,
            layout_detail: Detail::Overview,
            flushed_frame: Vec::new(),
        }
    }

    /// Zone-marker emission plan for a freshly composed frame: every marked
    /// row whose content changed since the last frame. The marker sequences
    /// are part of the row content (a row gaining or keeping its marker is a
    /// changed row, exactly like the TS renderer's per-row writes).
    pub fn take_osc_emissions(
        &mut self,
        frame: &[Line],
    ) -> Vec<(usize, crate::osc133::RowMarkers)> {
        let rows: Vec<String> = frame
            .iter()
            .map(|line| line.iter().map(|s| s.content.as_str()).collect())
            .collect();
        let plan = frame
            .iter()
            .enumerate()
            .filter_map(|(row, line)| {
                let markers = crate::osc133::row_markers(line);
                if !markers.start && !markers.end {
                    return None;
                }
                let changed = self
                    .osc_last_rows
                    .get(row)
                    .is_none_or(|prev| prev != &rows[row]);
                changed.then_some((row, markers))
            })
            .collect();
        self.osc_last_rows = rows;
        plan
    }

    pub fn set_terminal_rows(&mut self, rows: u16) {
        self.terminal_rows = rows;
    }

    /// Append one chat component (no cached layout yet: the next frame
    /// renders it and stores its rows).
    pub fn push_entry(&mut self, entry: ChatEntry) {
        self.chat.push(entry);
        self.entry_layout.push(None);
    }

    /// The number of chat entries (the status-row in-place update checks
    /// whether its own row is still the transcript's last entry).
    pub fn chat_len(&self) -> usize {
        self.chat.len()
    }

    /// Replace the text and tone of the status entry at `index` (TS
    /// `showStatus` updates its previous status row in place when nothing
    /// followed it). Returns `false` when the entry is not a status row.
    pub fn update_status_row(
        &mut self,
        index: usize,
        text: &str,
        kind: crate::chat::StatusKind,
    ) -> bool {
        let Some(ChatEntry::Status {
            text: slot,
            kind: kind_slot,
        }) = self.chat.get_mut(index)
        else {
            return false;
        };
        *slot = text.to_string();
        *kind_slot = kind;
        self.mark_entry_stale(index);
        true
    }

    /// Append a replay transcript item (mapped onto chat components).
    pub fn push(&mut self, item: TranscriptItem) {
        self.chat.push(item_to_entry(item));
        self.entry_layout.push(None);
    }

    /// Drop the whole transcript and its cached layout (a fresh snapshot
    /// rebuild re-renders every row).
    pub fn clear_chat(&mut self) {
        self.chat.clear();
        self.entry_layout.clear();
    }

    /// Mark one chat entry's cached rows stale: a mutation changed its
    /// content (streamed blocks, tool-card state, an attached error row),
    /// so the next frame lays it out again.
    pub fn mark_entry_stale(&mut self, index: usize) {
        if let Some(slot) = self.entry_layout.get_mut(index) {
            *slot = None;
        }
    }

    /// The conversation-detail label for the prompt-context row.
    fn detail_label(&self) -> String {
        let key = crate::keybindings::KeybindingsManager::new()
            .first_key("app.tools.expand")
            .map(|key| crate::keybindings::format_key_text(&key))
            .unwrap_or_default();
        conversation_detail_status(
            self.detail.tool_output_expanded(),
            self.detail.show_thinking(),
            &key,
        )
    }

    /// Scroll the transcript window (TS `FullscreenViewport.scrollBy`):
    /// a following view pages from the tail; scrolling up pauses following
    /// and reaching the bottom resumes it.
    pub fn scroll_by(&mut self, delta: isize) {
        let base = if self.following {
            self.last_max_scroll
        } else {
            self.scroll_top
        };
        self.scroll_top = (base as isize + delta).max(0) as usize;
        self.following = self.scroll_top >= self.last_max_scroll;
        if self.following {
            self.scroll_top = self.last_max_scroll;
        }
    }

    /// Jump to the transcript start (TS `scrollToTop`); an empty transcript
    /// keeps following.
    pub fn scroll_to_top(&mut self) {
        self.scroll_top = 0;
        self.following = self.last_max_scroll == 0;
    }

    /// Jump to the transcript end and resume following (TS
    /// `scrollToBottom`).
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_top = self.last_max_scroll;
        self.following = true;
    }

    /// Resume following (fresh attach, session switch).
    pub fn follow(&mut self) {
        self.following = true;
    }

    /// One page of the transcript window (TS `pageSize`: the window minus
    /// one row, at least one).
    pub fn page_size(&self) -> usize {
        self.window_rows.saturating_sub(1).max(1)
    }

    /// Whether the window pins the transcript tail.
    pub fn is_following(&self) -> bool {
        self.following
    }

    /// Scroll state of the last composed frame (TS `ScrollInfo`).
    pub fn scroll_info(&self) -> ScrollInfo {
        ScrollInfo {
            following: self.following,
            lines_above: self.scroll_top,
            lines_below: self.last_max_scroll.saturating_sub(self.scroll_top),
        }
    }

    /// Whether one chat entry's rows are stable: content that later frames
    /// cannot change (nothing mutates status/user/slash rows once pushed;
    /// an assistant message stops changing when its stream settles; a tool
    /// card stops animating once it holds a final result).
    fn entry_cacheable(&self, entry: &ChatEntry) -> bool {
        match entry {
            ChatEntry::Status { .. } | ChatEntry::User { .. } => true,
            ChatEntry::SlashCommand { .. } | ChatEntry::SlashCommandResult { .. } => true,
            ChatEntry::CompactionSummary { .. } => true,
            // Spacing-driven rows (agent messages, shell completions) lean
            // on the conversation-spacing scan over PRECEDING entries: a
            // streaming assistant's spacing contribution changes when its
            // stream settles, so they render fresh until every assistant
            // message in the transcript has settled (TS computes the
            // leading blank dynamically on every render).
            ChatEntry::AgentMessage(_) | ChatEntry::ShellCompletion(_) => !self
                .chat
                .iter()
                .any(|entry| matches!(entry, ChatEntry::Assistant(m) if m.streaming)),
            ChatEntry::InjectedPrompt(_) | ChatEntry::RefinementOutcome(_) => true,
            ChatEntry::CustomPanel(_) => true,
            ChatEntry::Assistant(message) => !message.streaming,
            ChatEntry::Tool(card) => !matches!(
                crate::tool_card::panel_status(card),
                crate::tool_card::PanelStatus::Queued | crate::tool_card::PanelStatus::Running
            ),
        }
    }

    /// TS `createConversationSpacing.shouldAddLeadingSpace` for one
    /// spacing-driven custom row (agent message, shell completion): scan
    /// back over entries that contribute no rows at this detail level
    /// (hidden thinking-only and tool-only assistant messages), then apply
    /// the trailing-space and compact-neighbor rules. `expanded` follows
    /// the TS `shouldAddLeadingSpace(expanded)` call shape.
    fn conversation_leading(&self, index: usize, expanded: bool) -> bool {
        let mut idx = index;
        let mut tool_separator = false;
        while idx > 0 {
            idx -= 1;
            match &self.chat[idx] {
                ChatEntry::Assistant(message) => {
                    match self.assistant_spacing_content(message) {
                        SpacingContent::Hidden => continue,
                        SpacingContent::ToolOnly => {
                            tool_separator = true;
                            continue;
                        }
                        SpacingContent::Visible => {
                            // TS `hasTrailingSpace` on the visible body.
                            let preceded_by_tool =
                                idx > 0 && matches!(&self.chat[idx - 1], ChatEntry::Tool(_));
                            if tool_separator
                                || message.has_trailing_space(self.detail, preceded_by_tool)
                            {
                                return false;
                            }
                            // An assistant message is never a compact
                            // neighbor; the collapsed and expanded rules
                            // both add the leading blank here.
                            return true;
                        }
                    }
                }
                preceding => {
                    if tool_separator && !is_compact_neighbor(preceding) {
                        return false;
                    }
                    if expanded {
                        return true;
                    }
                    return !is_compact_neighbor(preceding);
                }
            }
        }
        // The scan exhausted the transcript (only hidden or tool-only
        // assistant rows): TS keeps the tool separator with a trailing
        // space (no leading blank); with nothing preceding at all, the
        // expanded form sits flush against the top of the chat while the
        // collapsed form still leads with a blank
        // (`!isCompactAgentMessageNeighbor(undefined)`).
        if tool_separator {
            return false;
        }
        !expanded
    }

    /// TS `getSpacingContent`: an assistant message's contribution to
    /// conversation spacing at the current detail level.
    fn assistant_spacing_content(&self, message: &crate::chat::AssistantMessage) -> SpacingContent {
        let visible_body = message.blocks.iter().any(|block| match block {
            crate::chat::MessageBlock::Thinking(text) => {
                self.detail.show_thinking() && !text.trim().is_empty()
            }
            crate::chat::MessageBlock::Text(text) => !text.trim().is_empty(),
        });
        if visible_body || message.aborted || (message.error.is_some() && !message.has_tool_calls) {
            return SpacingContent::Visible;
        }
        if message.has_tool_calls {
            SpacingContent::ToolOnly
        } else {
            SpacingContent::Hidden
        }
    }

    /// Lay out one chat entry's transcript rows (the only producer of
    /// cached layout rows).
    fn render_entry(
        &self,
        index: usize,
        entry: &ChatEntry,
        width: usize,
        first: bool,
        preceded_by_tool_activity: bool,
    ) -> Vec<Line> {
        match entry {
            ChatEntry::Status { text, kind } => {
                let style = match kind {
                    crate::chat::StatusKind::Info => self.theme.fg_style(ThemeColor::Dim),
                    crate::chat::StatusKind::Warning => self.theme.fg_style(ThemeColor::Warning),
                    crate::chat::StatusKind::Error => self.theme.fg_style(ThemeColor::Error),
                };
                let mut rows = Vec::new();
                rows.push(Vec::new());
                rows.extend(render_text_rows(text, style, width));
                rows
            }
            ChatEntry::User { text } => {
                let mut rows = Vec::new();
                if !first {
                    rows.push(Vec::new());
                }
                rows.extend(render_user_block(
                    text,
                    &self.theme,
                    &self.code_block_indent,
                    width,
                ));
                rows
            }
            ChatEntry::SlashCommand { text } => {
                // The echo row leads with a spacer when the chat is not
                // empty (TS adds `Spacer(1)` before the component).
                let mut rows = Vec::new();
                if !first {
                    rows.push(Vec::new());
                }
                let typed = pa_types::slash_commands::parse_slash_command(text)
                    .map(|(name, _)| name)
                    .unwrap_or_default();
                let takes_argument = pa_types::slash_commands::SlashCommandRegistry::builtin()
                    .takes_argument(&typed);
                rows.extend(crate::chat_slash::render_slash_command(
                    text,
                    takes_argument,
                    &self.theme,
                    width,
                ));
                rows
            }
            ChatEntry::SlashCommandResult { content } => {
                crate::chat_slash::render_slash_command_result(content, &self.theme, width)
            }
            ChatEntry::CompactionSummary {
                summary,
                tokens_before,
                custom_instructions,
            } => {
                // TS `addMessageToChat` conversation spacing: the summary
                // follows the previous component with `Spacer(1)` when not
                // first (in the rebuilt transcript it trails the kept
                // tail's echo row).
                let mut rows = Vec::new();
                if !first {
                    rows.push(Vec::new());
                }
                rows.extend(crate::compaction_row::render_compaction_summary(
                    summary,
                    *tokens_before,
                    custom_instructions.as_deref(),
                    false,
                    &self.theme,
                    width,
                ));
                rows
            }
            ChatEntry::Assistant(message) => render_assistant(
                message,
                self.detail,
                &self.theme,
                &self.code_block_indent,
                width,
                preceded_by_tool_activity,
            ),
            ChatEntry::Tool(card) => crate::tool_card::render_tool_card(
                card,
                self.pulse_frame,
                self.detail,
                &self.theme,
                width,
            ),
            ChatEntry::AgentMessage(row) => crate::custom_message::render::render_agent_message(
                row,
                self.detail,
                &self.theme,
                width,
                self.conversation_leading(index, self.detail.tool_output_expanded()),
            ),
            ChatEntry::InjectedPrompt(row) => {
                crate::custom_message::render::render_injected_prompt(
                    row,
                    self.detail,
                    &self.theme,
                    width,
                )
            }
            ChatEntry::ShellCompletion(row) => {
                crate::custom_message::render::render_shell_completion(
                    row,
                    self.detail,
                    &self.theme,
                    width,
                    self.conversation_leading(index, self.detail.tool_output_expanded()),
                )
            }
            ChatEntry::RefinementOutcome(row) => {
                crate::custom_message::refinement::render_refinement_outcome(
                    row,
                    self.detail,
                    &self.theme,
                    width,
                )
            }
            ChatEntry::CustomPanel(row) => {
                crate::custom_message::render::render_custom_panel(row, &self.theme, width)
            }
        }
    }

    /// Render the scrollable transcript: splash rows, chat component rows,
    /// and the working loader when a turn is active.
    pub fn render_transcript(&mut self, width: usize) -> Vec<Line> {
        if let (Some(working), Some(since)) = (&mut self.working, self.working_since) {
            working.elapsed_secs = since.elapsed().as_secs();
        }
        // A width or detail change re-flows every row: drop the whole
        // layout cache (the flags below gate every entry's stored rows).
        if self.layout_width != width || self.layout_detail != self.detail {
            self.layout_width = width;
            self.layout_detail = self.detail;
            self.entry_layout.iter_mut().for_each(|slot| *slot = None);
        }
        self.entry_layout.resize(self.chat.len(), None);
        let mut lines: Vec<Line> = render_splash(&self.chrome, &self.theme, width);
        let mut first = true;
        let mut preceded_by_tool_activity = false;
        for (index, entry) in self.chat.iter().enumerate() {
            // Incremental layout: settled entries re-use their stored
            // rows; anything still animating (streaming messages, queued
            // or running tool cards) renders fresh and stores nothing.
            let rows = match self.entry_layout[index]
                .as_ref()
                .filter(|_| self.entry_cacheable(entry))
            {
                Some(rows) => rows.clone(),
                None => {
                    let rows =
                        self.render_entry(index, entry, width, first, preceded_by_tool_activity);
                    if self.entry_cacheable(entry) {
                        self.entry_layout[index] = Some(rows.clone());
                    }
                    rows
                }
            };
            lines.extend(rows);
            preceded_by_tool_activity = matches!(entry, ChatEntry::Tool(_));
            first = false;
        }
        // While the provider retry loop waits, its countdown loader owns
        // the status area (TS `stopWorkingLoader` + `retryLoader`); a
        // compaction run owns it next (TS `startCompactionLoader`); the
        // working loader renders only when neither is active.
        if let Some(retry) = &self.retry {
            lines.extend(crate::chat::render_retry(
                retry,
                self.pulse_frame,
                &self.theme,
                width,
            ));
        } else if let Some(compaction) = &self.compaction {
            let cancel_hint = self
                .editor
                .keybindings()
                .first_key("app.clear")
                .map(|key| crate::keybindings::format_key_text(&key))
                .unwrap_or_else(|| "Ctrl+C".to_string());
            lines.extend(crate::compaction_row::render_compaction_loader(
                compaction,
                self.pulse_frame,
                &cancel_hint,
                &self.theme,
                width,
            ));
        } else if let Some(working) = &self.working {
            lines.extend(render_loader(working, self.pulse_frame, &self.theme, width));
        }
        lines
    }

    /// Render the dock: prompt-context row(s), the autocomplete overlay
    /// (when showing), the editor surface, the tray.
    pub fn render_dock(&mut self, width: usize) -> Vec<Line> {
        let mut lines = render_prompt_context(&self.detail_label(), &self.theme, width);
        let context_rows = lines.len();
        let overlay_rows = self.render_autocomplete_overlay(width);
        lines.extend(overlay_rows);
        let (editor_rows, cursor) = self.render_editor_surface(width);
        let overlay_count = lines.len() - context_rows;
        self.dock_cursor = cursor.map(|(row, col)| (context_rows + overlay_count + row, col));
        lines.extend(editor_rows);
        lines.push(render_tray(&self.chrome, &self.theme, width));
        lines
    }

    /// The autocomplete dropdown, mounted just above the editor surface (TS
    /// anchors the overlay immediately above the cursor row; the editor's
    /// first content row carries the cursor in the common single-line
    /// case). Each row pads to the input width and floats on the popup
    /// background between the editor's left padding and prompt prefix.
    fn render_autocomplete_overlay(&mut self, width: usize) -> Vec<Line> {
        let Some(state) = self.editor.autocomplete_state() else {
            return Vec::new();
        };
        let styles = crate::autocomplete::SelectListStyles {
            selected_prefix: self.theme.fg_style(ThemeColor::Accent),
            selected_text: self.theme.fg_style(ThemeColor::Accent),
            description: self.theme.fg_style(ThemeColor::Muted),
            argument_hint: self.theme.fg_style(ThemeColor::MdCode),
            scroll_info: self.theme.fg_style(ThemeColor::Muted),
            no_match: self.theme.fg_style(ThemeColor::Muted),
        };
        let bg = self.theme.bg_style(ThemeBg::ToolPanelBg);
        let padding_x = 2usize;
        let prompt_width = str_width("> ");
        let content_width = width.saturating_sub(padding_x * 2).max(1);
        let input_width = content_width.saturating_sub(prompt_width).max(1);
        let mut rows: Vec<Line> = Vec::new();
        let mut overlay = Vec::new();
        overlay.push(Vec::new());
        overlay.extend(state.render(input_width, &styles));
        overlay.push(Vec::new());
        for line in overlay {
            let used: usize = line.iter().map(|s| str_width(&s.content)).sum();
            let mut row: Line = vec![Span::styled(" ".repeat(padding_x + prompt_width), bg)];
            row.extend(line);
            row.push(Span::styled(
                " ".repeat(input_width.saturating_sub(used)),
                bg,
            ));
            row.push(Span::styled(" ".repeat(padding_x), bg));
            rows.push(pad_row(row, width));
        }
        rows
    }

    /// The editor surface (TS `Editor.render` with a background): a blank
    /// bg row, content rows with the `> ` prompt and a reverse-video cursor,
    /// and a trailing bg row. Scroll indicators replace the blank rows.
    fn render_editor_surface(&mut self, width: usize) -> (Vec<Line>, Option<(usize, usize)>) {
        let bg = crate::chrome::editor_background(&self.theme);
        let border = self.theme.fg_style(ThemeColor::BorderMuted);
        let padding_x = 2usize;
        let content_width = width.saturating_sub(padding_x * 2).max(1);
        let prompt = "> ";
        let prompt_width = str_width(prompt);
        let input_width = content_width.saturating_sub(prompt_width).max(1);
        let layout_width = input_width;
        let (visible, scroll_offset, _hidden_above, hidden_below) =
            self.editor.visible_window(layout_width, self.terminal_rows);
        let mut rows: Vec<Line> = Vec::new();
        if scroll_offset > 0 {
            let indicator = format!(" \u{2191} {scroll_offset} more");
            rows.push(indicator_row(&indicator, bg, border, width));
        } else {
            rows.push(vec![Span::styled(" ".repeat(width), bg)]);
        }
        let mut cursor: Option<(usize, usize)> = None;
        for (index, line) in visible.iter().enumerate() {
            let mut row: Line = vec![Span::styled(" ".to_string(), bg)];
            // The `> ` prompt prefix renders plain on the surface background
            // (TS `formatPromptPrefix` styles only `!` bash prompts).
            if index == 0 {
                row.push(Span::styled(prompt.to_string(), bg));
            } else {
                row.push(Span::styled(" ".repeat(prompt_width), bg));
            }
            row.push(Span::styled(" ".to_string(), bg));
            let text: &str = &line.text;
            let before = line.cursor_pos.min(text.chars().count()).to_string();
            let _ = before;
            let (head, tail) = split_at_chars(text, line.cursor_pos.min(text.chars().count()));
            let mut used = str_width(text);
            if line.has_cursor {
                if tail.is_empty() {
                    row.push(Span::styled(head.to_string(), bg));
                    row.push(Span::styled(
                        " ".to_string(),
                        bg.add_modifier(Modifier::REVERSED),
                    ));
                    used += 1;
                } else {
                    let first = tail.chars().next().unwrap_or(' ');
                    let rest: String = tail[first.len_utf8()..].to_string();
                    row.push(Span::styled(head.to_string(), bg));
                    row.push(Span::styled(
                        first.to_string(),
                        bg.add_modifier(Modifier::REVERSED),
                    ));
                    row.push(Span::styled(rest, bg));
                }
                cursor = Some((index + 1, str_width(head) + 4));
            } else {
                row.push(Span::styled(text.to_string(), bg));
            }
            row.push(Span::styled(
                " ".repeat(input_width.saturating_sub(used)),
                bg,
            ));
            row.push(Span::styled(" ".repeat(padding_x), bg));
            rows.push(row);
        }
        if hidden_below > 0 {
            rows.push(indicator_row(
                &format!(" \u{2193} {hidden_below} more"),
                bg,
                border,
                width,
            ));
        } else {
            rows.push(vec![Span::styled(" ".repeat(width), bg)]);
        }
        (rows, cursor)
    }

    /// Compose the fullscreen frame: top bar, transcript window (padded),
    /// dock at the bottom — exactly `height` rows.
    pub fn render_frame(&mut self, width: usize, height: usize) -> Vec<Line> {
        // The onboarding splash covers the pane (TS `showOverlay` 100%):
        // no top bar, transcript, or prompt dock behind it.
        if let Some(screen) = &self.onboarding {
            return screen.render(&self.theme, width, height);
        }
        if let Some(picker) = &self.model_picker {
            return picker_pane(picker.render(&self.theme, width), width, height);
        }
        if let Some(picker) = &self.effort_picker {
            return picker_pane(picker.render(&self.theme, width), width, height);
        }
        let top = render_top_bar(&self.chrome, &self.theme, width);
        let transcript = self.render_transcript(width);
        let dock = self.render_dock(width);
        let dock_height = dock
            .len()
            .min(height.saturating_sub(FULLSCREEN_MIN_TRANSCRIPT_ROWS));
        let dock: Vec<Line> = if dock.len() > dock_height {
            dock[dock.len() - dock_height..].to_vec()
        } else {
            dock
        };
        let window_height = height
            .saturating_sub(1 + dock.len())
            .max(FULLSCREEN_MIN_TRANSCRIPT_ROWS.min(height.saturating_sub(1 + dock.len())));
        let max_scroll = transcript.len().saturating_sub(window_height);
        if self.following {
            self.scroll_top = max_scroll;
        } else {
            self.scroll_top = self.scroll_top.min(max_scroll);
        }
        self.last_max_scroll = max_scroll;
        let start = self.scroll_top.min(max_scroll);
        self.window_rows = window_height;
        let mut frame: Vec<Line> = Vec::with_capacity(height);
        frame.push(pad_row(top, width));
        for line in &transcript[start..(start + window_height).min(transcript.len())] {
            frame.push(pad_row(line.clone(), width));
        }
        while frame.len() < height.saturating_sub(dock.len()) {
            frame.push(vec![Span::raw(" ".repeat(width))]);
        }
        for line in dock {
            frame.push(pad_row(line, width));
        }
        // A paused viewport carries the follow hint over the last transcript
        // window row (TS composites it above the dock, below overlays).
        if !self.following {
            if let Some(row) = frame.get_mut(window_height) {
                let key = crate::keybindings::KeybindingsManager::new()
                    .first_key("tui.viewport.follow")
                    .unwrap_or_else(|| "ctrl+shift+down".to_string());
                let label = format!(" {key} to follow ");
                *row = composite_follow_hint(row, &label, width);
            }
        }
        frame
    }

    /// Hardware cursor position within the last composed frame (0-based row,
    /// 0-based column), when the editor surface drew the cursor.
    pub fn frame_cursor(&self) -> Option<(usize, usize)> {
        if self.onboarding.is_some() || self.model_picker.is_some() || self.effort_picker.is_some()
        {
            return None;
        }
        self.dock_cursor
            .map(|(row, col)| (row + 1 + self.window_rows, col))
    }

    /// The inline layout the exit flush paints onto the main screen (TS
    /// `exitFullscreen`'s synchronous inline repaint): the full transcript
    /// plus the dock, without the fullscreen window, top-bar pin, or height
    /// padding. Unlike an alt-screen frame, these rows persist in the
    /// terminal's native scrollback, which is what keeps the exit frame
    /// (and the resume hint printed below it) visible after the app exits.
    pub fn render_inline_frame(&mut self, width: usize) -> Vec<Line> {
        let mut rows = self.render_transcript(width);
        rows.extend(self.render_dock(width));
        rows
    }

    /// Rows of the inline layout that changed since the last main-screen
    /// flush, as a write plan for the flush primitive (TS
    /// `exitFullscreen`'s inline repaint):
    ///
    /// - [`FlushPlan::Append`] when the flushed frame is a prefix of the
    ///   new one (or nothing was flushed yet): the new tail appends below
    ///   the cursor and flows into native scrollback — this is the exit
    ///   path that keeps the exit frame and resume hint visible.
    /// - [`FlushPlan::Repaint`] when rows above the flushed tail changed
    ///   (a transcript that grew past a suspend-time flush, a snapshot
    ///   rebuild): the visible screen is erased and the last screenful
    ///   repainted, mirroring the TS full redraw. Scrollback above the
    ///   screen is never rewritten — terminal scrollback is immutable,
    ///   the same trade-off the TS renderer makes.
    pub fn take_flush_plan(&mut self, width: usize, screen_height: usize) -> FlushPlan {
        let rows = self.render_inline_frame(width);
        let texts: Vec<String> = rows.iter().map(row_text_of).collect();
        let first_changed = (0..self.flushed_frame.len().max(texts.len())).find(|&index| {
            let old = self.flushed_frame.get(index).map(String::as_str);
            let new = texts.get(index).map(String::as_str);
            old != new
        });
        let plan = match first_changed {
            // Identical frame: nothing to write.
            None => FlushPlan::Append(Vec::new()),
            // The flushed frame is a prefix: append the new tail.
            Some(index) if index >= self.flushed_frame.len() => {
                FlushPlan::Append(rows[index.min(rows.len())..].to_vec())
            }
            // Rows above the flushed tail changed: repaint the visible
            // window (the frame tail), leaving scrollback untouched.
            Some(_) => {
                let start = rows.len().saturating_sub(screen_height);
                FlushPlan::Repaint(rows[start..].to_vec())
            }
        };
        self.flushed_frame = texts;
        plan
    }
}

/// The main-screen write plan produced by [`AgentView::take_flush_plan`].
#[derive(Debug, PartialEq, Eq)]
pub enum FlushPlan {
    /// Write the rows below the cursor (joined with newlines), scrolling
    /// excess rows into native scrollback.
    Append(Vec<Line>),
    /// Erase the visible screen (scrollback above it stays) and paint the
    /// rows from the top — the TS full-redraw path for changes above the
    /// flushed tail.
    Repaint(Vec<Line>),
}

/// Concatenated span contents of a row (includes zero-width OSC zone
/// markers, which must persist into scrollback).
fn row_text_of(line: &Line) -> String {
    line.iter().map(|span| span.content.as_str()).collect()
}

/// Split a string at a char boundary.
fn split_at_chars(text: &str, at: usize) -> (&str, &str) {
    let mut end = text.len();
    let mut count = 0;
    for (index, _) in text.char_indices() {
        if count == at {
            end = index;
            break;
        }
        count += 1;
    }
    if count < at {
        return (text, "");
    }
    (&text[..end], &text[end..])
}

/// One scroll-indicator surface row (`↑ N more` on the editor background).
fn indicator_row(indicator: &str, bg: Style, border: Style, width: usize) -> Line {
    let mut row: Line = vec![Span::styled(indicator.to_string(), border)];
    let used = str_width(indicator);
    row.push(Span::styled(" ".repeat(width.saturating_sub(used)), bg));
    row
}

/// Fit the model picker's frame to the window (same geometry as the
/// full-screen selector loop: pad to height, truncate at height).
fn picker_pane(mut frame: Vec<Line>, width: usize, height: usize) -> Vec<Line> {
    while frame.len() < height {
        frame.push(vec![Span::raw(" ".repeat(width.max(1)))]);
    }
    frame.truncate(height);
    frame
}

/// Pad a rendered row to the full width (default background).
fn pad_row(line: Line, width: usize) -> Line {
    let used: usize = line.iter().map(|s| str_width(&s.content)).sum();
    let mut out = line;
    if used < width {
        out.push(Span::raw(" ".repeat(width - used)));
    }
    out
}

/// Scroll state of the transcript window (TS `ScrollInfo`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollInfo {
    pub following: bool,
    pub lines_above: usize,
    pub lines_below: usize,
}

/// Composite the follow hint over one frame row (TS `renderFullscreen`:
/// `ctrl+shift+down to follow` reversed, centered, over the last transcript
/// window row). Leading OSC-133 zone markers stay at the row head so the
/// marker plan keeps flagging the row.
fn composite_follow_hint(row: &Line, label: &str, width: usize) -> Line {
    let label_width = str_width(label);
    let (markers, rest) = crate::osc133::split_leading_markers(row);
    let col = width.saturating_sub(label_width) / 2;
    let mut out: Line = markers;
    out.extend(crate::width::slice_line_by_column(&rest, 0, col));
    out.push(Span::styled(
        label.to_string(),
        Style::default().add_modifier(Modifier::REVERSED),
    ));
    out.extend(crate::width::slice_line_by_column(
        &rest,
        col.saturating_add(label_width),
        width,
    ));
    out
}

/// Map a replay transcript item onto a chat component.
fn item_to_entry(item: TranscriptItem) -> ChatEntry {
    match item {
        TranscriptItem::UserMessage { text } => ChatEntry::User { text },
        TranscriptItem::SystemNote { text } => ChatEntry::Status {
            text,
            kind: crate::chat::StatusKind::Info,
        },
        TranscriptItem::Assistant { text } => {
            ChatEntry::Assistant(Box::new(crate::chat::AssistantMessage {
                blocks: vec![crate::chat::MessageBlock::Text(text)],
                has_tool_calls: false,
                streaming: false,
                error: None,
                aborted: false,
            }))
        }
        TranscriptItem::ToolCall {
            id,
            name,
            arguments,
        } => ChatEntry::Tool(Box::new(crate::chat::ToolCallCard {
            id,
            name,
            args: serde_json::from_str(&arguments).unwrap_or(serde_json::Value::Null),
            started: false,
            ..Default::default()
        })),
        TranscriptItem::ToolResult {
            tool_call_id,
            tool_name,
            text,
        } => ChatEntry::Tool(Box::new(crate::chat::ToolCallCard {
            id: tool_call_id,
            name: tool_name,
            args: serde_json::Value::Null,
            started: true,
            result: Some(crate::chat::ToolResultView {
                content: vec![serde_json::json!({ "type": "text", "text": text })],
                ..Default::default()
            }),
            ..Default::default()
        })),
        TranscriptItem::BashExecution {
            command, exit_code, ..
        } => ChatEntry::Tool(Box::new(crate::chat::ToolCallCard {
            id: String::new(),
            name: "bash".to_string(),
            args: serde_json::json!({ "command": command, "exitCode": exit_code }),
            started: true,
            ..Default::default()
        })),
        TranscriptItem::AgentStatus { summary, .. } => ChatEntry::Status {
            text: summary,
            kind: crate::chat::StatusKind::Info,
        },
        TranscriptItem::ModelChange { model_id, .. } => ChatEntry::Status {
            text: format!("\u{2699} {model_id}"),
            kind: crate::chat::StatusKind::Info,
        },
        TranscriptItem::CustomRow { entry } => entry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{AssistantMessage, MessageBlock};
    use crate::theme::{ColorMode, Theme};
    use crate::tool_card::{ToolCallCard, ToolResultView};

    fn view() -> AgentView {
        AgentView::new(Theme::builtin("prime", ColorMode::TrueColor))
    }

    fn text_of(line: &Line) -> String {
        line.iter().map(|s| s.content.as_str()).collect::<String>()
    }

    #[test]
    fn frame_is_exactly_height_rows() {
        let mut v = view();
        v.chrome.version = "0.0.0".to_string();
        v.chrome.cwd = "/tmp/project".to_string();
        v.chrome.chat_name = "project".to_string();
        let frame = v.render_frame(80, 24);
        assert_eq!(frame.len(), 24);
        assert!(frame.iter().all(|l| str_width(&text_of(l)) <= 80));
        let joined = frame.iter().map(text_of).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("prime agent v0.0.0"));
        assert!(joined.contains("Collapsed mode (Ctrl+O to expand)"));
        assert!(joined.contains(">"));
    }

    #[test]
    fn osc_emissions_reemit_only_changed_rows() {
        let mut v = view();
        v.chrome.version = "0.0.0".to_string();
        v.chrome.cwd = "/w".to_string();
        v.chrome.chat_name = "w".to_string();
        v.push(TranscriptItem::UserMessage {
            text: "hello".to_string(),
        });
        let frame = v.render_frame(80, 24);
        let first = v.take_osc_emissions(&frame);
        let marked: Vec<usize> = first.iter().map(|(row, _)| *row).collect();
        assert!(!marked.is_empty());
        // Re-emitting an unchanged frame rewrites no rows.
        let again = v.take_osc_emissions(&frame);
        assert!(again.is_empty());
    }

    /// Fill the transcript past one window so there is scrollable history.
    fn filled(mut v: AgentView, turns: usize) -> AgentView {
        v.chrome.version = "0.0.0".to_string();
        v.chrome.cwd = "/w".to_string();
        v.chrome.chat_name = "w".to_string();
        v.onboarding = None;
        for index in 0..turns {
            v.push(TranscriptItem::UserMessage {
                text: format!("user line {index}"),
            });
            v.push(TranscriptItem::Assistant {
                text: format!("assistant reply {index}"),
            });
        }
        v
    }

    fn row_text(line: &Line) -> String {
        line.iter().map(|s| s.content.as_str()).collect::<String>()
    }

    #[test]
    fn scroll_pages_from_tail_and_resumes_at_bottom() {
        let mut v = filled(view(), 30);
        let frame = v.render_frame(80, 24);
        // Fresh render follows the tail.
        assert!(v.is_following());
        let info = v.scroll_info();
        assert_eq!(info.lines_below, 0);
        assert!(frame.iter().any(|l| row_text(l).contains("reply 29")));

        // PageUp pauses following and moves the window up a page: `page`
        // rows remain below the window (`ScrollInfo` reports the tail
        // distance in `lines_below`, the transcript-top offset in
        // `lines_above`).
        let page = v.page_size();
        v.scroll_by(-(page as isize));
        assert!(!v.is_following());
        assert_eq!(v.scroll_info().lines_below, page);

        // Scrolling back down reaches the tail and resumes following.
        v.scroll_by(page as isize);
        assert!(v.is_following());
        assert_eq!(v.scroll_info().lines_below, 0);
    }

    #[test]
    fn scroll_offset_is_visible_in_frames() {
        let mut v = filled(view(), 30);
        let following = v.render_frame(80, 24);
        v.scroll_to_top();
        let top = v.render_frame(80, 24);
        // The top frame shows the earliest history the following frame
        // scrolled past: distinct window content for the same transcript.
        assert!(top.iter().any(|l| row_text(l).contains("reply 0")));
        assert!(!following.iter().any(|l| row_text(l).contains("reply 0")));
        assert!(following.iter().any(|l| row_text(l).contains("reply 29")));
        assert!(!top.iter().any(|l| row_text(l).contains("reply 29")));
    }

    #[test]
    fn compaction_loader_replaces_the_working_loader() {
        // TS `startCompactionLoader`: the compaction loader owns the status
        // area while a compaction runs, working loader hidden.
        let mut v = view();
        v.working = Some(WorkingState {
            activity: "Waiting",
            download: false,
            tokens: 0,
            elapsed_secs: 0,
        });
        v.compaction = Some(crate::chat::CompactionState {
            reason: crate::chat::CompactionReason::Manual,
            custom_instructions: None,
        });
        let frame = v.render_frame(80, 24);
        let flat: Vec<String> = frame.iter().map(row_text).collect();
        assert!(
            flat.iter()
                .any(|l| l.contains("Compacting context... (Ctrl+C to cancel)")),
            "{flat:?}"
        );
        assert!(
            !flat.iter().any(|l| l.contains("Waiting")),
            "the working loader is hidden during compaction: {flat:?}"
        );
        // `compaction_end` clears it; the summary row renders from the
        // transcript entry.
        v.compaction = None;
        v.push_entry(crate::chat::ChatEntry::CompactionSummary {
            summary: "the story so far".to_string(),
            tokens_before: 1234,
            custom_instructions: None,
        });
        let frame = v.render_frame(80, 24);
        let flat: Vec<String> = frame.iter().map(row_text).collect();
        assert!(
            flat.iter()
                .any(|l| l.trim() == "\u{25c6} Context compacted"),
            "{flat:?}"
        );
        assert!(
            flat.iter().any(|l| l.trim() == "the story so far"),
            "{flat:?}"
        );
    }

    #[test]
    fn follow_hint_shows_when_paused_and_hides_when_following() {
        let mut v = filled(view(), 30);
        let following_frame = v.render_frame(80, 24);
        assert!(!following_frame
            .iter()
            .any(|l| row_text(l).contains("to follow")));
        v.scroll_by(-(v.page_size() as isize));
        let paused_frame = v.render_frame(80, 24);
        assert!(paused_frame
            .iter()
            .any(|l| row_text(l).contains("ctrl+shift+down to follow")));
        // The follow key resumes: the hint disappears.
        v.scroll_to_bottom();
        let resumed_frame = v.render_frame(80, 24);
        assert!(v.is_following());
        assert!(!resumed_frame
            .iter()
            .any(|l| row_text(l).contains("to follow")));
        // scrollToTop pins the top; the hint shows again (TS shows it for
        // every non-following window, even at the very top).
        v.scroll_to_top();
        let top_frame = v.render_frame(80, 24);
        assert!(!v.is_following());
        assert!(top_frame.iter().any(|l| row_text(l).contains("to follow")));
    }

    #[test]
    fn follow_hint_keeps_zone_markers_on_the_composited_row() {
        // A marked row composited with the hint keeps its zone flags at
        // the head (the marker plan keeps flagging the row) and keeps the
        // visible text around the centered label.
        let mut row = vec![crate::Span::raw("x".repeat(80))];
        crate::osc133::mark_end(&mut row);
        let mut out = composite_follow_hint(&row, " ctrl+shift+down to follow ", 80);
        let markers = crate::osc133::row_markers(&out);
        assert!(markers.end && !markers.start);
        assert!(row_text(&out).contains("to follow"));
        assert_eq!(str_width(&row_text(&out)), 80);
        // Stripping the markers leaves the hint visible.
        crate::osc133::strip(&mut out);
        assert!(row_text(&out).contains("to follow"));
        // An unmarked row stays unmarked.
        let plain = vec![crate::Span::raw(" ".repeat(80))];
        let out = composite_follow_hint(&plain, " ctrl+shift+down to follow ", 80);
        assert_eq!(crate::osc133::row_markers(&out), Default::default());
    }

    #[test]
    fn flush_plan_appends_then_repaints_the_changed_tail() {
        let mut v = view();
        v.chrome.version = "0.0.0".to_string();
        v.chrome.cwd = "/w".to_string();
        v.chrome.chat_name = "w".to_string();
        v.push(TranscriptItem::UserMessage {
            text: "first turn".to_string(),
        });
        // The first flush appends the whole inline frame (splash,
        // transcript, dock) and keeps the zero-width zone markers embedded
        // in the rows — they must survive into scrollback for
        // shell-integration jumps.
        let first = v.take_flush_plan(80, 24);
        let FlushPlan::Append(rows) = &first else {
            panic!("first flush must append");
        };
        let joined = rows.iter().map(text_of).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("prime agent v0.0.0"));
        assert!(joined.contains("first turn"));
        assert!(rows.iter().any(|l| crate::osc133::row_markers(l).start));

        // An unchanged frame flushes nothing.
        assert_eq!(v.take_flush_plan(80, 24), FlushPlan::Append(Vec::new()));

        // New transcript rows land ABOVE the flushed dock, so the flush
        // repaints the visible window: the changed region is rewritten, not
        // appended below the stale dock (which would duplicate it).
        v.push(TranscriptItem::UserMessage {
            text: "second turn".to_string(),
        });
        let FlushPlan::Repaint(rows) = v.take_flush_plan(80, 24) else {
            panic!("growth past the flushed dock must repaint");
        };
        let joined = rows.iter().map(text_of).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("second turn"));
        assert!(joined.contains("first turn"));
        // The repaint covers at most one screenful: a long transcript
        // repaints only the tail.
        let mut long = filled(view(), 30);
        let FlushPlan::Append(_) = long.take_flush_plan(80, 10) else {
            panic!("first flush of a long transcript must append");
        };
        long.push(TranscriptItem::UserMessage {
            text: "late turn".to_string(),
        });
        let FlushPlan::Repaint(rows) = long.take_flush_plan(80, 10) else {
            panic!("growth past the flushed dock must repaint");
        };
        assert!(rows.len() <= 10);
        let joined = rows.iter().map(text_of).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("late turn"));
        assert!(!joined.contains("reply 0"));

        // A shrinking rebuild never rewinds into a rewrite of scrollback:
        // the changed region repaints the visible window only.
        v.clear_chat();
        let FlushPlan::Repaint(rows) = v.take_flush_plan(80, 24) else {
            panic!("a rebuild past the flushed frame must repaint");
        };
        let joined = rows.iter().map(text_of).collect::<Vec<_>>().join("\n");
        assert!(!joined.contains("second turn"));
    }

    #[test]
    fn inline_frame_is_transcript_plus_dock_without_padding() {
        let mut v = filled(view(), 30);
        // The inline layout is the unpinned frame: every transcript row is
        // present (no window slicing) and no height padding rows follow.
        let frame = v.render_frame(80, 24);
        let inline = v.render_inline_frame(80);
        assert!(inline.iter().any(|l| text_of(l).contains("reply 0")));
        assert!(inline.iter().any(|l| text_of(l).contains("reply 29")));
        assert!(frame.len() == 24 && inline.len() != frame.len());
        // The dock rows ride at the end (prompt context, editor, tray).
        let joined = inline.iter().map(text_of).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("Collapsed mode"));
    }

    #[test]
    fn dock_pads_window_between_splash_and_editor() {
        let mut v = view();
        v.chrome.version = "0.0.0".to_string();
        v.chrome.cwd = "/w".to_string();
        v.chrome.chat_name = "w".to_string();
        let frame = v.render_frame(60, 40);
        assert_eq!(frame.len(), 40);
        // The editor prompt sits above the (empty) tray row.
        let joined = frame.iter().map(text_of).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("Collapsed mode"));
    }

    fn view_with(entries: Vec<ChatEntry>) -> AgentView {
        let mut view = AgentView::new(crate::theme::Theme::builtin(
            "prime",
            crate::theme::ColorMode::Color256,
        ));
        for entry in entries {
            view.push_entry(entry);
        }
        view
    }

    fn settled_tool_card(id: &str) -> ChatEntry {
        ChatEntry::Tool(Box::new(ToolCallCard {
            id: id.to_string(),
            name: "bash".to_string(),
            args: serde_json::json!({"command": "echo done"}),
            started: true,
            started_at: Some(std::time::Instant::now()),
            ended_at: Some(std::time::Instant::now()),
            result: Some(ToolResultView {
                content: vec![serde_json::json!({"type": "text", "text": "done"})],
                details: serde_json::Value::Null,
                is_error: false,
            }),
            result_partial: false,
        }))
    }

    fn transcript_text(view: &mut AgentView, width: usize) -> String {
        let rows = view.render_transcript(width);
        rows.iter()
            .map(|line| line.iter().map(|span| span.content.as_str()).collect())
            .collect::<Vec<String>>()
            .join("\n")
    }

    /// A settled transcript renders identically from the layout cache and
    /// from a fresh layout: caching must never change the frame.
    #[test]
    fn cached_transcript_rows_match_fresh_render() {
        let mut view = view_with(vec![
            ChatEntry::User {
                text: "hello".to_string(),
            },
            ChatEntry::Assistant(Box::new(AssistantMessage {
                blocks: vec![MessageBlock::Text("world".to_string())],
                has_tool_calls: false,
                streaming: false,
                error: None,
                aborted: false,
            })),
            settled_tool_card("call_1"),
        ]);
        let fresh = transcript_text(&mut view, 80);
        let cached = transcript_text(&mut view, 80);
        assert_eq!(fresh, cached);
    }

    /// A mutation marked stale re-renders: the cached rows must never hide
    /// new content (streamed blocks, tool-card state, attached errors).
    #[test]
    fn stale_entry_re_renders_new_content() {
        let mut view = view_with(vec![ChatEntry::Assistant(Box::new(AssistantMessage {
            blocks: vec![MessageBlock::Text("part one".to_string())],
            has_tool_calls: false,
            streaming: false,
            error: None,
            aborted: false,
        }))]);
        let before = transcript_text(&mut view, 80);
        if let Some(ChatEntry::Assistant(open)) = view.chat.get_mut(0) {
            open.blocks = vec![MessageBlock::Text("part one part two".to_string())];
        }
        view.mark_entry_stale(0);
        let after = transcript_text(&mut view, 80);
        assert!(before.contains("part one"));
        assert!(!before.contains("part two"));
        assert!(after.contains("part one part two"));
    }

    /// A running tool card animates: its rows must not be cached (the
    /// spinner frame advances), while a settled card's rows ignore the
    /// pulse frame.
    #[test]
    fn running_card_is_not_cached_and_settled_card_is() {
        let running = ChatEntry::Tool(Box::new(ToolCallCard {
            id: "call_r".to_string(),
            name: "bash".to_string(),
            args: serde_json::json!({"command": "sleep 1"}),
            started: true,
            started_at: Some(std::time::Instant::now()),
            ended_at: None,
            result: None,
            result_partial: false,
        }));
        let mut view = view_with(vec![running, settled_tool_card("call_d")]);
        view.pulse_frame = 0;
        let frame0 = transcript_text(&mut view, 80);
        view.pulse_frame = 1;
        let frame1 = transcript_text(&mut view, 80);
        assert_ne!(frame0, frame1, "the running spinner must animate");

        // With only a settled card, the pulse frame cannot change rows.
        let mut settled_view = view_with(vec![settled_tool_card("call_d")]);
        settled_view.pulse_frame = 0;
        let s0 = transcript_text(&mut settled_view, 80);
        settled_view.pulse_frame = 7;
        let s7 = transcript_text(&mut settled_view, 80);
        assert_eq!(s0, s7);
    }

    /// A conversation-detail change re-flows every cached row (thinking
    /// blocks and tool output expand).
    #[test]
    fn detail_change_invalidates_cached_rows() {
        let mut view = view_with(vec![ChatEntry::Assistant(Box::new(AssistantMessage {
            blocks: vec![
                MessageBlock::Thinking("thinking body".to_string()),
                MessageBlock::Text("answer".to_string()),
            ],
            has_tool_calls: false,
            streaming: false,
            error: None,
            aborted: false,
        }))]);
        let overview = transcript_text(&mut view, 80);
        view.detail = view.detail.next();
        let details = transcript_text(&mut view, 80);
        assert!(!overview.contains("thinking body"));
        assert!(details.contains("thinking body"));
    }

    fn agent_message_row() -> ChatEntry {
        ChatEntry::AgentMessage(Box::new(crate::custom_message::AgentMessageRow {
            participant: "from child lane".to_string(),
            message: "hi".to_string(),
        }))
    }

    fn shell_completion_row() -> ChatEntry {
        ChatEntry::ShellCompletion(Box::new(crate::custom_message::ShellCompletionRow {
            pid: Some(1),
            exit_code: Some(0),
            content: "[bash-done]".to_string(),
        }))
    }

    /// TS `createConversationSpacing.shouldAddLeadingSpace` for one
    /// spacing-driven row: scan back over hidden assistant rows, honor the
    /// trailing space of a visible assistant, and sit flush against compact
    /// neighbors (tool cards, agent messages, shell completions).
    #[test]
    fn conversation_leading_matches_ts_spacing_rules() {
        let visible_assistant = || {
            ChatEntry::Assistant(Box::new(AssistantMessage {
                blocks: vec![MessageBlock::Text("done".to_string())],
                has_tool_calls: true,
                streaming: false,
                error: None,
                aborted: false,
            }))
        };
        let tool_only_assistant = || {
            ChatEntry::Assistant(Box::new(AssistantMessage {
                blocks: Vec::new(),
                has_tool_calls: true,
                streaming: false,
                error: None,
                aborted: false,
            }))
        };
        let user = || ChatEntry::User {
            text: "hello".to_string(),
        };

        // Nothing preceding: the collapsed form leads with a blank, the
        // expanded form sits flush against the top of the chat.
        let view = view_with(vec![agent_message_row()]);
        assert!(view.conversation_leading(0, false));
        assert!(!view.conversation_leading(0, true));

        // A user row is never a compact neighbor: both forms lead.
        let view = view_with(vec![user(), agent_message_row()]);
        assert!(view.conversation_leading(1, false));
        assert!(view.conversation_leading(1, true));

        // A visible assistant with tool calls carries the trailing space:
        // the next agent message sits flush in both forms.
        let view = view_with(vec![visible_assistant(), agent_message_row()]);
        assert!(!view.conversation_leading(1, false));
        assert!(!view.conversation_leading(1, true));

        // A compact neighbor (tool card, shell completion, agent message):
        // flush collapsed, blank expanded.
        for neighbor in [
            settled_tool_card("c1"),
            shell_completion_row(),
            agent_message_row(),
        ] {
            let view = view_with(vec![neighbor, agent_message_row()]);
            assert!(!view.conversation_leading(1, false), "flush collapsed");
            assert!(view.conversation_leading(1, true), "blank expanded");
        }

        // A tool-only assistant (no visible body) is a separator: the row
        // after it keeps the trailing-space spacing in both forms.
        let view = view_with(vec![tool_only_assistant(), agent_message_row()]);
        assert!(!view.conversation_leading(1, false));
        assert!(!view.conversation_leading(1, true));

        // The backward scan returns at the first non-skippable row it
        // meets: a user row NEWER than the tool-only assistant ends the
        // scan, so the agent message leads (the separator is never
        // reached).
        let view = view_with(vec![tool_only_assistant(), user(), agent_message_row()]);
        assert!(view.conversation_leading(2, false));
        assert!(view.conversation_leading(2, true));
        // With the separator NEWER than the non-compact row, the
        // separator dominates (TS returns the tool separator with a
        // trailing space), so the agent message renders flush.
        let view = view_with(vec![user(), tool_only_assistant(), agent_message_row()]);
        assert!(!view.conversation_leading(2, false));
        assert!(!view.conversation_leading(2, true));
        // A compact row older than the separator ends the scan WITHOUT the
        // separator (TS falls through the `toolSeparator` branch to the
        // compact row): flush collapsed, blank expanded.
        let view = view_with(vec![
            settled_tool_card("c2"),
            tool_only_assistant(),
            agent_message_row(),
        ]);
        assert!(!view.conversation_leading(2, false));
        assert!(view.conversation_leading(2, true));

        // A hidden thinking-only assistant contributes nothing to spacing:
        // the scan skips it to the user row.
        let hidden_assistant = || {
            ChatEntry::Assistant(Box::new(AssistantMessage {
                blocks: vec![MessageBlock::Thinking("quiet".to_string())],
                has_tool_calls: false,
                streaming: false,
                error: None,
                aborted: false,
            }))
        };
        let mut view = view_with(vec![user(), hidden_assistant(), agent_message_row()]);
        view.detail = Detail::Overview;
        assert!(view.conversation_leading(2, false));
    }

    /// The custom rows render through the transcript path: the agent
    /// message header plus its guttered body, and the shell-completion row.
    #[test]
    fn custom_rows_render_in_the_transcript() {
        let mut view = view_with(vec![agent_message_row(), shell_completion_row()]);
        view.detail = Detail::All;
        let text = transcript_text(&mut view, 80);
        assert!(text.contains("Agent message received \u{b7} from child lane"));
        assert!(text.contains("\u{2570}\u{2500} hi"));
        assert!(text.contains("Background shell command finished"));
        assert!(text.contains("[bash-done]"));
    }

    /// A streaming assistant message updates across frames: its rows stay
    /// out of the cache until the stream settles.
    #[test]
    fn streaming_assistant_updates_across_frames() {
        let mut view = view_with(vec![ChatEntry::Assistant(Box::new(AssistantMessage {
            blocks: vec![MessageBlock::Text("so far".to_string())],
            has_tool_calls: false,
            streaming: true,
            error: None,
            aborted: false,
        }))]);
        let frame0 = transcript_text(&mut view, 80);
        assert!(frame0.contains("so far"));
        if let Some(ChatEntry::Assistant(open)) = view.chat.get_mut(0) {
            open.blocks = vec![MessageBlock::Text("so far, and more".to_string())];
        }
        view.mark_entry_stale(0);
        let frame1 = transcript_text(&mut view, 80);
        assert!(frame1.contains("and more"));
    }
}
