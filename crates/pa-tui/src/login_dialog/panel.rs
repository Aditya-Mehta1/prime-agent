//! The mounted login dialog component (TS `LoginDialogComponent`): the
//! panel render and the key routing over the shared
//! [`LoginDialogHandle`] state.

use crate::clipboard::OscSink;
use crate::keybindings::{format_key_text, KeybindingsManager};
use crate::login_dialog::{
    lock_state, ClipboardFn, ContentRow, CopyStatus, LoginDialogHandle, RowColor,
};
use crate::theme::{Theme, ThemeColor};
use crate::width::{truncate_line, wrap_line};
use crate::{Line, Span};

/// What one key press asked the mount to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginDialogAction {
    /// Handled inside the panel (a field edit, a copy, a resolution).
    None,
    /// Esc / back: the flow was aborted; it settles through the mount's
    /// settle channel (TS `cancel()` rejects the pending resolvers).
    Cancelled,
    /// The armed exit keys (onboarding only): quit the app.
    Exit,
}

/// The mounted login dialog component.
pub struct LoginDialog {
    handle: LoginDialogHandle,
    clipboard: ClipboardFn,
}

impl LoginDialog {
    pub(crate) fn new(handle: LoginDialogHandle, clipboard: ClipboardFn) -> Self {
        LoginDialog { handle, clipboard }
    }

    /// The shared drive surface (the mount aborts the flow on a reset).
    pub fn handle(&self) -> &LoginDialogHandle {
        &self.handle
    }

    /// The panel's rendered rows (the TS `MenuPanel` inline shape): the
    /// top rule, the muted title, the wrapped content rows, the paste
    /// field, and the auth-actions hint row.
    pub fn render(&self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        let state = lock_state(&self.handle.inner);
        let mut lines: Vec<Line> = Vec::new();
        if state.options.top_rule {
            lines.push(vec![
                theme.fg_span(ThemeColor::BorderMuted, "\u{2500}".repeat(width.max(1)))
            ]);
        }
        if !state.options.hide_title {
            let title = format!("Login to {}", state.options.provider);
            lines.push(truncate_line(
                &vec![Span::raw(" "), theme.fg_span(ThemeColor::Muted, title)],
                width,
                "",
            ));
        }
        // Content children render at the inner width with a one-space
        // indent (the TS MenuPanel child layout).
        let inner = width.saturating_sub(2).max(1);
        for row in &state.rows {
            match row {
                ContentRow::Blank => lines.push(vec![Span::raw(" ")]),
                ContentRow::Text { text, color, bold } => {
                    let mut span = theme_row_span(theme, *color, text);
                    if *bold {
                        span = theme.bold(span);
                    }
                    let span_line: Line = vec![span];
                    for wrapped in wrap_line(&span_line, inner) {
                        let mut row = vec![Span::raw(" ")];
                        row.extend(wrapped);
                        lines.push(truncate_line(&row, width, ""));
                    }
                }
                ContentRow::KeyHint { hints } => {
                    let mut row = vec![Span::raw(" ")];
                    row.extend(key_hint_spans(theme, kb, hints));
                    lines.push(truncate_line(&row, width, ""));
                }
            }
        }
        // The floating panel-bottom rows (TS order: input, its spacer,
        // then the auth-actions hints).
        if state.input_visible {
            // The plain paste field (TS `MenuSearchInput("Paste value",
            // inline, plain)`): full width, no enclosing rules.
            lines.push(crate::menu_panel::render_input_field(
                theme,
                width,
                state.field.value(),
                state.field.cursor(),
                true,
                "Paste value",
            ));
            lines.push(vec![Span::raw(" ")]);
        }
        if state.auth_actions {
            let mut row = vec![Span::raw(" ")];
            row.extend(auth_action_spans(theme, kb, &state));
            lines.push(truncate_line(&row, width, ""));
        }
        lines
    }

    /// One key press (TS `handleInput`): the armed exit keys, the copy
    /// binding, cancel/back, the pending resolutions, then the field.
    pub(crate) fn handle_key(
        &mut self,
        key: &str,
        kb: &KeybindingsManager,
        osc: &mut OscSink,
    ) -> LoginDialogAction {
        // The exit keys quit the app before anything else when the
        // onboarding surface armed them (TS `isOnboardingExitKey`).
        {
            let state = lock_state(&self.handle.inner);
            if state.options.on_exit
                && (kb.matches(key, "app.clear") || kb.matches(key, "app.exit"))
            {
                return LoginDialogAction::Exit;
            }
            // The copy binding fires only while the URL is shown; a
            // printable char goes to the field, never the copy.
            if state.auth_url.is_some()
                && kb.matches(key, "app.clipboard.copyLoginUrl")
                && (!state.input_visible || !is_printable_input(key))
            {
                let url = state.auth_url.clone().unwrap_or_default();
                drop(state);
                let status = match (self.clipboard)(&url, osc) {
                    Ok(()) => CopyStatus::Copied,
                    Err(_) => CopyStatus::Failed,
                };
                lock_state(&self.handle.inner).copy_status = Some(status);
                return LoginDialogAction::None;
            }
        }
        // Esc, or the left arrow acting as back (TS `shouldTreatAsBack`:
        // only at the field's start while the field is shown).
        let back = {
            let state = lock_state(&self.handle.inner);
            kb.matches(key, "tui.select.cancel")
                || (kb.matches(key, "app.modal.back")
                    && (!state.input_visible || state.field.cursor() == 0))
        };
        if back {
            self.handle.abort();
            return LoginDialogAction::Cancelled;
        }
        let mut state = lock_state(&self.handle.inner);
        if state.pending_continue.is_some() && kb.matches(key, "tui.select.confirm") {
            if let Some(resolver) = state.pending_continue.take() {
                let _ = resolver.send(Ok(()));
            }
            return LoginDialogAction::None;
        }
        // The paste field's submit (TS `Input`'s `tui.input.submit`): the
        // pending input resolves with the field's value.
        if state.pending_input.is_some()
            && state.input_visible
            && kb.matches(key, "tui.input.submit")
        {
            if let Some(resolver) = state.pending_input.take() {
                let value = state.field.value().to_string();
                let _ = resolver.send(Ok(value));
            }
            return LoginDialogAction::None;
        }
        if state.input_visible {
            state.field.handle_key(key, kb);
        }
        LoginDialogAction::None
    }
}

/// One content row as a theme-colored span.
fn theme_row_span(theme: &Theme, color: RowColor, text: &str) -> crate::Span {
    match color {
        RowColor::Plain => Span::raw(text.to_string()),
        RowColor::Text => theme.fg_span(ThemeColor::Text, text),
        RowColor::Muted => theme.fg_span(ThemeColor::Muted, text),
        RowColor::Accent => theme.fg_span(ThemeColor::Accent, text),
    }
}

/// TS `keyHint` (`keybinding-hints.ts`): dim key text, muted label;
/// several hints join with two spaces (the TS `.join("  ")`).
fn key_hint_spans(
    theme: &Theme,
    kb: &KeybindingsManager,
    hints: &[(&'static str, &'static str)],
) -> Line {
    let mut spans: Line = Vec::new();
    for (index, (binding, description)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(theme.fg_span(ThemeColor::Dim, kb.key_text(binding)));
        spans.push(theme.fg_span(ThemeColor::Muted, format!(" {description}")));
    }
    spans
}

/// The auth-actions row (TS `getAuthActionsText`): the submit hint while
/// the field is visible, the copy status, the copy binding's keys, and
/// the cancel hint, joined by two spaces.
fn auth_action_spans(
    theme: &Theme,
    kb: &KeybindingsManager,
    state: &crate::login_dialog::LoginDialogState,
) -> Line {
    let mut parts: Vec<Line> = Vec::new();
    if state.input_visible {
        parts.push(key_hint_spans(
            theme,
            kb,
            &[("tui.select.confirm", "submit")],
        ));
    }
    match state.copy_status {
        Some(CopyStatus::Copied) => {
            parts.push(vec![
                theme.fg_span(ThemeColor::Success, "Copied sign-in link")
            ]);
        }
        Some(CopyStatus::Failed) => {
            parts.push(vec![
                theme.fg_span(ThemeColor::Error, "Failed to copy sign-in link")
            ]);
        }
        None => {}
    }
    // TS `getAuthActionsText`'s copy keys: the first configured key, or
    // the non-text-entry keys while the field is visible.
    let configured = kb.get_keys("app.clipboard.copyLoginUrl");
    let copy_keys: Vec<String> = if state.input_visible {
        configured
            .iter()
            .filter(|key| !is_text_entry_keybinding(key))
            .cloned()
            .collect()
    } else {
        configured.into_iter().take(1).collect()
    };
    let label = if state.copy_status == Some(CopyStatus::Failed) {
        "retry"
    } else {
        "copy"
    };
    if !copy_keys.is_empty() {
        parts.push(vec![
            theme.fg_span(ThemeColor::Dim, format_key_text(&copy_keys.join("/"))),
            theme.fg_span(ThemeColor::Muted, format!(" {label}")),
        ]);
    }
    parts.push(key_hint_spans(
        theme,
        kb,
        &[("tui.select.cancel", "cancel")],
    ));
    let mut spans: Line = Vec::new();
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.extend(part);
    }
    spans
}

/// TS `isTextEntryKeybinding`: a plain printable binding that must keep
/// typing into the field instead of acting.
fn is_text_entry_keybinding(key: &str) -> bool {
    let lower = key.to_lowercase();
    let parts: Vec<&str> = lower.split('+').collect();
    let key_part = parts.last().copied();
    !parts.contains(&"ctrl")
        && !parts.contains(&"alt")
        && key_part.is_some_and(|part| part == "space" || part.chars().count() == 1)
}

/// TS `isPrintableInput`: one printable character (never a modifier combo).
fn is_printable_input(key: &str) -> bool {
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => c >= ' ' && c != '\u{7f}',
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::KeybindingsManager;
    use crate::login_dialog::LoginDialogOptions;
    use crate::theme::{ColorMode, Theme};
    use std::sync::{Arc, Mutex};

    fn theme() -> Theme {
        Theme::builtin("prime", ColorMode::TrueColor)
    }

    fn kb() -> KeybindingsManager {
        KeybindingsManager::new()
    }

    /// A recording clipboard: captures the texts, never touches a real
    /// clipboard, and reports the scripted success.
    struct RecordingClipboard {
        copied: Mutex<Vec<String>>,
        succeed: bool,
    }

    fn recording_clipboard(succeed: bool) -> (ClipboardFn, Arc<RecordingClipboard>) {
        let recorder = Arc::new(RecordingClipboard {
            copied: Mutex::new(Vec::new()),
            succeed,
        });
        let capture = {
            let recorder = Arc::clone(&recorder);
            Arc::new(move |text: &str, _osc: &mut OscSink| {
                recorder.copied.lock().unwrap().push(text.to_string());
                if recorder.succeed {
                    Ok(())
                } else {
                    Err("Failed to copy to clipboard".to_string())
                }
            }) as ClipboardFn
        };
        (capture, recorder)
    }

    fn make_dialog(
        options: LoginDialogOptions,
        succeed: bool,
    ) -> (LoginDialog, Arc<RecordingClipboard>) {
        // The wording tests render the bare URL row, so pin the
        // capability gate off regardless of the host terminal.
        crate::hyperlinks::set_hyperlinks_override(Some(false));
        let handle = crate::login_dialog::LoginDialogHandle::new(options, Box::new(|| {}));
        let (clipboard, recorder) = recording_clipboard(succeed);
        (LoginDialog::new(handle, clipboard), recorder)
    }

    /// The rendered rows as trimmed plain text (tmux-capture shape).
    fn frame_text(dialog: &LoginDialog) -> Vec<String> {
        dialog
            .render(&theme(), 80, &kb())
            .iter()
            .map(|line| {
                line.iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
            })
            .map(|row| row.trim_end().to_string())
            .collect()
    }

    /// The `/mcp login` panel: rule, muted title, URL, browser fallback,
    /// and the copy/cancel hints (TS `getAuthActionsText` with the
    /// default bindings).
    #[test]
    fn renders_the_ts_panel_wording() {
        let (dialog, _) = make_dialog(LoginDialogOptions::new("linear"), true);
        dialog
            .handle()
            .show_auth("https://fixture.example/authorize?state=x", None);
        assert_eq!(
            frame_text(&dialog),
            vec![
                "\u{2500}".repeat(80),
                " Login to linear".to_string(),
                String::new(),
                " https://fixture.example/authorize?state=x".to_string(),
                String::new(),
                " Complete the sign-in in your browser.".to_string(),
                format!(" C copy  {} cancel", cancel_keys_label()),
            ]
        );
    }

    /// The provider label the composition root resolves retitles the
    /// panel (TS `providerNameOverride`).
    #[test]
    fn set_provider_name_titles_the_label() {
        let (dialog, _) = make_dialog(LoginDialogOptions::new("linear"), true);
        dialog.handle().set_provider_name("Linear");
        dialog
            .handle()
            .show_progress("Discovered https://fixture.example");
        assert_eq!(frame_text(&dialog)[1], " Login to Linear");
    }

    /// Onboarding owns the screen above the panel: no rule, no title
    /// (TS `loginDialogOptions` on the onboarding surface).
    #[test]
    fn onboarding_options_hide_the_rule_and_title() {
        let options = LoginDialogOptions {
            provider: "linear".to_string(),
            top_rule: false,
            hide_title: true,
            on_exit: true,
        };
        let (dialog, _) = make_dialog(options, true);
        dialog
            .handle()
            .show_auth("https://fixture.example/authorize", None);
        let frame = frame_text(&dialog);
        assert_eq!(frame[0], String::new(), "no top rule");
        assert!(
            !frame.iter().any(|row| row.contains("Login to")),
            "no title"
        );
    }

    /// The paste field block: the TS `Paste value` placeholder, the blank
    /// spacer row, and the submit/copy/cancel hints (the text-entry copy
    /// key drops out while the field is visible — TS
    /// `isTextEntryKeybinding`).
    #[test]
    fn manual_input_renders_the_field_and_submit_hints() {
        let (dialog, _) = make_dialog(LoginDialogOptions::new("linear"), true);
        let _pending = dialog
            .handle()
            .show_manual_input("Paste redirect URL below, or complete login in browser:");
        let frame = frame_text(&dialog);
        let joined = frame.join("\n");
        assert!(
            joined.contains("Paste value"),
            "the field placeholder:\n{joined}"
        );
        assert!(joined.contains("Paste redirect URL below, or complete login in browser:"));
        assert!(
            joined.contains("Enter submit"),
            "the submit hint:\n{joined}"
        );
        let copy_keys = format_key_text("alt+c");
        assert!(
            joined.contains(&format!("{copy_keys} copy")),
            "the non-text-entry copy key only:\n{joined}"
        );
        assert!(
            !joined.contains(" C copy"),
            "the plain `c` never copies while typing:\n{joined}"
        );
    }

    /// The copy binding fires the injectable clipboard with the URL and
    /// reports the TS success status.
    #[test]
    fn copy_binding_fires_the_clipboard_and_reports_copied() {
        let (mut dialog, recorder) = make_dialog(LoginDialogOptions::new("linear"), true);
        dialog
            .handle()
            .show_auth("https://fixture.example/authorize", None);
        assert_eq!(
            dialog.handle_key("c", &kb(), &mut OscSink::Stdout),
            LoginDialogAction::None
        );
        assert_eq!(
            *recorder.copied.lock().unwrap(),
            vec!["https://fixture.example/authorize".to_string()]
        );
        let joined = frame_text(&dialog).join("\n");
        assert!(joined.contains("Copied sign-in link"), "{joined}");
    }

    /// A failed copy reports the TS error status and relabels the copy
    /// hint to `retry` (TS `getAuthActionsText("failed")`).
    #[test]
    fn failed_copy_reports_the_error_status_and_retry_label() {
        let (mut dialog, _) = make_dialog(LoginDialogOptions::new("linear"), false);
        dialog
            .handle()
            .show_auth("https://fixture.example/authorize", None);
        dialog.handle_key("c", &kb(), &mut OscSink::Stdout);
        let joined = frame_text(&dialog).join("\n");
        assert!(joined.contains("Failed to copy sign-in link"), "{joined}");
        assert!(joined.contains(" C retry"), "{joined}");
    }

    /// While the field is visible, a printable char that matches the copy
    /// binding types into the field instead (TS `isPrintableInput`).
    #[test]
    fn printable_keys_type_into_the_field_never_copy() {
        let (mut dialog, recorder) = make_dialog(LoginDialogOptions::new("linear"), true);
        dialog
            .handle()
            .show_auth("https://fixture.example/authorize", None);
        let _pending = dialog
            .handle()
            .show_manual_input("Paste redirect URL below, or complete login in browser:");
        assert_eq!(
            dialog.handle_key("c", &kb(), &mut OscSink::Stdout),
            LoginDialogAction::None
        );
        assert!(recorder.copied.lock().unwrap().is_empty(), "no copy fired");
        let state = crate::login_dialog::lock_state(&dialog.handle().inner);
        assert_eq!(state.field.value(), "c", "the char typed into the field");
    }

    /// Enter submits the pending paste field with its typed value (TS
    /// `Input`'s `tui.input.submit` -> `onSubmit`).
    #[tokio::test]
    async fn enter_submits_the_pending_input_with_the_typed_value() {
        let (mut dialog, _) = make_dialog(LoginDialogOptions::new("linear"), true);
        let pending = dialog
            .handle()
            .show_manual_input("Paste redirect URL below, or complete login in browser:");
        // Type `abc`, then Enter: the answer resolves with the value.
        for key in ["a", "b", "c"] {
            dialog.handle_key(key, &kb(), &mut OscSink::Stdout);
        }
        assert_eq!(
            dialog.handle_key("enter", &kb(), &mut OscSink::Stdout),
            LoginDialogAction::None
        );
        assert_eq!(pending.await.expect("enter submits"), "abc");
    }

    /// Esc cancels: the flow aborts (the pending answers resolve as
    /// cancelled) and the key reports the cancel.
    #[tokio::test]
    async fn cancel_aborts_the_pending_flow() {
        let (mut dialog, _) = make_dialog(LoginDialogOptions::new("linear"), true);
        let pending = dialog
            .handle()
            .show_manual_input("Paste redirect URL below, or complete login in browser:");
        assert_eq!(
            dialog.handle_key("escape", &kb(), &mut OscSink::Stdout),
            LoginDialogAction::Cancelled
        );
        assert_eq!(pending.await.unwrap_err().to_string(), "Login cancelled");
    }

    /// The left arrow acts as back only at the field's start (TS
    /// `shouldTreatAsBack`); mid-edit it moves the cursor.
    #[test]
    fn left_arrow_is_back_only_at_the_field_start() {
        let (mut dialog, _) = make_dialog(LoginDialogOptions::new("linear"), true);
        let _pending = dialog
            .handle()
            .show_manual_input("Paste redirect URL below, or complete login in browser:");
        // At the start: back (a cancel).
        assert_eq!(
            dialog.handle_key("left", &kb(), &mut OscSink::Stdout),
            LoginDialogAction::Cancelled
        );
        // A fresh field with text: mid-edit, left moves the cursor.
        let _pending = dialog
            .handle()
            .show_manual_input("Paste redirect URL below, or complete login in browser:");
        dialog.handle_key("a", &kb(), &mut OscSink::Stdout);
        dialog.handle_key("b", &kb(), &mut OscSink::Stdout);
        assert_eq!(
            dialog.handle_key("left", &kb(), &mut OscSink::Stdout),
            LoginDialogAction::None
        );
        {
            let state = crate::login_dialog::lock_state(&dialog.handle().inner);
            assert_eq!(state.field.cursor(), 1, "the cursor moved, not a cancel");
        }
        // The info screen has no field to guard: left is always back.
        dialog
            .handle()
            .show_info(&["Signed in to the browser flow.".to_string()]);
        assert_eq!(
            dialog.handle_key("left", &kb(), &mut OscSink::Stdout),
            LoginDialogAction::Cancelled
        );
    }

    /// The onboarding exit keys quit the app only when armed (TS
    /// `isOnboardingExitKey` + `dialogOptions.onExit`).
    #[test]
    fn exit_keys_quit_only_when_armed() {
        let options = LoginDialogOptions {
            provider: "linear".to_string(),
            top_rule: false,
            hide_title: true,
            on_exit: true,
        };
        let (mut dialog, _) = make_dialog(options, true);
        dialog
            .handle()
            .show_auth("https://fixture.example/authorize", None);
        assert_eq!(
            dialog.handle_key("ctrl+c", &kb(), &mut OscSink::Stdout),
            LoginDialogAction::Exit
        );
        assert_eq!(
            dialog.handle_key("ctrl+d", &kb(), &mut OscSink::Stdout),
            LoginDialogAction::Exit
        );
        // Unarmed: ctrl+c is the cancel binding instead.
        let (mut dialog, _) = make_dialog(LoginDialogOptions::new("linear"), true);
        dialog
            .handle()
            .show_auth("https://fixture.example/authorize", None);
        assert_eq!(
            dialog.handle_key("ctrl+c", &kb(), &mut OscSink::Stdout),
            LoginDialogAction::Cancelled
        );
    }

    /// The info and continue screens render the TS hint wording
    /// (`esc close`, `enter continue · esc cancel`) and Enter resolves
    /// the pending continue.
    #[tokio::test]
    async fn info_and_continue_screens_render_their_hints() {
        let (mut dialog, _) = make_dialog(LoginDialogOptions::new("linear"), true);
        dialog
            .handle()
            .show_info(&["Signed in to the browser flow.".to_string()]);
        let frame = frame_text(&dialog);
        assert_eq!(
            frame.last().unwrap(),
            &format!(" {} close", cancel_keys_label()),
            "the info hint row"
        );
        let continuing = dialog
            .handle()
            .show_continue_info(&["Prime connected.".to_string()]);
        let joined = frame_text(&dialog).join("\n");
        assert!(
            joined.contains(&format!("Enter continue  {} cancel", cancel_keys_label())),
            "the continue hint row:\n{joined}"
        );
        dialog.handle_key("enter", &kb(), &mut OscSink::Stdout);
        continuing.await.expect("enter resolves the continue");
    }

    /// The default cancel-keys label for the hint wording (platform-
    /// independent assertion: built with the same formatter).
    fn cancel_keys_label() -> String {
        format_key_text("escape/ctrl+c")
    }

    /// The verification-code block renders the label and the bold code.
    #[test]
    fn verification_code_renders_the_label_and_bold_code() {
        let (dialog, _) = make_dialog(LoginDialogOptions::new("linear"), true);
        dialog
            .handle()
            .show_auth("https://fixture.example/authorize", Some("Code: ABC-123"));
        let frame = frame_text(&dialog);
        assert!(frame.contains(&" Verification code".to_string()));
        assert!(frame.contains(&" ABC-123".to_string()));
    }

    /// The waiting screen renders the accent row.
    #[test]
    fn waiting_renders_the_accent_row() {
        let (dialog, _) = make_dialog(LoginDialogOptions::new("linear"), true);
        dialog
            .handle()
            .show_waiting("Waiting for browser authentication...");
        assert!(frame_text(&dialog)
            .iter()
            .any(|row| row == " Waiting for browser authentication..."));
    }
}
