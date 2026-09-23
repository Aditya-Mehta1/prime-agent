//! The in-TUI login dialog (TS `LoginDialogComponent`): the inline panel
//! that replaces the prompt area during a provider login flow. The flow
//! runs in the composition root against the shared
//! [`LoginDialogHandle`] (its show* methods mirror the TS component's);
//! the mounted [`LoginDialog`] (in `panel.rs`) renders the panel and
//! routes the frame's keys to it.

mod panel;

pub use panel::{LoginDialog, LoginDialogAction};

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};

use crate::clipboard::OscSink;
use crate::search_input::SearchInput;

/// A pending answer the flow awaits (the TS show* promises).
pub type LoginAnswer<T> = Pin<Box<dyn Future<Output = Result<T>> + Send>>;

/// The injectable clipboard the copy binding drives (the mount passes the
/// session's OSC 52 sink; tests pass a recorder).
pub(crate) type ClipboardFn = Arc<dyn Fn(&str, &mut OscSink) -> Result<(), String> + Send + Sync>;

/// The dialog's chrome options (TS `dialogOptions`): onboarding owns the
/// screen above the panel, so it hides the title and rule and arms the
/// exit keys.
#[derive(Debug, Clone)]
pub struct LoginDialogOptions {
    /// The panel title's provider name (`Login to {provider}`); the
    /// composition root refines it from the integration's label.
    pub provider: String,
    /// Draw the inline top rule (on by default; onboarding turns it off).
    pub top_rule: bool,
    /// Draw the `Login to {provider}` title (onboarding turns it off).
    pub hide_title: bool,
    /// Answer the onboarding exit keys with [`LoginDialogAction::Exit`].
    pub on_exit: bool,
}

impl LoginDialogOptions {
    /// The `/mcp login` surface: titled panel over the transcript rule.
    pub fn new(provider: impl Into<String>) -> Self {
        LoginDialogOptions {
            provider: provider.into(),
            top_rule: true,
            hide_title: false,
            on_exit: false,
        }
    }
}

/// The status of one copy attempt (the auth-actions row's status text).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyStatus {
    Copied,
    Failed,
}

/// One content row's color (the TS rows' `theme.fg` kinds).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowColor {
    /// A plain `Text` child (no theme color).
    Plain,
    Text,
    Muted,
    Accent,
}

/// One panel content row (the TS dialog's `contentContainer` children).
#[derive(Clone)]
pub(crate) enum ContentRow {
    /// `Spacer(1)`: one blank row.
    Blank,
    /// A wrapped, one-space-indented text row (a TS `Text` child).
    Text {
        text: String,
        color: RowColor,
        bold: bool,
    },
    /// A key-hint row: dim key text, muted labels (TS `keyHint`), with
    /// several hints joined by two spaces.
    KeyHint {
        hints: Vec<(&'static str, &'static str)>,
    },
}

/// The shared dialog state, driven by the flow's handle calls and the
/// mounted component's key routing.
pub(crate) struct LoginDialogState {
    pub(crate) options: LoginDialogOptions,
    pub(crate) rows: Vec<ContentRow>,
    pub(crate) auth_url: Option<String>,
    /// The floating auth-actions hint row (TS `this.authActions`): every
    /// content insert lands above it; it renders last.
    pub(crate) auth_actions: bool,
    pub(crate) input_visible: bool,
    pub(crate) field: SearchInput,
    pub(crate) copy_status: Option<CopyStatus>,
    pub(crate) pending_input: Option<tokio::sync::oneshot::Sender<Result<String>>>,
    pub(crate) pending_continue: Option<tokio::sync::oneshot::Sender<Result<()>>>,
}

impl LoginDialogState {
    fn push_text(&mut self, text: impl Into<String>, color: RowColor) {
        self.rows.push(ContentRow::Text {
            text: text.into(),
            color,
            bold: false,
        });
    }

    /// TS `startContent`: clear the panel, drop the URL, the floating
    /// auth-actions row, and the paste field, and lead with one blank row.
    fn start_content(&mut self) {
        self.rows.clear();
        self.rows.push(ContentRow::Blank);
        self.auth_url = None;
        self.auth_actions = false;
        self.input_visible = false;
        self.copy_status = None;
    }

    /// TS `addSectionSpacer`: one blank row before the next block.
    fn section_spacer(&mut self) {
        if self.rows.is_empty() {
            self.start_content();
        } else {
            self.rows.push(ContentRow::Blank);
        }
    }

    /// The TS code-parse for provider instructions
    /// (`/^(?:Code|Enter code):\s*(.+)$/i`): the captured code.
    fn verification_code(instructions: &str) -> Option<&str> {
        let trimmed = instructions.trim();
        let lower = trimmed.to_lowercase();
        let prefix = ["enter code:", "code:"]
            .iter()
            .find(|prefix| lower.starts_with(*prefix))
            .map(|prefix| prefix.len())?;
        // `get` keeps an exotic case-folding length change a plain text
        // row instead of a panic.
        let code = trimmed.get(prefix..)?.trim_start();
        (!code.is_empty()).then_some(code)
    }

    /// TS `addInstructions`: a code prompt renders as a blank row, a
    /// muted "Verification code" label, and a bold code; anything else is
    /// the instruction text.
    fn push_instructions(&mut self, instructions: &str) {
        if let Some(code) = Self::verification_code(instructions) {
            self.rows.push(ContentRow::Blank);
            self.push_text("Verification code", RowColor::Muted);
            self.rows.push(ContentRow::Text {
                text: code.to_string(),
                color: RowColor::Text,
                bold: true,
            });
            return;
        }
        self.push_text(instructions, RowColor::Text);
    }

    /// TS `addInputField`: the paste field moves to the panel bottom
    /// with one blank row before the floating auth-actions row.
    fn add_input_field(&mut self) {
        self.input_visible = true;
        self.auth_actions = true;
        self.copy_status = None;
    }
}

/// The cross-task drive surface: the login flow (the composition root)
/// calls the show* methods while the TUI renders the shared state — the
/// TS `LoginDialogComponent`'s show* API split across the task boundary.
/// Every method triggers the mount's request-render callback.
pub struct LoginDialogHandle {
    pub(crate) inner: Arc<LoginDialogInner>,
}

pub(crate) struct LoginDialogInner {
    pub(crate) state: Mutex<LoginDialogState>,
    pub(crate) request_render: Box<dyn Fn() + Send + Sync>,
}

impl Clone for LoginDialogHandle {
    fn clone(&self) -> Self {
        LoginDialogHandle {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl std::fmt::Debug for LoginDialogHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginDialogHandle").finish()
    }
}

/// Lock the dialog state (a poisoned lock resolves empty rather than
/// panicking the render path).
pub(crate) fn lock_state(this: &LoginDialogInner) -> std::sync::MutexGuard<'_, LoginDialogState> {
    this.state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl LoginDialogHandle {
    pub fn new(options: LoginDialogOptions, request_render: Box<dyn Fn() + Send + Sync>) -> Self {
        LoginDialogHandle {
            inner: Arc::new(LoginDialogInner {
                state: Mutex::new(LoginDialogState {
                    options,
                    rows: Vec::new(),
                    auth_url: None,
                    auth_actions: false,
                    input_visible: false,
                    field: SearchInput::new(),
                    copy_status: None,
                    pending_input: None,
                    pending_continue: None,
                }),
                request_render,
            }),
        }
    }

    /// Run one state edit, then trigger the mount's repaint (every TS
    /// show* method ends with `this.tui.requestRender()`).
    fn edit<F: FnOnce(&mut LoginDialogState)>(&self, edit: F) {
        {
            let mut state = lock_state(&self.inner);
            edit(&mut state);
        }
        (self.inner.request_render)();
    }

    /// Retitle the panel from the provider's display name (the TS
    /// `providerNameOverride` the login flow resolves before the panel's
    /// first content).
    pub fn set_provider_name(&self, provider: &str) {
        lock_state(&self.inner).options.provider = provider.to_string();
        (self.inner.request_render)();
    }

    /// TS `showProgress`: narration for slow steps; the first call also
    /// adds the "Preparing authentication" section title.
    pub fn show_progress(&self, message: &str) {
        self.edit(|state| {
            if state.rows.is_empty() {
                state.start_content();
                state.push_text("Preparing authentication", RowColor::Text);
            }
            state.push_text(message, RowColor::Muted);
        });
    }

    /// TS `showAuth`: the authorization URL (a hyperlink where the
    /// terminal supports OSC 8) plus the instructions or the browser
    /// fallback, and the auth-actions hint row.
    pub fn show_auth(&self, url: &str, instructions: Option<&str>) {
        // The URL row keeps the OSC 8 pair in the row text; the render
        // path re-emits it around the painted cells (the URL is
        // clickable and never re-printed inline).
        let url_text = if crate::hyperlinks::hyperlinks_enabled() {
            format!(
                "{}{}{}",
                crate::hyperlinks::osc8_open(url),
                url,
                crate::hyperlinks::OSC8_CLOSE
            )
        } else {
            url.to_string()
        };
        self.edit(|state| {
            state.start_content();
            state.auth_url = Some(url.to_string());
            state.push_text(url_text, RowColor::Text);
            state.section_spacer();
            match instructions.filter(|text| !text.is_empty()) {
                Some(text) => state.push_instructions(text),
                None => state.push_text("Complete the sign-in in your browser.", RowColor::Muted),
            }
            state.auth_actions = true;
        });
    }

    /// TS `showManualInput`: the manual-paste prompt plus the field.
    pub fn show_manual_input(&self, prompt: &str) -> LoginAnswer<String> {
        self.edit(|state| {
            state.section_spacer();
            state.push_text(prompt, RowColor::Muted);
            state.add_input_field();
        });
        self.wait_for_input()
    }

    /// TS `showPrompt`: a section title, the placeholder example, and the
    /// field (cleared), waiting for the submission.
    pub fn show_prompt(&self, message: &str, placeholder: Option<&str>) -> LoginAnswer<String> {
        self.edit(|state| {
            state.section_spacer();
            state.push_text(message, RowColor::Text);
            if let Some(placeholder) = placeholder {
                state.push_text(format!("e.g., {placeholder}"), RowColor::Muted);
            }
            state.add_input_field();
            state.field.set_value("");
        });
        self.wait_for_input()
    }

    /// TS `showInfo`: informational rows plus the `esc close` hint.
    pub fn show_info(&self, lines: &[String]) {
        self.edit(|state| {
            state.start_content();
            for line in lines {
                state.push_text(line.clone(), RowColor::Plain);
            }
            state.rows.push(ContentRow::Blank);
            state.rows.push(ContentRow::KeyHint {
                hints: vec![("tui.select.cancel", "close")],
            });
        });
    }

    /// TS `showContinueInfo`: informational rows plus the
    /// `enter continue · esc cancel` hint, waiting for the confirm.
    pub fn show_continue_info(&self, lines: &[String]) -> LoginAnswer<()> {
        self.edit(|state| {
            state.start_content();
            for line in lines {
                state.push_text(line.clone(), RowColor::Plain);
            }
            state.rows.push(ContentRow::Blank);
            state.rows.push(ContentRow::KeyHint {
                hints: vec![
                    ("tui.select.confirm", "continue"),
                    ("tui.select.cancel", "cancel"),
                ],
            });
        });
        self.wait_for_continue()
    }

    /// TS `showWaiting`: the waiting message for polling flows.
    pub fn show_waiting(&self, message: &str) {
        self.edit(|state| {
            state.section_spacer();
            state.push_text(message, RowColor::Accent);
            state.auth_actions = true;
        });
    }

    /// TS `abort` (the dialog's cancel): resolve any pending input and
    /// continue as cancelled, so the flow settles.
    pub fn abort(&self) {
        let mut state = lock_state(&self.inner);
        if let Some(resolver) = state.pending_input.take() {
            let _ = resolver.send(Err(anyhow!("Login cancelled")));
        }
        if let Some(resolver) = state.pending_continue.take() {
            let _ = resolver.send(Err(anyhow!("Login cancelled")));
        }
    }

    fn wait_for_input(&self) -> LoginAnswer<String> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        lock_state(&self.inner).pending_input = Some(sender);
        Box::pin(async move {
            receiver
                .await
                .unwrap_or_else(|_| Err(anyhow!("Login cancelled")))
        })
    }

    fn wait_for_continue(&self) -> LoginAnswer<()> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        lock_state(&self.inner).pending_continue = Some(sender);
        Box::pin(async move {
            receiver
                .await
                .unwrap_or_else(|_| Err(anyhow!("Login cancelled")))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn make_handle() -> (LoginDialogHandle, std::sync::Arc<AtomicUsize>) {
        let renders = std::sync::Arc::new(AtomicUsize::new(0));
        let counting = {
            let renders = std::sync::Arc::clone(&renders);
            Box::new(move || {
                renders.fetch_add(1, Ordering::SeqCst);
            }) as Box<dyn Fn() + Send + Sync>
        };
        (
            LoginDialogHandle::new(LoginDialogOptions::new("linear"), counting),
            renders,
        )
    }

    /// One row's plain text.
    fn row_text(row: &ContentRow) -> String {
        match row {
            ContentRow::Blank => String::new(),
            ContentRow::Text { text, .. } => text.clone(),
            ContentRow::KeyHint { .. } => String::new(),
        }
    }

    fn rows(handle: &LoginDialogHandle) -> Vec<String> {
        lock_state(&handle.inner)
            .rows
            .iter()
            .map(row_text)
            .collect()
    }

    /// The first progress call adds the TS section title; the messages
    /// are muted rows.
    #[test]
    fn show_progress_titles_the_first_section() {
        let (handle, renders) = make_handle();
        handle.show_progress("Discovered https://fixture.example");
        handle.show_progress("Exchanging authorization code for tokens…");
        assert_eq!(
            rows(&handle),
            vec![
                String::new(),
                "Preparing authentication".to_string(),
                "Discovered https://fixture.example".to_string(),
                "Exchanging authorization code for tokens…".to_string(),
            ]
        );
        assert_eq!(renders.load(Ordering::SeqCst), 2, "every call repaints");
    }

    /// `showAuth` clears prior content, keeps the URL, and renders the
    /// instructions (or the TS browser fallback).
    #[test]
    fn show_auth_renders_url_instructions_and_fallback() {
        let (handle, _) = make_handle();
        let url = "https://fixture.example/authorize?code_challenge=x";
        handle.show_progress("Discovered https://fixture.example");
        handle.show_auth(url, Some("Complete login in your browser."));
        assert_eq!(
            rows(&handle),
            vec![
                String::new(),
                url.to_string(),
                String::new(),
                "Complete login in your browser.".to_string(),
            ]
        );
        assert_eq!(
            lock_state(&handle.inner).auth_url.as_deref(),
            Some(url),
            "the copy binding has a URL to copy"
        );
        // No instructions: the TS fallback line.
        handle.show_auth(url, None);
        assert_eq!(
            rows(&handle),
            vec![
                String::new(),
                url.to_string(),
                String::new(),
                "Complete the sign-in in your browser.".to_string(),
            ]
        );
    }

    /// The URL row wraps the link in an OSC 8 pair only in terminals
    /// known to support hyperlinks (TS `getCapabilities().hyperlinks`).
    #[test]
    fn show_auth_links_the_url_only_in_capable_terminals() {
        crate::hyperlinks::set_hyperlinks_override(Some(true));
        let (handle, _) = make_handle();
        handle.show_auth("https://fixture.example/authorize", None);
        let linked = rows(&handle)[1].clone();
        assert_eq!(
            linked,
            format!(
                "{}https://fixture.example/authorize{}",
                crate::hyperlinks::osc8_open("https://fixture.example/authorize"),
                crate::hyperlinks::OSC8_CLOSE
            )
        );
        crate::hyperlinks::set_hyperlinks_override(Some(false));
        let (handle, _) = make_handle();
        handle.show_auth("https://fixture.example/authorize", None);
        assert_eq!(
            rows(&handle)[1],
            "https://fixture.example/authorize",
            "the bare URL renders when hyperlinks are off"
        );
        crate::hyperlinks::set_hyperlinks_override(None);
    }

    /// The TS code-parse instructions (`Code: xyz` / `Enter code: xyz`):
    /// a blank row, the muted "Verification code" label, a bold code.
    #[test]
    fn show_auth_parses_verification_codes() {
        let (handle, _) = make_handle();
        handle.show_auth("https://x.dev/a", Some("Code: ABC-123"));
        assert_eq!(
            rows(&handle),
            vec![
                String::new(),
                "https://x.dev/a".to_string(),
                String::new(),
                String::new(),
                "Verification code".to_string(),
                "ABC-123".to_string(),
            ]
        );
        handle.show_auth("https://x.dev/a", Some("Enter code: xyz"));
        assert_eq!(
            rows(&handle),
            vec![
                String::new(),
                "https://x.dev/a".to_string(),
                String::new(),
                String::new(),
                "Verification code".to_string(),
                "xyz".to_string(),
            ]
        );
        // Anything else stays the instruction text (the TS no-match arm).
        handle.show_auth("https://x.dev/a", Some("Open the code in your app"));
        assert_eq!(rows(&handle)[4], "Open the code in your app");
    }

    /// The manual-input and prompt rows (TS `showManualInput` /
    /// `showPrompt`): the prompt, the field, and the placeholder example.
    #[test]
    fn show_prompt_and_manual_input_lay_out_the_field_blocks() {
        let (handle, _) = make_handle();
        handle.show_auth("https://x.dev/a", None);
        handle.show_manual_input("Paste redirect URL below, or complete login in browser:");
        assert_eq!(
            rows(&handle),
            vec![
                String::new(),
                "https://x.dev/a".to_string(),
                String::new(),
                "Complete the sign-in in your browser.".to_string(),
                String::new(),
                "Paste redirect URL below, or complete login in browser:".to_string(),
            ]
        );
        assert!(
            lock_state(&handle.inner).input_visible,
            "the paste field is mounted"
        );
        // The prompt block: section title plus the muted example.
        handle.show_prompt(
            "Paste the authorization code or full redirect URL:",
            Some("http://127.0.0.1:9/callback"),
        );
        assert_eq!(
            rows(&handle)[6],
            "Paste the authorization code or full redirect URL:"
        );
        assert_eq!(rows(&handle)[7], "e.g., http://127.0.0.1:9/callback");
    }

    /// `abort` resolves pending prompts and continues as cancelled, so the
    /// flow settles instead of waiting on an unmounted panel.
    #[tokio::test]
    async fn abort_resolves_pending_answers_as_cancelled() {
        let (handle, _) = make_handle();
        let input =
            handle.show_manual_input("Paste redirect URL below, or complete login in browser:");
        let continuing = handle.show_continue_info(&["Connected to the browser flow.".to_string()]);
        handle.abort();
        assert_eq!(input.await.unwrap_err().to_string(), "Login cancelled");
        assert_eq!(continuing.await.unwrap_err().to_string(), "Login cancelled");
    }

    /// The waiting row for polling flows keeps the accent color and the
    /// floating auth-actions row.
    #[test]
    fn show_waiting_renders_the_accent_row() {
        let (handle, _) = make_handle();
        handle.show_auth("https://x.dev/a", None);
        handle.show_waiting("Waiting for browser authentication...");
        assert_eq!(
            rows(&handle),
            vec![
                String::new(),
                "https://x.dev/a".to_string(),
                String::new(),
                "Complete the sign-in in your browser.".to_string(),
                String::new(),
                "Waiting for browser authentication...".to_string(),
            ]
        );
    }

    /// `set_provider_name` retitles the panel (the composition root
    /// resolves the integration label before the first content) and
    /// repaints.
    #[test]
    fn set_provider_name_retitles_and_renders() {
        let (handle, renders) = make_handle();
        handle.set_provider_name("Linear");
        assert_eq!(lock_state(&handle.inner).options.provider, "Linear");
        assert_eq!(renders.load(Ordering::SeqCst), 1);
    }
}
