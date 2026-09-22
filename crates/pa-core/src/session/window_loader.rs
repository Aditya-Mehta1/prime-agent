//! Session window loader: the model context from a bounded reverse scan.
//!
//! The session file is append-only JSONL over a parent-id tree. The model
//! context at a leaf ([`super::build_session_context`]) reads only the
//! leaf's parent chain, and when a compaction sits on that chain the
//! context needs nothing older than the compaction's `firstKeptEntryId`.
//! This loader reconstructs the exact context the full pipeline
//! (`read_to_string` + [`super::parse_session_entries`] +
//! [`super::build_session_context`]) produces, by scanning the file back to
//! front with [`ReverseJsonlScanner`] and parsing only the records the
//! context consumes.
//!
//! Two window modes:
//!
//! - compaction checkpoint ([`WindowSource::CompactionCheckpoint`], the
//!   last compaction on the leaf path): the context is the compaction
//!   summary, the retained records from the `firstKeptEntryId` boundary
//!   onward, and the settings baseline. The settings hunt runs until found
//!   or file start, not until the display floor closes: a setting changed
//!   deeper than the floor must still win, or the context would not be
//!   byte-identical.
//! - chain closure ([`WindowSource::ChainClosure`], no compaction on the
//!   leaf path): the context is the whole parent chain, so the window is
//!   the chain itself and the scan stops at the path terminus instead of
//!   reading the rest of the file. No display floor: no displayed content
//!   is dropped.
//!
//! Invariants:
//!
//! - byte-identical context: [`load_session_window`]'s context equals the
//!   full pipeline's for the same leaf (PartialEq and serde-serialized
//!   bytes; asserted by the golden tests, including the 42MB corpus
//!   replay);
//! - no behavior change: nothing calls this on the open path yet (the
//!   daemon open-path switch is a later PR);
//! - no new files: the loader reads the session file and writes nothing.
//!
//! The scan falls back to the full pipeline ([`WindowSource::FullRead`];
//! still byte-identical, just not windowed) when a precondition fails: the
//! file is not at the current session version (v1/v2 entries carry no ids,
//! so the walk cannot run), a record head is not in the canonical
//! `type,id,parentId` shape (header rows are the exception: their id sits
//! behind the variable header keys and is sniffed with a guarded search,
//! then parse-verified), a gate-passing record fails to parse, or a parent
//! link resolves to a record the backward scan has already passed (a
//! duplicate id above the leaf: the walk cannot reproduce
//! `build_session_context`'s by-id resolution then).

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;

use pa_types::session::{AgentMessage, CompactionSummaryMessage, FileEntry};
use serde_json::Value;

use super::reverse_scanner::ReverseJsonlScanner;
use super::{
    append_context_message, build_session_context, empty_context, parse_session_entries,
    timestamp_to_millis, SessionContext, CURRENT_SESSION_VERSION,
};

/// The display floor: pre-boundary records kept in the window so a
/// just-compacted session does not open empty. Collecting stops at
/// whichever bound hits first.
const DISPLAY_FLOOR_MIN_ENTRIES: usize = 200;
const DISPLAY_FLOOR_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Entry type of the child-usage attribution rows the context folds into
/// retained assistant messages.
const CHILD_USAGE_TYPE: &str = "child_usage_attributed";

/// How a [`SessionWindow`] was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSource {
    /// Windowed scan: the last compaction on the leaf path checkpointed the
    /// context; the window is the `firstKeptEntryId` boundary through the
    /// leaf.
    CompactionCheckpoint,
    /// Windowed scan with no compaction on the leaf path: the window is the
    /// whole parent chain (chain closure), stopping at the path terminus.
    /// An absent leaf id produces the same shape with an empty window (the
    /// full pipeline's context is empty there too).
    ChainClosure,
    /// The full-read pipeline ran (a window precondition failed); the
    /// context is still byte-identical, just not windowed.
    FullRead,
}

/// The model context at a leaf plus the records the window retains.
pub struct SessionWindow {
    /// Byte-equal to the full pipeline's context for the same leaf.
    pub context: SessionContext,
    /// Pre-boundary leaf-path records, oldest first, bounded by the display
    /// floor. Raw session-file bytes (newline excluded). Empty outside the
    /// compaction-checkpoint mode: the other modes drop no displayed
    /// content.
    pub display_floor_records: Vec<Vec<u8>>,
    /// The leaf-path records the context consumes, oldest first: from the
    /// `firstKeptEntryId` boundary (inclusive) through the leaf in the
    /// compaction-checkpoint mode, the whole chain in chain-closure mode.
    /// Raw session-file bytes (newline excluded), unfolded: the child-usage
    /// fold is a context concern (the pipeline folds into the context it
    /// builds), not a store-record one.
    pub retained_records: Vec<Vec<u8>>,
    pub source: WindowSource,
}

/// Load the session window for `leaf_id` (`None` or `""`: the last
/// parseable record, matching [`super::build_session_context`]'s default).
pub fn load_session_window(path: &Path, leaf_id: Option<&str>) -> SessionWindow {
    match scan_window(path, leaf_id) {
        Ok(window) => window,
        Err(reason) => {
            match &reason {
                ScanFallback::Io(error) => tracing::debug!(
                    error = %error,
                    path = %path.display(),
                    "session window scan fell back to the full read"
                ),
                reason => tracing::debug!(
                    ?reason,
                    path = %path.display(),
                    "session window scan fell back to the full read"
                ),
            }
            full_read_window(path, leaf_id)
        }
    }
}

/// The full-read pipeline: the reference the window must match, run
/// verbatim when a window precondition fails.
fn full_read_window(path: &Path, leaf_id: Option<&str>) -> SessionWindow {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    SessionWindow {
        context: build_session_context(&parse_session_entries(&content), leaf_id),
        display_floor_records: Vec::new(),
        retained_records: Vec::new(),
        source: WindowSource::FullRead,
    }
}

/// Why the windowed scan gave up. The fallback is byte-identical, so this
/// is a performance boundary, not a correctness one.
#[derive(Debug)]
enum ScanFallback {
    Io(std::io::Error),
    HeaderNotCurrentVersion,
    NonCanonicalRecord,
    RecordParseFailed,
    /// A by-id target resolves to a record the backward scan has already
    /// passed (a duplicate id above the walk's reach): the walk cannot
    /// reproduce `build_session_context`'s by-id resolution.
    DuplicateIdAboveScan,
}

fn scan_window(path: &Path, leaf_id: Option<&str>) -> Result<SessionWindow, ScanFallback> {
    let mut file = File::open(path).map_err(ScanFallback::Io)?;
    let file_len = file.seek(SeekFrom::End(0)).map_err(ScanFallback::Io)?;
    if !header_is_current_version(path)? {
        return Err(ScanFallback::HeaderNotCurrentVersion);
    }
    let mut scanner = ReverseJsonlScanner::new_at(file, file_len).map_err(ScanFallback::Io)?;
    let mut walk = WindowWalk::new(leaf_id);
    while let Some(record) = scanner.scan_next_record().map_err(ScanFallback::Io)? {
        let Some(head) = parse_canonical_head(&record) else {
            return Err(ScanFallback::NonCanonicalRecord);
        };
        walk.step(&record, head)?;
        if walk.done() {
            break;
        }
    }
    walk.finish()
}

/// The first line must be a `session` header at the current version: older
/// versions carry no per-entry ids, so the window walk cannot run on them.
fn header_is_current_version(path: &Path) -> Result<bool, ScanFallback> {
    let mut head = Vec::with_capacity(64 * 1024);
    File::open(path)
        .map_err(ScanFallback::Io)?
        .take(64 * 1024)
        .read_to_end(&mut head)
        .map_err(ScanFallback::Io)?;
    let Some(line_end) = head.iter().position(|&byte| byte == b'\n') else {
        return Ok(false);
    };
    let Ok(value) = serde_json::from_slice::<Value>(&head[..line_end]) else {
        return Ok(false);
    };
    Ok(value.get("type").and_then(Value::as_str) == Some("session")
        && value.get("version").and_then(Value::as_u64) == Some(u64::from(CURRENT_SESSION_VERSION)))
}

/// The canonical record head the walk's prefix reads rely on:
/// `{"type":"<t>","id":"<id>","parentId":<id|null>,...` — the header row
/// (`t == "session"`) carries no `parentId` key. Anything else fails the
/// gate, so the prefix reads are exact on every record the walk trusts.
struct RecordHead<'a> {
    entry_type: &'a str,
    id: &'a str,
    /// The `parentId` string value; `None` when null or absent.
    parent_id: Option<&'a str>,
}

fn parse_canonical_head(record: &[u8]) -> Option<RecordHead<'_>> {
    // Type, id, and parentId heads are tiny; bounding the scan window
    // refuses pathological heads instead of scanning them.
    let head = &record[..record.len().min(4096)];
    let mut rest = strip_ascii_ws_start(head)?.strip_prefix(b"{\"type\":\"")?;
    let type_end = rest.iter().position(|&byte| byte == b'"')?;
    let entry_type = std::str::from_utf8(&rest[..type_end]).ok()?;
    if entry_type.contains('\\') {
        return None;
    }
    rest = &rest[type_end + 1..];
    // Header rows carry variable header keys (version, timestamp, cwd)
    // before the id; every other row has the id immediately after the
    // type. The guarded search is exact for headers because JSON-escaped
    // content cannot produce the plain `"id":"` byte sequence.
    let (id, rest) = if entry_type == "session" {
        let id_key = find_guarded_key(rest, b"\"id\":\"")?;
        let after = &rest[id_key + b"\"id\":\"".len()..];
        let id_end = after.iter().position(|&byte| byte == b'"')?;
        let id = std::str::from_utf8(&after[..id_end]).ok()?;
        if id.contains('\\') {
            return None;
        }
        (id, &after[id_end + 1..])
    } else {
        let after = rest.strip_prefix(b",\"id\":\"")?;
        let id_end = after.iter().position(|&byte| byte == b'"')?;
        let id = std::str::from_utf8(&after[..id_end]).ok()?;
        if id.contains('\\') {
            return None;
        }
        (id, &after[id_end + 1..])
    };
    if entry_type == "session" {
        // The header's parent is None by type definition; the walk
        // parses and verifies header rows it matches on.
        return Some(RecordHead {
            entry_type,
            id,
            parent_id: None,
        });
    }
    if let Some(after_key) = rest.strip_prefix(b",\"parentId\":") {
        let parent_id = if let Some(value) = after_key.strip_prefix(b"\"") {
            let value_end = value.iter().position(|&byte| byte == b'"')?;
            let parent = std::str::from_utf8(&value[..value_end]).ok()?;
            if parent.contains('\\') {
                return None;
            }
            Some(parent)
        } else {
            after_key.strip_prefix(b"null")?;
            None
        };
        return Some(RecordHead {
            entry_type,
            id,
            parent_id,
        });
    }
    // A non-header row without the parentId key is outside the canonical shape.
    None
}

/// The first occurrence of `needle` whose leading quote sits at a key
/// boundary (preceded by `,` or `{`).
fn find_guarded_key(head: &[u8], needle: &[u8]) -> Option<usize> {
    let mut from = 0;
    while let Some(at) = head[from..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let at = from + at;
        if at > 0 && (head[at - 1] == b',' || head[at - 1] == b'{') {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

fn strip_ascii_ws_start(record: &[u8]) -> Option<&[u8]> {
    let start = record
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(record.len());
    record.get(start..)
}

/// Whether the pipeline's transcript push rule keeps this entry's message.
fn pushes_into_context(entry: &FileEntry) -> bool {
    match entry {
        FileEntry::Message { .. } | FileEntry::CustomMessage { .. } => true,
        FileEntry::BranchSummary { payload, .. } => !payload.summary.is_empty(),
        _ => false,
    }
}

/// Where an on-path record lands relative to the compaction checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeRange {
    /// The compaction record itself: payload capture, never pushed.
    Compaction,
    /// Between the boundary (inclusive) and the compaction (exclusive).
    Boundary,
    /// Between the leaf and the compaction (exclusive), or the whole chain
    /// when no compaction was found.
    Suffix,
    /// Below the boundary: display-floor content, not context.
    Floor,
}

/// The compaction payload fields the summary message needs.
struct CompactionCapture {
    summary: String,
    tokens_before: u64,
    custom_instructions: Option<String>,
    harness_digest: Option<String>,
    timestamp: String,
}

#[derive(Default)]
struct SettingsHunt {
    /// The leaf-side-most `thinking_level_change` on the path; `None` until
    /// found (the pipeline default is `"off"`).
    thinking_level: Option<String>,
    /// `Some(tier)` once a change row was parsed; the inner value stays
    /// `None` for an explicit null, which the pipeline applies too.
    service_tier: Option<Option<pa_types::ai::ServiceTier>>,
    model: Option<(String, String)>,
}

impl SettingsHunt {
    fn all_found(&self) -> bool {
        self.thinking_level.is_some() && self.service_tier.is_some() && self.model.is_some()
    }

    /// Whether this record type still owes the hunt a parse.
    fn needs_parse(&self, entry_type: &str) -> bool {
        match entry_type {
            "thinking_level_change" => self.thinking_level.is_none(),
            "service_tier_change" => self.service_tier.is_none(),
            "model_change" => self.model.is_none(),
            "message" => self.model.is_none(),
            _ => false,
        }
    }

    fn apply(&mut self, entry: &FileEntry) {
        match entry {
            FileEntry::ThinkingLevelChange { payload, .. } => {
                if self.thinking_level.is_none() {
                    self.thinking_level = Some(payload.thinking_level.clone());
                }
            }
            FileEntry::ServiceTierChange { payload, .. } => {
                if self.service_tier.is_none() {
                    self.service_tier = Some(payload.service_tier);
                }
            }
            FileEntry::ModelChange { payload, .. } => {
                if self.model.is_none() {
                    self.model = Some((payload.provider.clone(), payload.model_id.clone()));
                }
            }
            FileEntry::Message {
                message: AgentMessage::Assistant(assistant),
                ..
            } => {
                if self.model.is_none() {
                    self.model = Some((assistant.provider.clone(), assistant.model.clone()));
                }
            }
            _ => {}
        }
    }
}

/// The backward walk over the leaf path. Records arrive last-first; the
/// walk locks onto the leaf, then follows parent links, parsing only the
/// records whose payload the context consumes.
struct WindowWalk {
    leaf_target: Option<String>,
    leaf_locked: bool,
    expected_parent: Option<String>,
    walk_complete: bool,
    /// Scan-order counter; file order runs opposite to scan order.
    scan_step: usize,
    /// Sniffed ids of the records above the leaf (scanned before the leaf
    /// locked): where a missing parent link's by-id resolution may live.
    ids_above_leaf: HashSet<String>,
    /// Message-typed records by sniffed id, at the first (lowest) scan step
    /// they were seen. A sighting at a step below a folded assistant's own
    /// scan step is file-after that assistant, where the pipeline's by-id
    /// fold target would resolve.
    message_id_steps: HashMap<String, usize>,
    /// Scan step of each retained assistant message row, by entry id.
    assistant_steps: HashMap<String, usize>,
    compaction: Option<CompactionCapture>,
    boundary_id: Option<String>,
    boundary_found: bool,
    suffix_records: Vec<Vec<u8>>,
    suffix_entries: Vec<FileEntry>,
    boundary_records: Vec<Vec<u8>>,
    boundary_entries: Vec<FileEntry>,
    display_floor_records: Vec<Vec<u8>>,
    floor_entries: usize,
    floor_bytes: usize,
    settings: SettingsHunt,
    attribution_records: Vec<Vec<u8>>,
}

impl WindowWalk {
    fn new(leaf_id: Option<&str>) -> Self {
        // `build_session_context` reads `Some("")` as the default leaf.
        let leaf_target = leaf_id.filter(|id| !id.is_empty()).map(str::to_string);
        Self {
            leaf_target,
            leaf_locked: false,
            expected_parent: None,
            walk_complete: false,
            scan_step: 0,
            ids_above_leaf: HashSet::new(),
            message_id_steps: HashMap::new(),
            assistant_steps: HashMap::new(),
            compaction: None,
            boundary_id: None,
            boundary_found: false,
            suffix_records: Vec::new(),
            suffix_entries: Vec::new(),
            boundary_records: Vec::new(),
            boundary_entries: Vec::new(),
            display_floor_records: Vec::new(),
            floor_entries: 0,
            floor_bytes: 0,
            settings: SettingsHunt::default(),
            attribution_records: Vec::new(),
        }
    }

    fn done(&self) -> bool {
        if !self.leaf_locked {
            return false;
        }
        self.walk_complete
            || (self.compaction.is_some()
                && self.boundary_found
                && self.settings.all_found()
                && !self.floor_open())
    }

    fn floor_open(&self) -> bool {
        self.floor_entries < DISPLAY_FLOOR_MIN_ENTRIES && self.floor_bytes < DISPLAY_FLOOR_MAX_BYTES
    }

    fn step(&mut self, record: &[u8], head: RecordHead) -> Result<(), ScanFallback> {
        self.scan_step += 1;
        if head.entry_type == "message" {
            // First sighting keeps the lowest scan step: the file-latest
            // position among the message rows seen so far.
            self.message_id_steps
                .entry(head.id.to_string())
                .or_insert(self.scan_step);
        }
        if !self.leaf_locked {
            self.ids_above_leaf.insert(head.id.to_string());
            if head.entry_type == CHILD_USAGE_TYPE {
                self.attribution_records.push(record.to_vec());
            }
            let matched = match &self.leaf_target {
                Some(target) => head.id == target.as_str(),
                None => true,
            };
            if !matched {
                return Ok(());
            }
            // Unparsable records are not entries (the pipeline skips them);
            // try older records for the leaf.
            let Ok(entry) = serde_json::from_slice::<FileEntry>(&record) else {
                return Ok(());
            };
            if let Some(target) = &self.leaf_target {
                if entry.id() != Some(target.as_str()) {
                    // The sniff matched embedded content, not the id key.
                    return Ok(());
                }
            }
            self.leaf_locked = true;
            let range = self.node_range(&head);
            self.on_path_node(record, head, Some(entry), range);
            return Ok(());
        }
        if self.walk_complete {
            return Ok(());
        }
        let Some(expected) = self.expected_parent.clone() else {
            return Ok(());
        };
        if head.id != expected {
            if head.entry_type == CHILD_USAGE_TYPE {
                self.attribution_records.push(record.to_vec());
            }
            return Ok(());
        }
        let range = self.node_range(&head);
        let parse_owed = match range {
            NodeRange::Compaction => true,
            _ if head.entry_type == "session" => true,
            NodeRange::Boundary | NodeRange::Suffix => {
                matches!(
                    head.entry_type,
                    "message" | "custom_message" | "branch_summary"
                ) || self.settings.needs_parse(head.entry_type)
            }
            NodeRange::Floor => self.floor_open() || self.settings.needs_parse(head.entry_type),
        };
        let entry = match parse_owed {
            true => match serde_json::from_slice::<FileEntry>(&record) {
                Ok(entry) => Some(entry),
                Err(_) => return Err(ScanFallback::RecordParseFailed),
            },
            false => None,
        };
        // The sniff match is verified against the parsed id; a mismatch
        // means embedded content shadowed the id key, so this record is
        // not the parent and the walk keeps looking.
        if let Some(parsed) = &entry {
            if parsed.id().is_some() && parsed.id() != Some(head.id) {
                return Ok(());
            }
        }
        self.on_path_node(record, head, entry, range);
        Ok(())
    }

    /// The range for an on-path record; flips the boundary flag when the
    /// boundary record arrives.
    fn node_range(&mut self, head: &RecordHead) -> NodeRange {
        if head.entry_type == "compaction" && self.compaction.is_none() {
            return NodeRange::Compaction;
        }
        if self.compaction.is_some()
            && !self.boundary_found
            && self.boundary_id.as_deref() == Some(head.id)
        {
            self.boundary_found = true;
            return NodeRange::Boundary;
        }
        if self.compaction.is_none() {
            NodeRange::Suffix
        } else if !self.boundary_found {
            NodeRange::Boundary
        } else {
            NodeRange::Floor
        }
    }

    fn on_path_node(
        &mut self,
        record: &[u8],
        head: RecordHead,
        entry: Option<FileEntry>,
        range: NodeRange,
    ) {
        if head.entry_type == CHILD_USAGE_TYPE {
            self.attribution_records.push(record.to_vec());
        }
        // The header row's parent is None by type definition; every other
        // on-path record carries the walk's next link in its payload (when
        // parsed) or its canonical head.
        self.expected_parent = if head.entry_type == "session" {
            None
        } else {
            match &entry {
                Some(parsed) => parsed.parent_id().map(str::to_string),
                None => head.parent_id.map(str::to_string),
            }
        };
        self.walk_complete = self.expected_parent.is_none();
        if let Some(parsed) = &entry {
            self.settings.apply(parsed);
        }
        match range {
            NodeRange::Compaction => {
                let Some(FileEntry::Compaction { payload, base, .. }) = &entry else {
                    return;
                };
                self.compaction = Some(CompactionCapture {
                    summary: payload.summary.clone(),
                    tokens_before: payload.tokens_before,
                    custom_instructions: payload.custom_instructions.clone(),
                    harness_digest: payload.harness_digest.clone(),
                    timestamp: base.timestamp.clone().unwrap_or_default(),
                });
                self.boundary_id = Some(payload.first_kept_entry_id.clone());
            }
            NodeRange::Suffix => {
                self.suffix_records.push(record.to_vec());
                if let Some(parsed) = entry {
                    if pushes_into_context(&parsed) {
                        if let Some(id) = parsed.id().map(str::to_string) {
                            self.assistant_steps.insert(id, self.scan_step);
                        }
                        self.suffix_entries.push(parsed);
                    }
                }
            }
            NodeRange::Boundary => {
                self.boundary_records.push(record.to_vec());
                if let Some(parsed) = entry {
                    if pushes_into_context(&parsed) {
                        if let Some(id) = parsed.id().map(str::to_string) {
                            self.assistant_steps.insert(id, self.scan_step);
                        }
                        self.boundary_entries.push(parsed);
                    }
                }
            }
            NodeRange::Floor => {
                if self.floor_open() {
                    self.floor_entries += 1;
                    self.floor_bytes += record.len();
                    self.display_floor_records.push(record.to_vec());
                }
            }
        }
    }

    fn finish(mut self) -> Result<SessionWindow, ScanFallback> {
        if !self.leaf_locked {
            // The pipeline's by-id lookup misses too: empty context.
            return Ok(SessionWindow {
                context: empty_context(),
                display_floor_records: Vec::new(),
                retained_records: Vec::new(),
                source: WindowSource::ChainClosure,
            });
        }
        if !self.walk_complete {
            let expected = self.expected_parent.clone().unwrap_or_default();
            if self.ids_above_leaf.contains(&expected) {
                return Err(ScanFallback::DuplicateIdAboveScan);
            }
        }
        self.apply_folds()?;
        let thinking_level = self
            .settings
            .thinking_level
            .take()
            .unwrap_or_else(|| "off".to_string());
        let service_tier = self.settings.service_tier.take().flatten();
        let model = self.settings.model.take();
        let compaction = self.compaction.take();
        let (messages, retained_records, source) = match compaction {
            Some(compaction) => {
                let mut messages = Vec::new();
                let mut retained = Vec::new();
                let mut retained_count = 0u64;
                for entry in self.boundary_entries.iter().rev() {
                    if append_context_message(entry, &mut retained) {
                        retained_count += 1;
                    }
                }
                messages.push(AgentMessage::CompactionSummary(CompactionSummaryMessage {
                    summary: compaction.summary.clone(),
                    tokens_before: compaction.tokens_before,
                    retained_message_count: Some(retained_count),
                    custom_instructions: compaction.custom_instructions.clone(),
                    harness_digest: compaction.harness_digest.clone(),
                    timestamp: timestamp_to_millis(&compaction.timestamp),
                }));
                messages.extend(retained);
                for entry in self.suffix_entries.iter().rev() {
                    append_context_message(entry, &mut messages);
                }
                let mut records = std::mem::take(&mut self.boundary_records);
                records.reverse();
                let mut suffix = std::mem::take(&mut self.suffix_records);
                suffix.reverse();
                records.extend(suffix);
                (messages, records, WindowSource::CompactionCheckpoint)
            }
            None => {
                let mut messages = Vec::new();
                for entry in self.suffix_entries.iter().rev() {
                    append_context_message(entry, &mut messages);
                }
                let mut records = std::mem::take(&mut self.suffix_records);
                records.reverse();
                (messages, records, WindowSource::ChainClosure)
            }
        };
        self.display_floor_records.reverse();
        Ok(SessionWindow {
            context: SessionContext {
                messages,
                thinking_level,
                service_tier,
                model,
            },
            display_floor_records: self.display_floor_records,
            retained_records,
            source,
        })
    }

    /// Fold child-usage attributions into the retained assistant messages,
    /// reproducing the pipeline's fold: the last attribution row in file
    /// order wins per target, and the target is the file-latest assistant
    /// with that id. A message row file-after the folded assistant with
    /// the same id could own the by-id target, so the window aborts
    /// instead of guessing.
    fn apply_folds(&mut self) -> Result<(), ScanFallback> {
        if self.attribution_records.is_empty() {
            return Ok(());
        }
        let mut folds: HashMap<String, pa_types::ai::Usage> = HashMap::new();
        for record in &self.attribution_records {
            let Ok(FileEntry::ChildUsageAttributed { payload, .. }) =
                serde_json::from_slice::<FileEntry>(record)
            else {
                continue;
            };
            folds
                .entry(payload.target_id.clone())
                .or_insert_with(|| payload.aggregate_usage.clone());
        }
        if folds.is_empty() {
            return Ok(());
        }
        let mut folded = HashSet::new();
        // Backward-order first-wins per target == file-order last-write-wins.
        let entries = self
            .boundary_entries
            .iter_mut()
            .rev()
            .chain(self.suffix_entries.iter_mut().rev());
        for entry in entries {
            let Some(id) = entry.id().map(str::to_string) else {
                continue;
            };
            let Some(usage) = folds.get(&id) else {
                continue;
            };
            if !folded.insert(id.clone()) {
                continue;
            }
            let Some(assistant_step) = self.assistant_steps.get(&id) else {
                continue;
            };
            // The assistant's own row sighted its id at its own scan step;
            // a strictly lower step is a message row file-after it.
            if self
                .message_id_steps
                .get(&id)
                .is_some_and(|step| step < assistant_step)
            {
                return Err(ScanFallback::DuplicateIdAboveScan);
            }
            if let FileEntry::Message {
                message: AgentMessage::Assistant(assistant),
                ..
            } = entry
            {
                assistant.usage = usage.clone();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{build_session_context, parse_session_entries};
    use super::{load_session_window, WindowSource};
    use std::io::Write as _;
    use std::path::Path;

    const HEADER: &str = r#"{"type":"session","version":3,"id":"sess1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/w"}"#;
    const TS: &str = "2026-01-01T00:00:01.000Z";

    fn usage_json(input_tokens: u64) -> serde_json::Value {
        serde_json::json!({
            "input": input_tokens, "output": 1, "cacheRead": 0, "cacheWrite": 0,
            "totalTokens": input_tokens,
            "cost": {
                "input": input_tokens, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                "total": input_tokens
            }
        })
    }

    fn user_message(id: &str, parent: &str, text: &str) -> String {
        serde_json::json!({
            "type": "message", "id": id, "parentId": parent, "timestamp": TS,
            "message": { "role": "user", "content": text, "timestamp": 0 }
        })
        .to_string()
    }

    fn assistant_message(id: &str, parent: &str, model: &str, input_tokens: u64) -> String {
        serde_json::json!({
            "type": "message", "id": id, "parentId": parent, "timestamp": TS,
            "message": {
                "role": "assistant", "content": [], "api": "anthropic-messages",
                "provider": "anthropic", "model": model, "usage": usage_json(input_tokens),
                "stopReason": "stop", "timestamp": 0
            }
        })
        .to_string()
    }

    fn tool_result(id: &str, parent: &str) -> String {
        serde_json::json!({
            "type": "message", "id": id, "parentId": parent, "timestamp": TS,
            "message": { "role": "toolResult", "content": [{ "type": "text", "text": "tool out" }], "timestamp": 0 }
        })
        .to_string()
    }

    fn thinking_change(id: &str, parent: &str, level: &str) -> String {
        serde_json::json!({
            "type": "thinking_level_change", "id": id, "parentId": parent,
            "timestamp": TS, "thinkingLevel": level
        })
        .to_string()
    }

    fn tier_change(id: &str, parent: &str, tier: serde_json::Value) -> String {
        serde_json::json!({
            "type": "service_tier_change", "id": id, "parentId": parent,
            "timestamp": TS, "serviceTier": tier
        })
        .to_string()
    }

    fn model_change(id: &str, parent: &str, model: &str) -> String {
        serde_json::json!({
            "type": "model_change", "id": id, "parentId": parent,
            "timestamp": TS, "provider": "openai", "modelId": model
        })
        .to_string()
    }

    fn compaction(id: &str, parent: &str, first_kept: &str, summary: &str) -> String {
        serde_json::json!({
            "type": "compaction", "id": id, "parentId": parent, "timestamp": TS,
            "summary": summary, "firstKeptEntryId": first_kept, "tokensBefore": 1234
        })
        .to_string()
    }

    fn custom_message(id: &str, parent: &str, text: &str) -> String {
        serde_json::json!({
            "type": "custom_message", "id": id, "parentId": parent, "timestamp": TS,
            "customType": "note", "content": text, "display": true
        })
        .to_string()
    }

    fn attribution(id: &str, parent: &str, target: &str, aggregate_input: u64) -> String {
        serde_json::json!({
            "type": "child_usage_attributed", "id": id, "parentId": parent, "timestamp": TS,
            "targetId": target, "childUsage": usage_json(1),
            "aggregateUsage": usage_json(aggregate_input), "origin": "spawn_task"
        })
        .to_string()
    }

    fn unknown_row(id: &str, parent: &str) -> String {
        serde_json::json!({
            "type": "request_started", "id": id, "parentId": parent,
            "timestamp": TS, "requestId": "r1"
        })
        .to_string()
    }

    fn write_session(entries: &[String]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("temp session file");
        writeln!(file, "{HEADER}").expect("write header");
        for entry in entries {
            writeln!(file, "{entry}").expect("write entry");
        }
        file.flush().expect("flush");
        file
    }

    fn assert_window_matches(path: &Path, leaf: Option<&str>) -> super::SessionWindow {
        let window = load_session_window(path, leaf);
        let content = std::fs::read_to_string(path).expect("read back");
        let expected = build_session_context(&parse_session_entries(&content), leaf);
        assert_eq!(
            window.context, expected,
            "window context differs from the full pipeline"
        );
        assert_eq!(
            serde_json::to_string(&window.context.messages).expect("serialize window"),
            serde_json::to_string(&expected.messages).expect("serialize pipeline"),
            "serialized window messages differ from the full pipeline"
        );
        window
    }

    fn chain_fixture() -> Vec<String> {
        vec![
            user_message("u1", "sess1", "hello"),
            assistant_message("a1", "u1", "claude-x", 10),
            tool_result("t1", "a1"),
            assistant_message("a2", "t1", "claude-x", 20),
            thinking_change("th1", "a2", "high"),
            tier_change("ti1", "th1", serde_json::json!("priority")),
            unknown_row("x1", "ti1"),
            custom_message("cm1", "x1", "a note"),
            assistant_message("a3", "cm1", "claude-x", 30),
        ]
    }

    #[test]
    fn compaction_window_matches_full_parse() {
        // Side branch off a1 (off-path rows), a compaction whose boundary is
        // a1, retained tail, settings below the boundary, an attribution row
        // for a retained assistant, and an off-path attribution row.
        let mut entries = chain_fixture();
        entries.push(assistant_message("a4", "a3", "claude-x", 40));
        entries.push(user_message("u2", "a4", "branch off"));
        entries.push(assistant_message("a5", "u2", "claude-y", 50));
        entries.push(user_message("u3", "a5", "back on main"));
        entries.push(assistant_message("a6", "u3", "claude-x", 60));
        entries.push(attribution("at1", "a6", "a6", 70));
        entries.push(user_message("u4", "a6", "keep going"));
        entries.push(attribution("at2", "u4", "a5", 80));
        entries.push(compaction("c1", "u4", "a1", "the story so far"));
        entries.push(assistant_message("a7", "c1", "claude-x", 90));
        entries.push(user_message("u5", "a7", "after compaction"));
        entries.push(model_change("m1", "u5", "gpt-x"));
        entries.push(assistant_message("a8", "m1", "claude-z", 100));
        let file = write_session(&entries);

        let window = assert_window_matches(file.path(), None);
        assert_eq!(window.source, WindowSource::CompactionCheckpoint);
        // Retained records run from the boundary (a1) through the leaf.
        assert_eq!(window.retained_records.len(), entries.len() - 6);
        // The display floor keeps the pre-boundary rows.
        assert!(!window.display_floor_records.is_empty());
        let leaf_message = window.context.messages.last().expect("messages");
        assert!(matches!(
            leaf_message,
            super::super::super::session::AgentMessage::Assistant(_)
        ));
    }

    #[test]
    fn chain_closure_matches_full_parse_without_path_compaction() {
        // The only compaction hangs off a side branch: not on the leaf path.
        let mut entries = chain_fixture();
        entries.push(user_message("u2", "a3", "side branch"));
        entries.push(assistant_message("a4", "u2", "claude-y", 40));
        entries.push(compaction("c1", "a4", "u2", "side checkpoint"));
        entries.push(user_message("u3", "a3", "main continues"));
        let file = write_session(&entries);

        let window = assert_window_matches(file.path(), None);
        assert_eq!(window.source, WindowSource::ChainClosure);
        assert!(window.display_floor_records.is_empty());
    }

    #[test]
    fn chain_closure_stops_at_broken_parent_link() {
        // u2's parent does not exist anywhere: the walk and the pipeline
        // both end the path there.
        let mut entries = chain_fixture();
        entries.push(user_message("u2", "missing-parent", "orphan"));
        let file = write_session(&entries);

        let window = assert_window_matches(file.path(), None);
        assert_eq!(window.source, WindowSource::ChainClosure);
    }

    #[test]
    fn settings_hunt_reaches_file_start_past_floor_cap() {
        // A thinking change deep below the display floor (250 filler rows)
        // must still win the baseline.
        let mut entries = Vec::new();
        entries.push(user_message("u1", "sess1", "hello"));
        entries.push(assistant_message("a1", "u1", "claude-x", 10));
        entries.push(compaction("c1", "a1", "a1", "checkpoint"));
        for index in 0..250 {
            entries.push(user_message(&format!("f{index}"), "a1", "filler"));
        }
        entries.push(thinking_change("th-deep", "a1", "high"));
        entries.push(assistant_message("a2", "c1", "claude-x", 20));
        let file = write_session(&entries);

        let window = assert_window_matches(file.path(), None);
        assert_eq!(window.source, WindowSource::CompactionCheckpoint);
        assert_eq!(window.context.thinking_level, "high");
        assert_eq!(window.display_floor_records.len(), 200);
    }

    #[test]
    fn boundary_off_path_keeps_retained_empty() {
        let entries = vec![
            user_message("u1", "sess1", "hello"),
            assistant_message("a1", "u1", "claude-x", 10),
            compaction("c1", "a1", "not-in-file", "checkpoint"),
            user_message("u2", "c1", "after"),
        ];
        let file = write_session(&entries);

        let window = assert_window_matches(file.path(), None);
        assert_eq!(window.source, WindowSource::CompactionCheckpoint);
        // No boundary record on the path: only the summary + post rows.
        assert_eq!(window.context.messages.len(), 2);
    }

    #[test]
    fn attribution_last_row_wins_per_target() {
        let entries = vec![
            user_message("u1", "sess1", "hello"),
            assistant_message("a1", "u1", "claude-x", 10),
            compaction("c1", "a1", "a1", "checkpoint"),
            assistant_message("a2", "c1", "claude-x", 20),
            attribution("at1", "a2", "a2", 30),
            attribution("at2", "a2", "a2", 40),
        ];
        let file = write_session(&entries);
        assert_window_matches(file.path(), None);
    }

    #[test]
    fn attribution_row_above_leaf_is_honored() {
        // The leaf is mid-file; the attribution row lands after it in file
        // order (above the leaf for the backward scan).
        let entries = vec![
            user_message("u1", "sess1", "hello"),
            compaction("c1", "u1", "u1", "checkpoint"),
            assistant_message("a1", "c1", "claude-x", 10),
            user_message("u2", "a1", "after"),
            attribution("at1", "u2", "a1", 50),
        ];
        let file = write_session(&entries);
        assert_window_matches(file.path(), Some("u2"));
    }

    #[test]
    fn duplicate_message_id_above_assistant_falls_back() {
        // A message row with the same id appears after the folded
        // assistant in file order: the by-id fold target could resolve
        // there, so the window must not guess.
        let entries = vec![
            user_message("u1", "sess1", "hello"),
            compaction("c1", "u1", "u1", "checkpoint"),
            assistant_message("a1", "c1", "claude-x", 10),
            attribution("at1", "a1", "a1", 50),
            user_message("u2", "a1", "after"),
            assistant_message("a1", "u2", "claude-x", 99),
        ];
        let file = write_session(&entries);

        let window = assert_window_matches(file.path(), None);
        assert_eq!(window.source, WindowSource::FullRead);
    }

    #[test]
    fn duplicate_parent_link_above_leaf_falls_back() {
        // x1 exists above the leaf; u2's parent link resolves to it in the
        // pipeline's by-id map but the backward walk cannot reach it.
        let entries = vec![
            user_message("u1", "sess1", "hello"),
            thinking_change("x1", "u1", "high"),
            assistant_message("a1", "x1", "claude-x", 10),
            user_message("u2", "x1", "relinks"),
            assistant_message("a2", "u2", "claude-x", 20),
        ];
        let file = write_session(&entries);

        let window = assert_window_matches(file.path(), Some("a2"));
        assert_eq!(window.source, WindowSource::FullRead);
    }

    #[test]
    fn non_canonical_head_falls_back() {
        // The message payload precedes the id: outside the canonical shape
        // the walk's prefix reads rely on.
        let entries = vec![
            user_message("u1", "sess1", "hello"),
            assistant_message("a1", "u1", "claude-x", 10),
            r#"{"type":"message","message":{"role":"user","content":"weird","timestamp":0},"id":"w1","parentId":"a1","timestamp":"2026-01-01T00:00:01.000Z"}"#.to_string(),
            user_message("u2", "w1", "after weird"),
        ];
        let file = write_session(&entries);

        let window = assert_window_matches(file.path(), None);
        assert_eq!(window.source, WindowSource::FullRead);
    }

    #[test]
    fn version_one_header_falls_back() {
        let mut file = tempfile::NamedTempFile::new().expect("temp session file");
        writeln!(
            file,
            r#"{{"type":"session","id":"sess1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/w"}}"#
        )
        .expect("write header");
        writeln!(
            file,
            r#"{{"type":"message","timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":"v1","timestamp":0}}}}"#
        )
        .expect("write entry");
        file.flush().expect("flush");

        let window = assert_window_matches(file.path(), None);
        assert_eq!(window.source, WindowSource::FullRead);
    }

    #[test]
    fn leaf_id_absent_gives_empty_context() {
        let entries = chain_fixture();
        let file = write_session(&entries);

        let window = assert_window_matches(file.path(), Some("nope"));
        assert_eq!(window.source, WindowSource::ChainClosure);
        assert!(window.context.messages.is_empty());
        assert!(window.retained_records.is_empty());
    }

    #[test]
    fn leaf_empty_string_is_the_default_leaf() {
        let entries = chain_fixture();
        let file = write_session(&entries);
        assert_window_matches(file.path(), Some(""));
        assert_window_matches(file.path(), None);
    }

    #[test]
    fn display_floor_stops_at_byte_cap() {
        let filler = "x".repeat(3 * 1024 * 1024);
        let mut entries = Vec::new();
        entries.push(user_message("u1", "sess1", "hello"));
        entries.push(assistant_message("a1", "u1", "claude-x", 10));
        entries.push(compaction("c1", "a1", "a1", "checkpoint"));
        for index in 0..4 {
            entries.push(user_message(&format!("f{index}"), "a1", &filler));
        }
        entries.push(assistant_message("a2", "c1", "claude-x", 20));
        let file = write_session(&entries);

        let window = assert_window_matches(file.path(), None);
        // 3 fillers (~9MB) pass the byte cap before the 4th.
        assert_eq!(window.display_floor_records.len(), 3);
    }

    #[test]
    fn empty_and_header_only_files_match() {
        let empty = tempfile::NamedTempFile::new().expect("empty file");
        let window = assert_window_matches(empty.path(), None);
        assert_eq!(window.source, WindowSource::FullRead);

        let mut header_only = tempfile::NamedTempFile::new().expect("header file");
        writeln!(header_only, "{HEADER}").expect("write header");
        header_only.flush().expect("flush");
        assert_window_matches(header_only.path(), None);
    }

    #[test]
    fn malformed_lines_are_skipped_like_the_pipeline() {
        let mut entries = chain_fixture();
        entries.push("not-json".to_string());
        entries.push(user_message("u2", "a3", "after garbage"));
        entries.push("{\"truncated\":".to_string());
        let file = write_session(&entries);
        assert_window_matches(file.path(), None);
    }

    #[test]
    fn compaction_at_leaf_gives_summary_only() {
        let entries = vec![
            user_message("u1", "sess1", "hello"),
            assistant_message("a1", "u1", "claude-x", 10),
            compaction("c1", "a1", "a1", "checkpoint"),
        ];
        let file = write_session(&entries);

        let window = assert_window_matches(file.path(), None);
        assert_eq!(window.source, WindowSource::CompactionCheckpoint);
        assert_eq!(window.context.messages.len(), 1);
    }

    fn corpus_path() -> std::path::PathBuf {
        let path = std::env::var("PA_FAST_OPEN_CORPUS")
            .unwrap_or_else(|_| "/tmp/corpus-42mb.jsonl".to_string());
        let path = std::path::PathBuf::from(path);
        assert!(
            path.is_file(),
            "corpus file missing: run this ignored test where the 42MB corpus exists (set PA_FAST_OPEN_CORPUS)"
        );
        path
    }

    #[test]
    #[ignore = "42MB corpus replay; run with cargo test --release -- --ignored where the corpus exists"]
    fn corpus_default_leaf_matches_full_parse() {
        let path = corpus_path();
        let window = assert_window_matches(&path, None);
        assert_eq!(window.source, WindowSource::ChainClosure);
    }

    #[test]
    #[ignore = "42MB corpus replay; run with cargo test --release -- --ignored where the corpus exists"]
    fn corpus_compaction_leaf_matches_full_parse() {
        let path = corpus_path();
        // The tip of the branch the last on-disk compaction (85a49d50)
        // checkpointed: the compaction sits on this leaf's parent chain.
        let window = assert_window_matches(&path, Some("a3186289"));
        assert_eq!(window.source, WindowSource::CompactionCheckpoint);
    }

    #[test]
    #[ignore = "42MB corpus replay; run with cargo test --release -- --ignored where the corpus exists"]
    fn corpus_window_loads_under_200ms() {
        let path = corpus_path();
        for leaf in [None, Some("a3186289")] {
            let started = std::time::Instant::now();
            let window = load_session_window(&path, leaf);
            let elapsed = started.elapsed();
            assert_eq!(
                window.source != WindowSource::FullRead,
                true,
                "expected a windowed load"
            );
            assert!(
                elapsed < std::time::Duration::from_millis(200),
                "window load took {elapsed:?} (leaf {leaf:?})"
            );
        }
    }
}
