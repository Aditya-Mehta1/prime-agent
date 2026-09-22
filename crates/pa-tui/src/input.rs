//! One crossterm input reader per process at a time.
//!
//! TUI surfaces hand the terminal to each other inside one process: the
//! agents-view loop opens chat sessions and reopens the view, and `/resume`
//! chains open one session after another. crossterm events are process
//! global, so two concurrent reader threads race for the same bytes; the
//! losing (older) thread can read a keypress after its channel is gone and
//! drop it — the user's key vanishes. [`spawn_terminal_reader`] joins the
//! still-running reader from the previous surface before starting the next
//! one, so exactly one reader is alive at any time.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Reader {
    handle: std::thread::JoinHandle<()>,
    stop: Arc<AtomicBool>,
}

/// The reader of the previous TUI surface in this process, if any.
static PREVIOUS_READER: Mutex<Option<Reader>> = Mutex::new(None);

const POLL_TIMEOUT_MS: u64 = 100;

/// Start the terminal input reader. `on_event` runs for every crossterm
/// event; returning `false` stops the reader (the caller stops it when its
/// channel dies). The reader from the previous surface is stopped and joined
/// first so it cannot steal events from the new one.
pub(crate) fn spawn_terminal_reader<F>(mut on_event: F)
where
    F: FnMut(crossterm::event::Event) -> bool + Send + 'static,
{
    let mut previous = PREVIOUS_READER
        .lock()
        .expect("the input-reader registry lock is poisoned");
    if let Some(reader) = previous.take() {
        reader.stop.store(true, Ordering::Relaxed);
        let _ = reader.handle.join();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let handle = std::thread::spawn(move || loop {
        if thread_stop.load(Ordering::Relaxed) {
            break;
        }
        match crossterm::event::poll(Duration::from_millis(POLL_TIMEOUT_MS)) {
            Ok(false) => continue,
            Ok(true) => match crossterm::event::read() {
                Ok(event) => {
                    if !on_event(event) {
                        break;
                    }
                }
                Err(_) => break,
            },
            Err(_) => break,
        }
    });
    *previous = Some(Reader { handle, stop });
}
