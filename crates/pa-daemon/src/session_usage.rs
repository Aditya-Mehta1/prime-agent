//! Whole-file own-usage scan: the single source of truth for a session's
//! own token/cost summary.
//!
//! Port of the TS session-listing fold (`foldSessionScanLine`'s usage arms +
//! `snapshotSessionInfo`, `core/session-manager.ts`): assistant usage
//! keyed by entry id, a `child_usage_attributed` entry replacing the raw
//! block with its latest aggregate while every child block accumulates,
//! summarization (`compaction` / `branch_summary`) usage added, and all
//! attributed child usage subtracted. The child's own row carries the
//! child spend, so recursive rollups never double count. TS's live
//! `getOwnUsageSummary` documents the same contract: "Whole-file own
//! spend, identical to the catalog scan so rows never shift at
//! passivation" (`agent-session.ts`).
//!
//! Consumers: the saved-session listing scan (`session_store` feeds one
//! [`UsageScan`] while parsing each line) and the worker's live own-usage
//! summary ([`read_own_usage_summary`]). The live side layers only what
//! never reaches the file — child spend attributed in memory ahead of the
//! durable entry, and memoization — on top of this fold.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use pa_types::ai::Usage;
use serde::{Deserialize, Serialize};

use pa_core::session_engine::compaction_exec::{add_assistant_usage, subtract_assistant_usage};

/// TS `SessionUsageSummary` (`sessionUsageSummaryFrom`): the token/cost
/// summary rows publish. `inputTokens` folds cache reads and writes into
/// the input total; `cost` is the provider-billed total.
///
/// `Eq` is manual: `cost` is the f64 under `JsNumber`, and JSON numbers are
/// always finite (a NaN never round-trips `serde_json`), so equality is
/// total.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsageSummary {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: f64,
}

impl Eq for SessionUsageSummary {}

/// TS `sessionUsageSummaryFrom`: `None` — an absent wire field — when the
/// session recorded no billable work at all.
pub fn session_usage_summary_from(usage: &Usage) -> Option<SessionUsageSummary> {
    let input_tokens = usage.input + usage.cache_read + usage.cache_write;
    if input_tokens == 0 && usage.output == 0 && usage.cost.total.as_f64() == 0.0 {
        return None;
    }
    Some(SessionUsageSummary {
        input_tokens,
        output_tokens: usage.output,
        cost: usage.cost.total.as_f64(),
    })
}

/// The per-assistant usage map. TS uses a `Map`: a later write replaces in
/// place and iteration keeps first-insertion order — the final summary
/// sums the cost floats in exactly the order TS does.
#[derive(Default)]
struct AssistantUsageById {
    entries: Vec<(String, Usage)>,
}

impl AssistantUsageById {
    fn contains(&self, id: &str) -> bool {
        self.entries.iter().any(|(key, _)| key == id)
    }

    fn set(&mut self, id: &str, usage: Usage) {
        for (key, value) in &mut self.entries {
            if key == id {
                *value = usage;
                return;
            }
        }
        self.entries.push((id.to_string(), usage));
    }
}

/// The streaming whole-file own-usage accumulator: feed one entry at a
/// time in file order, then read [`summary`](Self::summary). Line order is
/// the fold's authority — an attribution folds only when its target is
/// already in the map (the assistant entry precedes its children's settle
/// in the file).
#[derive(Default)]
pub struct UsageScan {
    assistant_usage_by_id: AssistantUsageById,
    attributed_child_usage: Usage,
    summarization_usage: Usage,
}

impl UsageScan {
    /// TS `foldSessionScanLine`: the raw assistant usage keyed by entry id.
    /// Only an assistant row with a usage block lands in the map.
    pub(crate) fn fold_message(&mut self, id: &str, role: Option<&str>, usage: Option<&Usage>) {
        if role != Some("assistant") {
            return;
        }
        if let Some(usage) = usage {
            self.assistant_usage_by_id.set(id, *usage);
        }
    }

    /// TS `foldSessionScanLine`: a `child_usage_attributed` entry folds
    /// only when its target is already in the map — the latest aggregate
    /// replaces the raw block while every child block accumulates. A
    /// malformed attribution (missing aggregate or child block)
    /// contributes nothing; well-formed files always carry both.
    pub(crate) fn fold_child_attribution(
        &mut self,
        target_id: Option<&str>,
        child_usage: Option<&Usage>,
        aggregate_usage: Option<&Usage>,
    ) {
        let (Some(target_id), Some(child_usage), Some(aggregate_usage)) =
            (target_id, child_usage, aggregate_usage)
        else {
            return;
        };
        if self.assistant_usage_by_id.contains(target_id) {
            self.assistant_usage_by_id.set(target_id, *aggregate_usage);
            add_assistant_usage(&mut self.attributed_child_usage, child_usage);
        }
    }

    /// TS `foldSessionScanLine`: a `compaction` or `branch_summary`
    /// entry's own usage (the summarization call's billed block).
    pub(crate) fn fold_summarization(&mut self, usage: Option<&Usage>) {
        if let Some(usage) = usage {
            add_assistant_usage(&mut self.summarization_usage, usage);
        }
    }

    /// TS `snapshotSessionInfo`'s total: the assistant aggregates plus the
    /// summarization calls, minus every attributed child block (clamped
    /// at zero to absorb attribution drift).
    pub fn summary(&self) -> Option<SessionUsageSummary> {
        let mut total = Usage::default();
        for (_, usage) in &self.assistant_usage_by_id.entries {
            add_assistant_usage(&mut total, usage);
        }
        add_assistant_usage(&mut total, &self.summarization_usage);
        subtract_assistant_usage(&mut total, &self.attributed_child_usage);
        session_usage_summary_from(&total)
    }
}

/// The standalone scanner's minimal entry parse: only the usage fold's
/// fields, so unknown (and large) content is skipped by serde.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScanEntry {
    #[serde(rename = "type")]
    type_: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    message: Option<ScanMessage>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    target_id: Option<String>,
    #[serde(default)]
    child_usage: Option<Usage>,
    #[serde(default)]
    aggregate_usage: Option<Usage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScanMessage {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

impl ScanEntry {
    /// The standalone scanner's dispatch: fold this parsed entry into a
    /// scan (the same fold the listing scan drives).
    fn fold_into(&self, scan: &mut UsageScan) {
        match self.type_.as_str() {
            "message" => {
                let (role, usage) = self.message.as_ref().map_or((None, None), |message| {
                    (message.role.as_deref(), message.usage.as_ref())
                });
                scan.fold_message(&self.id, role, usage);
            }
            "child_usage_attributed" => scan.fold_child_attribution(
                self.target_id.as_deref(),
                self.child_usage.as_ref(),
                self.aggregate_usage.as_ref(),
            ),
            "compaction" | "branch_summary" => scan.fold_summarization(self.usage.as_ref()),
            _ => {}
        }
    }
}

/// Whole-file own usage ([`UsageScan`] over every parsable line): the
/// worker's live own-usage summary reads this, so live rows and saved rows
/// never disagree. Invalid lines contribute nothing, exactly like the
/// listing scan.
pub fn read_own_usage_summary(path: &Path) -> Option<SessionUsageSummary> {
    let file = fs::File::open(path).ok()?;
    let mut scan = UsageScan::default();
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<ScanEntry>(trimmed) else {
            continue;
        };
        entry.fold_into(&mut scan);
    }
    scan.summary()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage(input: u64, output: u64, total: f64) -> Usage {
        serde_json::from_value(json!({
            "input": input, "output": output, "cacheRead": 0, "cacheWrite": 0,
            "totalTokens": input + output,
            "cost": { "input": 0.0, "output": total, "cacheRead": 0.0, "cacheWrite": 0.0, "total": total }
        }))
        .unwrap()
    }

    fn scan_summary(lines: &[Value]) -> Option<SessionUsageSummary> {
        let mut scan = UsageScan::default();
        for line in lines {
            let entry: ScanEntry = serde_json::from_value(line.clone()).unwrap();
            entry.fold_into(&mut scan);
        }
        scan.summary()
    }

    fn message(id: &str, role: &str, usage: Value) -> Value {
        json!({ "type": "message", "id": id, "message": { "role": role, "usage": usage } })
    }

    fn attribution(target: &str, child: Value, aggregate: Value) -> Value {
        json!({
            "type": "child_usage_attributed", "targetId": target,
            "childUsage": child, "aggregateUsage": aggregate
        })
    }

    /// TS `snapshotSessionInfo` on a parent whose child settled twice: the
    /// latest aggregate replaces the raw block, every child block
    /// accumulates, and the summary subtracts the child spend (the child's
    /// own row carries it — no rollup double count).
    #[test]
    fn latest_aggregate_replaces_raw_and_child_usage_accumulates() {
        let summary = scan_summary(&[
            message("a", "assistant", json!({
                "input": 100, "output": 10, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 110,
                "cost": { "input": 0.0, "output": 1.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 1.0 }
            })),
            attribution("a", usage(20, 2, 0.2), usage(120, 12, 1.2)),
            attribution("a", usage(30, 3, 0.3), usage(150, 15, 1.5)),
            message("b", "assistant", json!({
                "input": 50, "output": 5, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 55,
                "cost": { "input": 0.0, "output": 0.5, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.5 }
            })),
        ]);
        assert_eq!(
            summary,
            Some(SessionUsageSummary { input_tokens: 150, output_tokens: 15, cost: 1.5 })
        );
    }

    /// TS folds an attribution only when its target is already in the map:
    /// an attribution ahead of its assistant entry (or aimed at a missing
    /// one) contributes nothing.
    #[test]
    fn attribution_without_a_present_target_folds_nothing() {
        let assistant = message("a", "assistant", json!({
            "input": 40, "output": 4, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 44,
            "cost": { "input": 0.0, "output": 0.4, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.4 }
        }));
        let orphan = attribution("a", usage(30, 3, 0.3), usage(70, 7, 0.7));
        let ahead_of_target = attribution("z", usage(1, 1, 0.1), usage(41, 5, 0.5));

        let before = scan_summary(&[orphan.clone(), ahead_of_target.clone(), assistant.clone()]);
        let after = scan_summary(&[assistant, orphan, ahead_of_target]);
        assert_eq!(
            before,
            Some(SessionUsageSummary { input_tokens: 40, output_tokens: 4, cost: 0.4 })
        );
        assert_eq!(before, after);
    }

    /// `compaction` and `branch_summary` entries carry the summarization
    /// call's own billed usage into the summary.
    #[test]
    fn summarization_usage_is_added() {
        let summary = scan_summary(&[
            message("a", "assistant", json!({
                "input": 100, "output": 10, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 110,
                "cost": { "input": 0.0, "output": 1.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 1.0 }
            })),
            json!({
                "type": "compaction", "id": "c",
                "usage": { "input": 200, "output": 20, "cacheRead": 5, "cacheWrite": 0,
                           "totalTokens": 225,
                           "cost": { "input": 0.1, "output": 0.2, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.3 } }
            }),
            json!({
                "type": "branch_summary", "id": "b",
                "usage": { "input": 50, "output": 5, "cacheRead": 0, "cacheWrite": 0,
                           "totalTokens": 55,
                           "cost": { "input": 0.0, "output": 0.1, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.1 } }
            }),
        ]);
        // 100 + 200 + 5 + 50 input tokens; 10 + 20 + 5 output; $1.0 + $0.3 + $0.1.
        assert_eq!(
            summary,
            Some(SessionUsageSummary { input_tokens: 355, output_tokens: 35, cost: 1.4000000000000001 })
        );
    }

    /// A session with no billable work publishes no usage field at all
    /// (TS `sessionUsageSummaryFrom` returns undefined).
    #[test]
    fn no_billable_work_is_none() {
        assert_eq!(scan_summary(&[]), None);
        assert_eq!(
            scan_summary(&[
                message("u", "user", json!({ "input": 10, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 10, "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0 } })),
                message("a", "assistant", json!({ "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0, "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0 } })),
            ]),
            None
        );
    }

    /// TS `subtractAssistantUsage` clamps at zero to absorb attribution
    /// drift (child spend the aggregates never folded).
    #[test]
    fn child_attribution_drift_clamps_at_zero() {
        let summary = scan_summary(&[
            message("a", "assistant", json!({
                "input": 10, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 11,
                "cost": { "input": 0.0, "output": 0.1, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.1 }
            })),
            // Drifted child spend larger than the aggregate folds in.
            attribution("a", usage(900, 90, 0.9), usage(10, 1, 0.1)),
        ]);
        assert_eq!(summary, Some(SessionUsageSummary { input_tokens: 0, output_tokens: 0, cost: 0.0 }));
    }

    /// The map keeps first-insertion order so the cost sums stay
    /// bit-identical to the TS `Map` fold (a HashMap would reorder them).
    #[test]
    fn cost_sums_follow_insertion_order() {
        let line = |id: &str, cost: f64| {
            message(id, "assistant", json!({
                "input": 10, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 11,
                "cost": { "input": 0.0, "output": cost, "cacheRead": 0.0, "cacheWrite": 0.0, "total": cost }
            }))
        };
        let summary = scan_summary(&[line("a", 0.1), line("b", 0.2), line("c", 0.3)]);
        assert_eq!(summary.map(|s| s.cost), Some(0.1 + 0.2 + 0.3));
        assert_eq!(summary.map(|s| s.cost), Some(0.6000000000000001));
    }

    /// The standalone whole-file scan reads the same fold from disk;
    /// unparsable lines contribute nothing.
    #[test]
    fn read_own_usage_summary_scans_a_file() {
        let dir = std::env::temp_dir()
            .join(format!("session-usage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"session\",\"id\":\"s\",\"timestamp\":\"2026-09-23T00:00:00.000Z\",\"cwd\":\"/t\"}\n",
                "{\"type\":\"message\",\"id\":\"a\",\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":100,\"output\":10,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":110,\"cost\":{\"input\":0.0,\"output\":1.0,\"cacheRead\":0.0,\"cacheWrite\":0.0,\"total\":1.0}}}}\n",
                "{\"type\":\"child_usage_attributed\",\"targetId\":\"a\",\"childUsage\":{\"input\":30,\"output\":3,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":33,\"cost\":{\"input\":0.0,\"output\":0.3,\"cacheRead\":0.0,\"cacheWrite\":0.0,\"total\":0.3}},\"aggregateUsage\":{\"input\":130,\"output\":13,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":143,\"cost\":{\"input\":0.0,\"output\":1.3,\"cacheRead\":0.0,\"cacheWrite\":0.0,\"total\":1.3}}}\n",
                "not json at all\n",
            ),
        )
        .unwrap();
        assert_eq!(
            read_own_usage_summary(&path),
            Some(SessionUsageSummary { input_tokens: 100, output_tokens: 10, cost: 1.0 })
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The captured-session numeric parity harness (manual run in the gate
    /// VM): `SAVED_USAGE_FIXTURE=<captured .jsonl> cargo test --ignored`.
    /// The devbox parent fixture (228 entries, six attributions onto one
    /// target) pins the whole fold against the TS-computed summary of the
    /// same file: own spend only — the attributed child spend (input
    /// 50,208 / output 2,929 / $0.0089957) stays on the child rows.
    #[test]
    #[ignore = "needs a captured session fixture (SAVED_USAGE_FIXTURE)"]
    fn captured_session_summary_matches_the_ts_fold() {
        let Some(path) = std::env::var_os("SAVED_USAGE_FIXTURE") else {
            return;
        };
        let summary = read_own_usage_summary(Path::new(&path)).unwrap();
        assert_eq!(
            summary,
            Some(SessionUsageSummary { input_tokens: 1_505_509, output_tokens: 14_472, cost: 0.0 })
        );
    }
}
