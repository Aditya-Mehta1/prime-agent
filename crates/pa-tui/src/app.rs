//! Terminal app: crossterm event loop driving an [`AgentView`] against a
//! [`SessionStream`]. Both the interactive product surface and the replay
//! verifier binary run through this single loop.

use crate::editor::{Editor, EditorEvent};
use crate::keys::key_event_to_id;
use crate::session::{SessionEvent, SessionStream};
use crate::theme::Theme;
use crate::view::AgentView;
use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::io::stdout;
use std::time::Duration;

pub struct AppOptions {
    pub theme: String,
    /// Replay delay per entry while streaming history (ms). 0 = instant load.
    pub replay_delay_ms: u64,
    /// Auto-exit after this many ms of runtime (headless verification).
    pub auto_exit_ms: Option<u64>,
}

impl Default for AppOptions {
    fn default() -> Self {
        Self {
            theme: "prime".to_string(),
            replay_delay_ms: 0,
            auto_exit_ms: None,
        }
    }
}

pub fn load_theme(name: &str) -> Theme {
    let mode = crate::theme::detect_color_mode();
    // The default brand theme when the caller passes none (empty) or an
    // unknown name; only known builtins resolve.
    let known = ["prime", "dark", "light"];
    let name = if known.contains(&name) { name } else { "prime" };
    Theme::builtin(name, mode)
}

/// Run the view against a session stream until the stream ends and the user
/// exits. `on_submit` receives editor submissions (unused in replay mode).
pub fn run_app(
    mut stream: Box<dyn SessionStream>,
    options: AppOptions,
    mut on_submit: Box<dyn FnMut(&str) + Send>,
) -> Result<()> {
    // The TS theme emits raw ANSI color codes regardless of NO_COLOR; match
    // that so the same terminal renders the same frames either way.
    crossterm::style::force_color_output(true);
    terminal::enable_raw_mode()?;
    crossterm::execute!(stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let theme = load_theme(&options.theme);
    let mut view = AgentView::new(theme);
    let mut running = true;
    let start = std::time::Instant::now();
    let mut stream_ended = false;

    loop {
        // Drain stream events.
        if !stream_ended {
            match stream.poll()? {
                SessionEvent::Item(item) => {
                    if let crate::session::TranscriptItem::ModelChange { model_id, .. } = &item {
                        view.chrome.model_id = Some(model_id.clone());
                    }
                    view.push(item);
                    if options.replay_delay_ms > 0 {
                        std::thread::sleep(Duration::from_millis(options.replay_delay_ms));
                    }
                }
                SessionEvent::End => stream_ended = true,
            }
        }

        let (_w, h) = crossterm::terminal::size()?;
        view.set_terminal_rows(h);
        draw(&mut terminal, &mut view)?;

        // Input.
        let timeout = Duration::from_millis(if stream_ended { 50 } else { 5 });
        if crossterm::event::poll(timeout)? {
            match crossterm::event::read()? {
                Event::Key(key) => {
                    handle_key(&mut view, key, &mut running, &mut *on_submit);
                }
                Event::Paste(text) => {
                    view.editor.handle_paste(&text);
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        if let Some(ms) = options.auto_exit_ms {
            if start.elapsed() >= Duration::from_millis(ms) {
                running = false;
            }
        }
        if !running && stream_ended {
            break;
        }
        if !stream_ended {
            continue;
        }
        if !running {
            break;
        }
    }

    terminal::disable_raw_mode()?;
    crossterm::execute!(stdout(), LeaveAlternateScreen)?;
    Ok(())
}

fn handle_key(
    view: &mut AgentView,
    key: KeyEvent,
    running: &mut bool,
    on_submit: &mut dyn FnMut(&str),
) {
    // App-level bindings (coding-agent keybindings.ts):
    // ctrl+c exits the app shell in replay mode; escape cancels autocomplete.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if view.editor.is_showing_autocomplete() {
            view.editor.cancel_autocomplete();
            return;
        }
        *running = false;
        return;
    }
    if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
        *running = false;
        return;
    }
    if key.code == KeyCode::Esc {
        view.editor.cancel_autocomplete();
        return;
    }
    let Some(id) = key_event_to_id(&key) else {
        return;
    };
    let is_paste_marker_key = false;
    let _ = is_paste_marker_key;
    view.editor.handle_input(&id);
    dispatch_events(&mut view.editor, on_submit);
}

pub fn dispatch_events(editor: &mut Editor, on_submit: &mut dyn FnMut(&str)) {
    for ev in editor.take_events() {
        match ev {
            EditorEvent::Submitted(text) => {
                editor.add_to_history(&text);
                on_submit(&text);
            }
            EditorEvent::Changed(_) | EditorEvent::AutocompleteToggled(_) => {}
        }
    }
}

pub(crate) fn draw(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    view: &mut AgentView,
) -> Result<()> {
    let area = terminal.size()?;
    let frame_area = ratatui::layout::Rect::new(0, 0, area.width, area.height);
    let width = area.width as usize;
    let height = area.height as usize;
    let frame = view.render_frame(width, height);
    let cursor = view.frame_cursor();
    terminal.draw(|f| {
        let lines: Vec<ratatui::text::Line<'static>> =
            frame.iter().map(crate::markdown::to_ratatui_line).collect();
        f.render_widget(ratatui::text::Text::from(lines), frame_area);
        if let Some((row, col)) = cursor {
            if row < height && col < width {
                f.set_cursor_position(ratatui::layout::Position::new(col as u16, row as u16));
            }
        }
    })?;
    Ok(())
}

/// Render one frame as plain text (headless structural dump used by the tmux
/// verifier and diff tests). ANSI styling is stripped.
pub fn render_frame_text(view: &mut AgentView, width: u16, height: u16) -> Vec<String> {
    let frame = view.render_frame(width as usize, height as usize);
    frame
        .iter()
        .map(|line| line.iter().map(|s| s.content.as_str()).collect())
        .collect()
}

#[allow(dead_code)]
fn unused(_: TerminalOptions, _: Viewport) {}
