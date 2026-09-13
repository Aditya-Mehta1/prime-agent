---
name: pr-triage
description: Rank a GitHub repository's open pull-request queue via the gh CLI. Counts PRs by merge state and review decision, age and staleness, per-PR unresolved bot review threads (Cursor, Macroscope, Codex), a CLEAN-first review-ready ranking with READY verdicts, and a duplicate pre-flight that flags open PRs overlapping a planned change by changed files and title similarity. Use when asked which PRs are ready for review, how old, stale, or conflicted the queue is, or whether an open PR already covers planned work.
---

# PR Triage

Summarize a repository's open pull-request queue and pre-flight planned work
against open PRs, using the authenticated `gh` CLI. No credentials or API keys
beyond a working `gh` login are needed; run `gh auth status` if calls fail.

## Usage

Queue digest (which PRs are actually ready for a human?):

```python
digest = await pr_triage(repo="PrimeIntellect-ai/prime-agent")
print(digest)
```

The digest counts the queue by merge state and review decision, reports age
percentiles and per-author depth, and lists CLEAN pull requests oldest-first
with per-PR unresolved bot-thread counts and a READY verdict. Pass
`include_drafts=True` to include drafts, `stale_days=14` to adjust the
staleness flag, `limit=200` to fetch more of a long queue, or leave `repo`
empty inside a checkout to infer it from the origin remote.

Duplicate pre-flight (does an open PR already cover this work?):

```python
report = await pr_triage.check_overlaps(
    repo="PrimeIntellect-ai/prime-agent",
    title="Fix heartbeat listing",
    files=["packages/agent/src/daemon-supervisor.ts", "packages/agent/test/daemon-supervisor.test.ts"],
)
print(report)
```

Run it before starting work on a change and before opening a PR. Open PRs are
shortlisted by title similarity, then their changed files are fetched; PRs
sharing files with the candidate (Jaccard >= 0.3 or most candidate files
touched) are flagged with links. Pass `deep=True` to file-check every open PR
instead of only title-similar ones (one `gh` call per open PR, slower but
thorough for high-stakes work).

## Notes

- Thread-bot counts cap at the 50 most recent threads per PR and the file
  pre-flight at 300 files per PR; queues larger than `limit` note the cut-off.
- Staleness uses the latest non-bot review timestamp as a proxy for the last
  human touch; PRs without human reviews fall back to their creation date.
- Both functions return setup guidance instead of raising when `gh` is
  missing, and error text when a `gh` call fails.
