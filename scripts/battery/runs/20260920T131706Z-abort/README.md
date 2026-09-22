# 20260920T131706Z-abort — the stale-binary failure (defect 2 root cause)

Run against the MAIN CHECKOUT's prebuilt `target/release/prime-agent`
(mtime 2026-09-20 01:48 UTC — hours before the #204 auto-compaction
threshold merge). The TS side passes (capture.json, summarizer held and
aborted mid-flight); the Rust side fails with "the compaction summarizer
request never arrived" because that binary predates the whole
auto-compaction feature: `rust/mock-script.json.requests.jsonl` shows the
seed turn, the crossing turn (126010-token usage recorded in
`rust/agent/sessions/*.jsonl`), and then only the post-turn status-line
request — no threshold `compaction_start`, no summarizer request, and the
status-line request consumes the held summarizer entry (default-queue
fallthrough). A fresh build of the same main commit passes both sides
(see 20260920T131814Z-abort), which is why the harness now refuses a
rust binary older than the checkout's newest product commit.
