//! The windowed store lifecycle: seed, metadata, and the full-history
//! upgrade (fast-session-open PR 2).
//!
//! The open path seeds the store from the window loader
//! (`pa_core::session::window_loader`): the compaction-bounded context
//! region plus a display floor, or the whole leaf chain by closure,
//! instead of the whole file. The window is a READ optimization only — the
//! durable file is never truncated — so every windowed mutation is
//! append-only (`persist_appended`) and a full rewrite upgrades the store
//! first. [`SessionFile::ensure_full_history`] is the explicit async
//! upgrade the full-history consumers run (export, fork, fork-point
//! listing, pre-window tree navigation); it re-reads the file and absorbs
//! anything the live store appended while the parse ran.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::session_store::{SessionEntry, SessionFile, SessionHeader};
use crate::worker::SessionCore;

/// Distinguishes one windowed seed from the next; an in-flight full-history
/// upgrade compares it before installing its parse, so a store replaced
/// underneath the parse (session switch) drops the stale result.
static WINDOW_GENERATION: AtomicU64 = AtomicU64::new(1);

/// The window byte span the `session_open` event reports: the seeded
/// records' on-disk size (the loader's scan span minus the records it
/// rejected), newline included.
fn window_bytes_of(records: &[Vec<u8>]) -> u64 {
    records.iter().map(|record| record.len() as u64 + 1).sum()
}

/// The loader output the store seeds from: the raw session-file records
/// (newline excluded), oldest first, plus whether the cut is a compaction
/// checkpoint (its boundary rides the retained records).
#[derive(Debug, Clone, Default)]
pub(crate) struct WindowSeed {
    pub display_floor_records: Vec<Vec<u8>>,
    pub retained_records: Vec<Vec<u8>>,
    pub checkpoint: bool,
}

/// The window state of a store opened through the window loader.
#[derive(Debug, Clone)]
pub(crate) struct WindowInfo {
    /// Held entry count at open (display floor + retained suffix; the
    /// header is not an entry). The `session_open` telemetry `entries`
    /// value for a windowed open.
    pub window_entries: usize,
    /// Byte span the window covers in the session file. The `session_open`
    /// telemetry `window_bytes` value; 0 when the open fell back to a full
    /// parse.
    pub window_bytes: u64,
    /// The compaction cut the window was taken at (`firstKeptEntryId`),
    /// when the window is compaction-bounded.
    pub boundary_entry_id: Option<String>,
    /// The oldest held entry (the display floor's floor); the `before`
    /// cursor the history backfill pager starts from.
    pub oldest_entry_id: Option<String>,
    pub generation: u64,
}

/// The session header for a windowed open: a bounded first-line read, so
/// the open never pays the full-file read the plain loader spends on the
/// header alone. The header line is small in practice; the cap only guards
/// a corrupt first line.
fn read_session_header_bounded(path: &std::path::Path) -> Result<SessionHeader> {
    use std::io::Read;
    const HEADER_READ_CAP: u64 = 64 * 1024;
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("read session file {}", path.display()))?;
    let mut head = Vec::new();
    (&mut file)
        .take(HEADER_READ_CAP)
        .read_to_end(&mut head)
        .with_context(|| format!("read session file {}", path.display()))?;
    let first_line_end = head
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| anyhow!("empty session file {}", path.display()))?;
    let header_value: serde_json::Value = serde_json::from_slice(&head[..first_line_end])
        .with_context(|| format!("invalid session header in {}", path.display()))?;
    if header_value.get("type").and_then(serde_json::Value::as_str) != Some("session") {
        return Err(anyhow!("missing session header in {}", path.display()));
    }
    serde_json::from_value(header_value)
        .with_context(|| format!("invalid session header in {}", path.display()))
}

impl SessionFile {
    /// Seed a windowed store from the loader's window entries (oldest
    /// first). Everything seeded came from disk, so the store starts fully
    /// persisted.
    pub(crate) fn from_window_parts(
        path: PathBuf,
        header: SessionHeader,
        entries: Vec<SessionEntry>,
        window: WindowInfo,
    ) -> Self {
        let mut file = SessionFile {
            path,
            header,
            entries: Vec::new(),
            by_id: std::collections::HashMap::new(),
            leaf_id: None,
            window: Some(window),
            persisted_entries: 0,
        };
        for entry in entries {
            file.push_index(entry);
        }
        file.persisted_entries = file.entries.len();
        file
    }

    /// The window state, when the store was opened through the window
    /// loader; `None` means the store holds the full chain.
    pub(crate) fn window_info(&self) -> Option<&WindowInfo> {
        self.window.as_ref()
    }

    /// Open a session file through the window loader: the store holds the
    /// context region (a compaction checkpoint, or the whole leaf chain by
    /// closure) instead of the whole file. The leaf is the file's last
    /// record — the daemon's resume leaf. A `FullRead` outcome (header
    /// below v3, non-canonical record heads, duplicate-id divergence) falls
    /// back to the plain full parse.
    pub(crate) fn open_windowed(path: &Path) -> Result<Self> {
        Self::open_windowed_at(path, None)
    }

    /// [`SessionFile::open_windowed`] at an explicit leaf (the golden
    /// replay's compaction-branch leaf; production always passes `None`).
    pub(crate) fn open_windowed_at(path: &Path, leaf_id: Option<&str>) -> Result<Self> {
        let window = pa_core::session::window_loader::load_session_window(path, leaf_id);
        if matches!(
            window.source,
            pa_core::session::window_loader::WindowSource::FullRead
        ) {
            return SessionFile::open(path);
        }
        let checkpoint = matches!(
            window.source,
            pa_core::session::window_loader::WindowSource::CompactionCheckpoint
        );
        Self::seed_from_window(
            path,
            WindowSeed {
                display_floor_records: window.display_floor_records,
                retained_records: window.retained_records,
                checkpoint,
            },
        )
    }

    /// Seed the store from the loader's window records: display floor then
    /// retained suffix, both oldest first — the store's entry order. The
    /// boundary is inclusive, so the first retained record is the cut's
    /// `firstKeptEntryId`.
    pub(crate) fn seed_from_window(path: &Path, seed: WindowSeed) -> Result<Self> {
        let header = read_session_header_bounded(path)?;
        let mut entries: Vec<SessionEntry> =
            Vec::with_capacity(seed.display_floor_records.len() + seed.retained_records.len());
        for record in seed
            .display_floor_records
            .iter()
            .chain(seed.retained_records.iter())
        {
            match serde_json::from_slice::<SessionEntry>(record) {
                Ok(entry) => entries.push(entry),
                // The loader only emits canonical records; a store-side
                // decode miss is skipped like the plain loader skips a
                // malformed line.
                Err(_) => continue,
            }
        }
        let boundary_entry_id = if seed.checkpoint {
            seed.retained_records
                .first()
                .and_then(|record| serde_json::from_slice::<SessionEntry>(record).ok())
                .map(|entry| entry.id)
        } else {
            None
        };
        let info = WindowInfo {
            window_entries: entries.len(),
            window_bytes: window_bytes_of(&seed.display_floor_records)
                + window_bytes_of(&seed.retained_records),
            boundary_entry_id,
            oldest_entry_id: entries.first().map(|entry| entry.id.clone()),
            generation: WINDOW_GENERATION.fetch_add(1, Ordering::Relaxed),
        };
        Ok(Self::from_window_parts(
            path.to_path_buf(),
            header,
            entries,
            info,
        ))
    }

    /// Upgrade the store to the full entry chain (synchronous). A no-op on a
    /// full store. The live tail is flushed first so the parse sees it;
    /// entries appended while the parse ran are absorbed by id afterwards.
    pub(crate) fn ensure_full_history(&mut self) -> Result<bool> {
        let Some((path, generation)) = self.begin_full_history_upgrade()? else {
            return Ok(false);
        };
        let full = SessionFile::open(&path)?;
        Ok(self.finish_full_history_upgrade(full, generation))
    }

    /// Flush the live tail and report the upgrade inputs: `None` when the
    /// store already holds the full chain. The parse itself runs off the
    /// command loop (`spawn_blocking`); the caller installs it with
    /// [`SessionFile::finish_full_history_upgrade`].
    pub(crate) fn begin_full_history_upgrade(&mut self) -> Result<Option<(PathBuf, u64)>> {
        let Some(generation) = self.window.as_ref().map(|window| window.generation) else {
            return Ok(None);
        };
        self.persist_appended()?;
        Ok(Some((self.path.clone(), generation)))
    }

    /// Install the full-history parse. The store must still be the windowed
    /// seed the parse was started for (same generation); anything the live
    /// store appended while the parse ran — flush-through entries the parse
    /// predates — is re-appended by id. Returns whether the install ran.
    pub(crate) fn finish_full_history_upgrade(
        &mut self,
        full: SessionFile,
        generation: u64,
    ) -> bool {
        let Some(window) = self.window.as_ref() else {
            return false;
        };
        if window.generation != generation {
            return false;
        }
        let appended_since_parse: Vec<SessionEntry> = self
            .entries
            .iter()
            .filter(|entry| !full.by_id.contains_key(&entry.id))
            .cloned()
            .collect();
        self.path = full.path;
        self.header = full.header;
        self.entries = full.entries;
        self.by_id = full.by_id;
        self.leaf_id = full.leaf_id;
        self.window = None;
        self.persisted_entries = self.entries.len();
        for entry in appended_since_parse {
            self.push_index(entry);
        }
        self.persisted_entries = self.entries.len();
        true
    }
}

/// The process-wide `session_open` telemetry client (one per worker
/// process — workers are one-process-per-session). Built on first use from
/// the creating session's settings; `None` when telemetry is disabled or
/// the install identity is unavailable.
static SESSION_OPEN_CLIENT: std::sync::OnceLock<Option<pa_telemetry::TelemetryClient>> =
    std::sync::OnceLock::new();

fn session_open_client(
    agent_dir: &std::path::Path,
    cwd: &std::path::Path,
    telemetry_disabled: bool,
) -> Option<&'static pa_telemetry::TelemetryClient> {
    if telemetry_disabled {
        return None;
    }
    SESSION_OPEN_CLIENT
        .get_or_init(|| {
            let settings = pa_core::settings::SettingsManager::create(cwd, agent_dir);
            Some(pa_core::session_engine::telemetry::build_client(
                &settings, agent_dir,
            ))
        })
        .as_ref()
}

/// The `session_open` adoption event (`docs/telemetry-events.md`,
/// fast-session-open PR 2): the open path's cost and window shape, emitted
/// at the daemon's open seams. Best-effort — telemetry never fails the open.
pub(crate) fn emit_session_open(
    agent_dir: &std::path::Path,
    cwd: &std::path::Path,
    telemetry_disabled: bool,
    open_ms: u64,
    store: &SessionFile,
) {
    let Some(client) = session_open_client(agent_dir, cwd, telemetry_disabled) else {
        return;
    };
    let file_bytes = std::fs::metadata(&store.path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let (window_bytes, from_window) = store
        .window_info()
        .map(|window| (window.window_bytes, true))
        .unwrap_or((0, false));
    let mut properties = pa_telemetry::Properties::new();
    properties.set("duration_ms", serde_json::Value::from(open_ms));
    properties.set(
        "entries",
        serde_json::Value::from(store.entries().len() as u64),
    );
    properties.set("window_bytes", serde_json::Value::from(window_bytes));
    properties.set("file_bytes", serde_json::Value::from(file_bytes));
    properties.set("from_window", serde_json::Value::from(from_window));
    client.track("session_open", properties);
}

/// The async full-history upgrade the full-history consumers run before
/// reading the whole chain (export, fork, pre-window tree navigation): the
/// live tail flushes, the file parses on a blocking thread — never the
/// command loop — and the parse installs under the core lock. A no-op when
/// the store already holds the full chain.
pub(crate) async fn ensure_store_full_history(
    core: &Arc<Mutex<SessionCore>>,
) -> Result<(), String> {
    let upgrade = {
        let mut core = core.lock().unwrap();
        let Some(store) = core.store.as_mut() else {
            return Ok(());
        };
        store
            .begin_full_history_upgrade()
            .map_err(|error| format!("full history upgrade failed: {error:#}"))?
    };
    let Some((path, generation)) = upgrade else {
        return Ok(());
    };
    let parsed = tokio::task::spawn_blocking(move || SessionFile::open(&path))
        .await
        .map_err(|join| format!("full history parse failed: {join}"))?
        .map_err(|error| format!("full history parse failed: {error:#}"))?;
    let mut core = core.lock().unwrap();
    if let Some(store) = core.store.as_mut() {
        store.finish_full_history_upgrade(parsed, generation);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::{session_file_name, SessionFile};
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pa-daemon-window-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A persisted session with `count` user messages; returns the file path
    /// and the entry ids the appends returned.
    fn persisted_session(dir: &std::path::Path, count: usize) -> (PathBuf, Vec<String>) {
        let mut session = SessionFile::create("/tmp", None, 0);
        let path = dir.join(session_file_name(session.session_id()));
        session.set_path(path.clone());
        let mut ids = Vec::new();
        for i in 0..count {
            let id = session.append_message(json!({
                "role": "user", "content": format!("m{i}"), "timestamp": i as u64
            }));
            ids.push(id);
        }
        session.rewrite().unwrap();
        (path, ids)
    }

    /// A windowed store holding only the last `held` entries.
    fn windowed_seed(path: &std::path::Path, held: usize) -> SessionFile {
        let full = SessionFile::open(path).unwrap();
        let header = full.header.clone();
        let entries: Vec<SessionEntry> = full.entries()[full.entries().len() - held..].to_vec();
        let window = WindowInfo {
            window_entries: entries.len(),
            window_bytes: 0,
            boundary_entry_id: None,
            oldest_entry_id: entries.first().map(|entry| entry.id.clone()),
            generation: WINDOW_GENERATION.fetch_add(1, Ordering::Relaxed),
        };
        SessionFile::from_window_parts(path.to_path_buf(), header, entries, window)
    }

    #[test]
    fn windowed_store_persists_appends_without_touching_the_pre_window_file() {
        let dir = temp_dir();
        let (path, ids) = persisted_session(&dir, 6);
        let before = fs::read_to_string(&path).unwrap();

        let mut store = windowed_seed(&path, 2);
        let new_id =
            store.append_message(json!({"role": "user", "content": "m6", "timestamp": 6u64}));
        store.persist_appended().unwrap();

        // The durable file grew by exactly the appended line.
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.starts_with(&before));
        assert_eq!(after.lines().count(), before.lines().count() + 1);

        let reloaded = SessionFile::open(&path).unwrap();
        assert_eq!(reloaded.message_count(), 7);
        assert!(reloaded.window_info().is_none());
        assert_eq!(reloaded.leaf_id().map(str::to_string), Some(new_id));
        let _ = ids;
    }

    #[test]
    fn windowed_rewrite_upgrades_to_the_full_chain_first() {
        let dir = temp_dir();
        let (path, _) = persisted_session(&dir, 6);

        let mut store = windowed_seed(&path, 2);
        store.append_message(json!({"role": "user", "content": "m6", "timestamp": 6u64}));
        store.rewrite().unwrap();

        assert!(store.window_info().is_none());
        assert_eq!(store.message_count(), 7);
        let on_disk = SessionFile::open(&path).unwrap();
        assert_eq!(on_disk.message_count(), 7);
        assert_eq!(on_disk.entries().len(), store.entries().len());
    }

    #[test]
    fn ensure_full_history_restores_the_full_chain_and_is_idempotent() {
        let dir = temp_dir();
        let (path, ids) = persisted_session(&dir, 6);

        let mut store = windowed_seed(&path, 2);
        assert!(store.window_info().is_some());
        assert_eq!(store.message_count(), 2);
        assert_eq!(store.leaf_id().map(str::to_string), Some(ids[5].clone()));

        assert!(store.ensure_full_history().unwrap());
        assert!(store.window_info().is_none());
        assert_eq!(store.message_count(), 6);
        assert_eq!(store.leaf_id().map(str::to_string), Some(ids[5].clone()));

        // The second upgrade is a no-op on the full store.
        assert!(!store.ensure_full_history().unwrap());
    }

    #[test]
    fn the_full_history_upgrade_absorbs_appends_made_while_the_parse_ran() {
        let dir = temp_dir();
        let (path, _) = persisted_session(&dir, 6);

        let mut store = windowed_seed(&path, 2);
        let (parse_path, generation) = store
            .begin_full_history_upgrade()
            .unwrap()
            .expect("windowed store");

        let parsed = SessionFile::open(&parse_path).unwrap();
        assert_eq!(parsed.message_count(), 6);

        // The live store keeps appending while the background parse runs;
        // the flush-through lands the line on disk after the parse read it.
        store.append_message(json!({"role": "user", "content": "m6", "timestamp": 6u64}));
        store.persist_appended().unwrap();
        assert!(store.finish_full_history_upgrade(parsed, generation));

        assert!(store.window_info().is_none());
        assert_eq!(store.message_count(), 7);
        let on_disk = SessionFile::open(&path).unwrap();
        assert_eq!(on_disk.message_count(), 7);
        assert_eq!(on_disk.entries().len(), store.entries().len());
    }

    #[test]
    fn a_stale_upgrade_install_is_dropped_when_the_store_changed_underneath() {
        let dir = temp_dir();
        let (path, _) = persisted_session(&dir, 6);

        let mut store = windowed_seed(&path, 2);
        let (parse_path, generation) = store
            .begin_full_history_upgrade()
            .unwrap()
            .expect("windowed store");
        let parsed = SessionFile::open(&parse_path).unwrap();

        // The worker switched sessions mid-parse: the store is a different
        // windowed seed (new generation), so the stale parse is dropped.
        let mut switched = windowed_seed(&path, 3);
        assert!(!switched.finish_full_history_upgrade(parsed, generation));
        assert!(switched.window_info().is_some());
    }

    #[test]
    fn the_windowed_context_folds_identically_to_the_full_parse() {
        let dir = temp_dir();
        let mut session = SessionFile::create("/tmp", None, 0);
        let path = dir.join(session_file_name(session.session_id()));
        session.set_path(path.clone());
        let mut ids = Vec::new();
        for i in 0..4 {
            let id = session.append_message(json!({
                "role": "user", "content": format!("m{i}"), "timestamp": i as u64
            }));
            ids.push(id);
        }
        // The compaction cut keeps the second message; the durable row lands
        // after m3, the post-compaction messages after it.
        session
            .persist_entry(
                "compaction",
                json!({"summary": "the story", "firstKeptEntryId": ids[1], "tokensBefore": 12}),
            )
            .unwrap();
        for i in 4..6 {
            session.append_message(json!({
                "role": "user", "content": format!("m{i}"), "timestamp": i as u64
            }));
        }
        session.rewrite().unwrap();

        let full = SessionFile::open(&path).unwrap();
        let full_messages = full.messages();
        assert_eq!(full_messages.len(), 6); // summary + m1..m5

        // The window holds the on-path entries from the kept message
        // (firstKeptEntryId) through the leaf: m1, m2, m3, the compaction
        // row, m4, m5. The fold must not be able to tell.
        let keep_from = full
            .entries()
            .iter()
            .position(|entry| entry.id == ids[1])
            .unwrap();
        let entries: Vec<SessionEntry> = full.entries()[keep_from..].to_vec();
        let window = WindowInfo {
            window_entries: entries.len(),
            window_bytes: 0,
            boundary_entry_id: Some(ids[1].clone()),
            oldest_entry_id: entries.first().map(|entry| entry.id.clone()),
            generation: WINDOW_GENERATION.fetch_add(1, Ordering::Relaxed),
        };
        let store =
            SessionFile::from_window_parts(path.clone(), full.header.clone(), entries, window);

        assert_eq!(
            serde_json::to_vec(&store.messages()).unwrap(),
            serde_json::to_vec(&full_messages).unwrap()
        );
    }

    /// The window seed decodes raw file records in order: display floor
    /// first, then the retained suffix, with the checkpoint boundary taken
    /// from the first retained record.
    #[test]
    fn the_window_seed_decodes_records_in_order_with_the_boundary() {
        let dir = temp_dir();
        let (path, ids) = persisted_session(&dir, 6);
        let full = SessionFile::open(&path).unwrap();

        // Records as the loader hands them over: raw bytes, no newline.
        let record_of = |id: &str| -> Vec<u8> {
            let entry = full.entries().iter().find(|entry| entry.id == id).unwrap();
            let mut line = serde_json::to_vec(entry).unwrap();
            line.push(b'\n');
            line[..line.len() - 1].to_vec()
        };
        let seed = WindowSeed {
            display_floor_records: vec![record_of(&ids[0]), record_of(&ids[1])],
            retained_records: vec![record_of(&ids[2]), record_of(&ids[3])],
            checkpoint: true,
        };
        let mut store = SessionFile::seed_from_window(&path, seed).unwrap();

        let window = store.window_info().unwrap();
        assert_eq!(window.window_entries, 4);
        assert_eq!(
            window.boundary_entry_id.as_deref(),
            Some(ids[2].as_str()),
            "the inclusive boundary is the first retained record"
        );
        assert_eq!(
            window.oldest_entry_id.as_deref(),
            Some(ids[0].as_str()),
            "the floor's oldest record leads the store"
        );
        assert_eq!(store.leaf_id().map(str::to_string), Some(ids[3].clone()));
        assert_eq!(
            window.window_bytes as usize,
            serde_json::to_vec(&full.entries()[0]).unwrap().len()
                + serde_json::to_vec(&full.entries()[1]).unwrap().len()
                + serde_json::to_vec(&full.entries()[2]).unwrap().len()
                + serde_json::to_vec(&full.entries()[3]).unwrap().len()
                + 4,
            "the reported span is the records' on-disk size"
        );
        assert!(store.persist_appended().is_ok());
        let on_disk = SessionFile::open(&path).unwrap();
        assert_eq!(on_disk.message_count(), 6, "the durable file is intact");
    }

    /// The golden-session replay gate (fast-session-open PR 2): on a real
    /// captured session, the windowed open's context is byte-equal to the
    /// full parse's, and the open is fast. The corpus is the 42MB driver
    /// session; run in the sandbox with the read-only copy uploaded.
    #[test]
    #[ignore = "golden corpus replay: PA_FAST_OPEN_CORPUS must name the file"]
    fn golden_window_open_context_is_byte_equal_to_the_full_parse() {
        let Some(path) = std::env::var_os("PA_FAST_OPEN_CORPUS").map(PathBuf::from) else {
            panic!("PA_FAST_OPEN_CORPUS must name the golden session file");
        };
        let full = SessionFile::open(&path).unwrap();
        let full_messages = serde_json::to_vec(&full.messages()).unwrap();

        let open_started = std::time::Instant::now();
        let windowed = SessionFile::open_windowed(&path).unwrap();
        let open_ms = open_started.elapsed().as_millis() as u64;
        assert!(
            windowed.window_info().is_some(),
            "the window must apply to the golden corpus"
        );
        assert_eq!(
            serde_json::to_vec(&windowed.messages()).unwrap(),
            full_messages,
            "window-open context diverged from the full parse"
        );
        assert!(
            open_ms < 200,
            "windowed open took {open_ms}ms (gate: <200ms)"
        );
    }

    /// The compaction-checkpoint window on the golden corpus: a leaf on the
    /// last-compaction branch seeds a CompactionCheckpoint window whose
    /// context is byte-equal to the full parse at the same leaf. Leaf
    /// `a3186289` (line 37265 of the captured corpus) is the tip of the
    /// branch carrying the last compaction; the append-only file keeps both
    /// facts frozen.
    #[test]
    #[ignore = "golden corpus replay: PA_FAST_OPEN_CORPUS must name the file"]
    fn golden_compaction_checkpoint_window_is_byte_equal_at_its_leaf() {
        let Some(path) = std::env::var_os("PA_FAST_OPEN_CORPUS").map(PathBuf::from) else {
            panic!("PA_FAST_OPEN_CORPUS must name the golden session file");
        };
        let leaf_id = "a3186289";
        let windowed = SessionFile::open_windowed_at(&path, Some(leaf_id)).unwrap();
        let window = windowed.window_info().expect("the window applies");
        assert_eq!(window.window_entries, 145, "the retained window size");
        assert_eq!(
            window.boundary_entry_id.as_deref(),
            Some("4f4cc0e3"),
            "the window cut is the compaction's firstKeptEntryId"
        );

        // The full-parse reference at the same leaf: the store's leaf drives
        // the same fold the windowed seed must reproduce.
        let mut full = SessionFile::open(&path).unwrap();
        full.leaf_id = Some(leaf_id.to_string());
        assert_eq!(
            serde_json::to_vec(&windowed.messages()).unwrap(),
            serde_json::to_vec(&full.messages()).unwrap(),
            "compaction-checkpoint window diverged from the full parse"
        );
    }
}
