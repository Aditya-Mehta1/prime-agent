# Parity battery run 20260920T192358Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /tmp/lane-prime-agent-musl
- flows: f7_compaction

## Findings

0 gaps, 0 EXPECTED-FAIL (known gaps, owner lanes), 9 parity checks passed.


## Passed checks

- [f7_compaction/protocol] ts: daemon 'compact' succeeded: {"summary": "pre-compaction reply 6", "firstKeptEntryId": "60205f49", "tokensBefore": 110, "details": {"readFiles": [], "modifiedFiles": []}}
- [f7_compaction/protocol] rust: daemon 'compact' succeeded: {"details": {"modifiedFiles": [], "readFiles": []}, "firstKeptEntryId": "8852641f", "summary": "pre-compaction reply 6", "tokensBefore": 110}
- [f7_compaction/behavior] overflow compact-and-retry wire surface identical: [{"type": "assistant_error", "overflow": true}, {"type": "assistant_error", "overflow": true}, {"type": "compaction_start", "reason": "overflow"}, {"type": "compaction_end", "reason": "overflow", "willRetry": true, "hasResult": true, "errorMessage": null, "errorSeverity": null}, {"type": "assistant_
- [f7_compaction/behavior] split-turn compaction: two summarizer requests with identical prompt shapes: [{"user_text": ["<conversation>\n[User]: seed turn\n\n[Assistant]: seed reply\n</conversation>\n\nThe messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user
- [f7_compaction/behavior] split-turn durable compaction row identical (merged summary, summed usage): {"summary": "the history summary\n\n---\n\n**Turn Context (split turn):**\n\nthe turn prefix summary", "tokensBefore": 110, "details": {"readFiles": [], "modifiedFiles": []}, "fromHook": false, "usage": {"input": 40, "output": 20, "cacheRead": 160, "cacheWrite": 0, "totalTokens": 220, "cost": {"inpu
- [f7_compaction/behavior] second-compaction update-mode summarizer request identical (previous-summary merge, new history only): <conversation>
[User]: history turn two kept by the first compact

[Assistant]: second reply
</conversation>

<previous-summary>
the first compaction summary
</previous-summary>

The messages above ar
- [f7_compaction/behavior] iterative compaction durable rows identical (checkpoint + updated summary): [{"summary": "the first compaction summary", "tokensBefore": 110, "details": {"readFiles": [], "modifiedFiles": []}, "fromHook": false, "usage": {"input": 20, "output": 10, "cacheRead": 80, "cacheWrite": 0, "totalTokens": 110, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total
- [f7_compaction/behavior] durable compaction entries identical (tokensBefore, fromHook, details, usage, harnessDigest; normalized ids/timestamps): [{"summary": "pre-compaction reply 6", "tokensBefore": 110, "details": {"readFiles": [], "modifiedFiles": []}, "fromHook": false, "usage": {"input": 20, "output": 10, "cacheRead": 80, "cacheWrite": 0, "totalTokens": 110, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}}
- [f7_compaction/behavior] kernel-notice parity: ipython_state row after each compaction, back-to-back second compact runs (update mode): [{"customType": "ipython_state", "display": false, "content_shape": "[python-state]\n\nYour Python kernel persisted through compaction; its remaining variables, imports, and helpers are still available."}, {"customType": "ipython_state", "display": false, "content_shape": "[python-state]\n\nYour Pyt
