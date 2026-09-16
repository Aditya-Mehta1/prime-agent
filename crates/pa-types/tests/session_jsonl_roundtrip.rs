//! Round-trip tests over real captured session JSONL.
//!
//! Verifier for the `pa-types` session port: every line of every session file
//! under `PA_TYPES_SESSIONS_DIR` (default `~/.prime/agent/sessions`) must
//! deserialize into [`FileEntry`] and re-serialize to the same JSON value.
//! The directory is skipped when absent (e.g. CI) so the fixture test below
//! still guarantees coverage.

use pa_types::session::FileEntry;
use serde_json::Value;
use std::path::PathBuf;

fn sessions_dir() -> Option<PathBuf> {
    std::env::var_os("PA_TYPES_SESSIONS_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| {
                PathBuf::from(h)
                    .join(".prime")
                    .join("agent")
                    .join("sessions")
            })
        })
        .filter(|p| p.is_dir())
}

fn roundtrip_file(path: &std::path::Path) -> usize {
    let data = std::fs::read_to_string(path).expect("read session file");
    let mut lines = 0usize;
    for (n, line) in data.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed: FileEntry = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("{}:{}: deserialize failed: {e}", path.display(), n + 1));
        let reserialized = serde_json::to_string(&parsed).expect("serialize entry");
        let original: Value = serde_json::from_str(line).unwrap();
        let roundtripped: Value = serde_json::from_str(&reserialized).unwrap();
        assert_eq!(
            original,
            roundtripped,
            "{}:{}: round trip changed the value\n  {}\n  {}",
            path.display(),
            n + 1,
            line,
            reserialized
        );
        lines += 1;
    }
    lines
}

#[test]
fn real_captured_sessions_roundtrip_losslessly() {
    let Some(dir) = sessions_dir() else {
        eprintln!("no sessions dir present; skipping live-data test");
        return;
    };
    let mut files = 0usize;
    let mut lines = 0usize;
    let mut stack = vec![dir];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).expect("read sessions dir");
        for entry in entries {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                lines += roundtrip_file(&path);
                files += 1;
            }
        }
    }
    assert!(files > 0, "no session files found to verify");
    eprintln!("round-tripped {lines} lines across {files} session files");
}

#[test]
fn committed_fixture_roundtrips() {
    for entry in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        roundtrip_file(&path);
    }
}
