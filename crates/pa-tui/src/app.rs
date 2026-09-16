//! Terminal app: crossterm event loop driving an [`AgentView`] against a
//! [`SessionStream`]. Both the interactive product surface and the replay
//! verifier binary run through this single loop.

use crate::editor::{Editor, EditorEvent};
use crate::keys::key_event_to_id;
use crate::session::{SessionEvent, SessionStream};
use crate::theme::{ColorMode, Theme};
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
    let mode = if std::env::var("COLORTERM")
        .ok()
        .is_some_and(|v| v.contains("truecolor"))
        || std::env::var("TERM")
            .ok()
            .is_some_and(|v| v.contains("256color"))
    {
        ColorMode::TrueColor
    } else {
        ColorMode::Color256
    };
    Theme::builtin(name, mode)
}

/// Run the view against a session stream until the stream ends and the user
/// exits. `on_submit` receives editor submissions (unused in replay mode).
pub fn run_app(
    mut stream: Box<dyn SessionStream>,
    options: AppOptions,
    mut on_submit: Box<dyn FnMut(&str) + Send>,
) -> Result<()> {
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
                        view.model_label = model_id.clone();
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
        view.editor.set_terminal_rows(h);
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

fn draw(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    view: &mut AgentView,
) -> Result<()> {
    let area = terminal.size()?;
    let width = area.width as usize;
    let height = area.height as usize;
    let footer = view.render_footer(width);
    let frame = view.render_editor(width, area.height);
    let footer_lines: usize = if footer.is_empty() { 0 } else { 1 };
    let editor_lines = frame.lines.len();
    let transcript_height = height.saturating_sub(editor_lines + footer_lines);
    let transcript = view.render_transcript(width);

    terminal.draw(|f| {
        use ratatui::layout::Rect;
        let mut y = 0u16;
        // Transcript: show the tail.
        if transcript_height > 0 {
            let skip = transcript.len().saturating_sub(transcript_height);
            let lines: Vec<ratatui::text::Line<'static>> = transcript[skip..]
                .iter()
                .map(crate::markdown::to_ratatui_line)
                .collect();
            let area_rect = Rect::new(0, y, area.width, transcript_height as u16);
            let mut text = ratatui::text::Text::from(lines);
            text.style = ratatui::style::Style::default();
            f.render_widget(text, area_rect);
            y += transcript_height as u16;
        }
        // Editor block.
        let editor_area = Rect::new(0, y, area.width, editor_lines as u16);
        let lines: Vec<ratatui::text::Line<'static>> = frame
            .lines
            .iter()
            .map(crate::markdown::to_ratatui_line)
            .collect();
        f.render_widget(ratatui::text::Text::from(lines), editor_area);
        // Cursor.
        if let (Some(row), Some(col)) = (frame.cursor_row, frame.cursor_col) {
            if (y as usize + row) < height {
                f.set_cursor_position(ratatui::layout::Position::new(
                    col.min(area.width as usize - 1) as u16,
                    y + row as u16,
                ));
            }
        }
        // Footer.
        if footer_lines == 1 {
            let foot_area = Rect::new(0, y + editor_lines as u16, area.width, 1);
            f.render_widget(
                ratatui::text::Text::from(vec![crate::markdown::to_ratatui_line(&footer)]),
                foot_area,
            );
        }
    })?;
    Ok(())
}

/// Render one frame to stdout as plain text (headless structural dump used by
/// the tmux verifier and diff tests). ANSI styling is stripped.
pub fn render_frame_text(view: &mut AgentView, width: u16, height: u16) -> Vec<String> {
    let mut buf = String::new();
    let footer = view.render_footer(width as usize);
    let frame = view.render_editor(width as usize, height);
    let footer_lines: usize = if footer.is_empty() { 0 } else { 1 };
    let editor_lines = frame.lines.len();
    let transcript_height = (height as usize).saturating_sub(editor_lines + footer_lines);
    let transcript = view.render_transcript(width as usize);
    let skip = transcript.len().saturating_sub(transcript_height);
    let mut lines: Vec<String> = Vec::new();
    for line in &transcript[skip..] {
        lines.push(line.iter().map(|s| s.content.as_str()).collect());
    }
    while lines.len() < transcript_height {
        lines.push(String::new());
    }
    for line in &frame.lines {
        lines.push(line.iter().map(|s| s.content.as_str()).collect());
    }
    if footer_lines == 1 {
        lines.push(footer.iter().map(|s| s.content.as_str()).collect());
    }
    while lines.len() < height as usize {
        lines.push(String::new());
    }
    let _ = &mut buf;
    lines
}

#[allow(dead_code)]
fn unused(_: TerminalOptions, _: Viewport) {}
