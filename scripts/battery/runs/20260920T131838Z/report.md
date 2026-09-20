# Parity battery run 20260920T131838Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/harness-debt/bin/prime-agent-musl
- flows: f7_compaction, f14_compact

## Findings

0 gaps, 0 EXPECTED-FAIL (known gaps, owner lanes), 15 parity checks passed.


## Passed checks

- [f7_compaction/protocol] ts: daemon 'compact' succeeded: {"summary": "pre-compaction reply 6", "firstKeptEntryId": "67830e16", "tokensBefore": 110, "details": {"readFiles": [], "modifiedFiles": []}}
- [f7_compaction/protocol] rust: daemon 'compact' succeeded: {"details": {"modifiedFiles": [], "readFiles": []}, "firstKeptEntryId": "f0a82cff", "summary": "pre-compaction reply 6", "tokensBefore": 110}
- [f7_compaction/behavior] overflow compact-and-retry wire surface identical: [{"type": "assistant_error", "overflow": true}, {"type": "assistant_error", "overflow": true}, {"type": "compaction_start", "reason": "overflow"}, {"type": "compaction_end", "reason": "overflow", "willRetry": true, "hasResult": true, "errorMessage": null, "errorSeverity": null}, {"type": "assistant_
- [f7_compaction/behavior] durable compaction entries identical (tokensBefore, fromHook, details, usage, harnessDigest; normalized ids/timestamps): [{"summary": "pre-compaction reply 6", "tokensBefore": 110, "details": {"readFiles": [], "modifiedFiles": []}, "fromHook": false, "usage": {"input": 20, "output": 10, "cacheRead": 80, "cacheWrite": 0, "totalTokens": 110, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}}
- [f14_compact/visual] ts: /compact shows the durable '◆ Context compacted' summary row
- [f14_compact/visual] ts: the Ctrl+O detail cycle expands the compaction summary block
- [f14_compact/visual] ts: the third Ctrl+O re-collapses the compaction summary block
- [f14_compact/behavior] ts: crossing the compaction threshold auto-compacts and shows the summary row
- [f14_compact/visual] rust: /compact shows the durable '◆ Context compacted' summary row
- [f14_compact/visual] rust: the Ctrl+O detail cycle expands the compaction summary block
- [f14_compact/visual] rust: the third Ctrl+O re-collapses the compaction summary block
- [f14_compact/behavior] rust: crossing the compaction threshold auto-compacts and shows the summary row
- [f14_compact/visual] manual-compact: frames identical TS vs Rust (normalized)
- [f14_compact/visual] manual-expanded: frames identical TS vs Rust (normalized)
- [f14_compact/visual] auto-compact: frames identical TS vs Rust (normalized)
