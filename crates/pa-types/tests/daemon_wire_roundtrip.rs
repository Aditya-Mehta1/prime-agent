//! Round-trip tests over real captured daemon wire data.
//!
//! The durable worker descriptors under `PA_TYPES_WORKERS_DIR` (default
//! `~/.prime/agent/daemon-workers`) are real `DaemonWorkerDescriptor` JSON
//! captures written by the live TS daemon. Every one must deserialize and
//! re-serialize losslessly. Skipped when the directory is absent (e.g. CI).

use pa_types::daemon::DaemonWorkerDescriptor;
use serde_json::Value;
use std::path::PathBuf;

fn workers_dir() -> Option<PathBuf> {
    std::env::var_os("PA_TYPES_WORKERS_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| {
                PathBuf::from(h)
                    .join(".prime")
                    .join("agent")
                    .join("daemon-workers")
            })
        })
        .filter(|p| p.is_dir())
}

#[test]
fn real_worker_descriptors_roundtrip_losslessly() {
    let Some(dir) = workers_dir() else {
        eprintln!("no daemon-workers dir present; skipping live-data test");
        return;
    };
    let mut count = 0usize;
    let mut stack = vec![dir];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).expect("read workers dir");
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = std::fs::read_to_string(&path).expect("read descriptor");
            let parsed: DaemonWorkerDescriptor = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("{}: deserialize failed: {e}", path.display()));
            let out = serde_json::to_string(&parsed).expect("serialize descriptor");
            let original: Value = serde_json::from_str(&raw).unwrap();
            let roundtripped: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(
                original,
                roundtripped,
                "{}: round trip changed the value",
                path.display()
            );
            count += 1;
        }
    }
    assert!(count > 0, "no worker descriptors found to verify");
    eprintln!("round-tripped {count} worker descriptors");
}

#[test]
fn committed_fixture_roundtrips() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/worker-descriptor.json"
    ))
    .unwrap();
    let parsed: DaemonWorkerDescriptor = serde_json::from_str(&raw).expect("deserialize");
    let out = serde_json::to_string(&parsed).expect("serialize");
    let original: Value = serde_json::from_str(&raw).unwrap();
    let roundtripped: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(original, roundtripped);
}
