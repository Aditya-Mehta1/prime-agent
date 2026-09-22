# Parity battery run 20260918T042841Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/roster-wire/target/release/prime-agent
- flows: f2_prompt, f3_tool, f7_compaction, f8_resume

## Findings

1 gaps, 8 parity checks passed.

### f3_tool

- [protocol] session entry types differ: ts-only=['session_state'] rust-only=[]; full counts in f3_tool-session-shapes.json — evidence: f3_tool-session-shapes.json

## Passed checks

- [f2_prompt/behavior] print-mode stdout identical: 'battery hello from mock'
- [f2_prompt/protocol] system prompt: rust layered redesign (cached static layers + dynamic tail; TS-prompt parity superseded; raw prompts in protocol-request-diff.txt)
- [f3_tool/behavior] ts: ipython tool call executed and output captured
- [f3_tool/behavior] rust: ipython tool call executed and output captured
- [f7_compaction/protocol] ts: daemon 'compact' succeeded: {"summary": "pre-compaction reply 6", "firstKeptEntryId": "d33c3984", "tokensBefore": 110, "details": {"readFiles": [], "modifiedFiles": []}}
- [f7_compaction/protocol] rust: daemon 'compact' succeeded: {"firstKeptEntryId": "d8f8e5fe", "summary": "pre-compaction reply 6", "tokensBefore": 110}
- [f8_resume/behavior] print '-c' refuses a session active in the daemon on both sides: 2a5fee61639c (ts) vs 02073a81fa05 (rust)
- [f8_resume/protocol] session entry type sets match (['compaction', 'custom_message', 'message', 'model_change', 'service_tier_change', 'session', 'session_info', 'session_state', 'thinking_level_change'])
