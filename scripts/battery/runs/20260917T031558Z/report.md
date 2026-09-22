# Parity battery run 20260917T031558Z

- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/compaction2/target/release/prime-agent
- flows: f1_launch, f2_prompt, f3_tool, f4_commands, f5_side_questions, f6_attach, f7_compaction, f8_resume, f9_agents_view

## Findings

8 gaps, 23 parity checks passed.

### f1_launch

- [visual] ts shows a first-run notice on fresh install: splash + 'Share agent traces with Prime Intellect?' dialog (Share / Not now, /traces hint) — evidence: ts/f1_launch/01-launch.txt
- [visual] Rust launches straight into the TUI: no splash ASCII art and no first-run trace-sharing notice — evidence: rust/f1_launch/01-launch.txt
### f2_prompt

- [protocol] model tool surface differs: ts=['ipython'] rust=['bash', 'edit', 'ipython']
- [protocol] message roles differ: ts=['system', 'user', 'user'] rust=['system', 'user']
- [protocol] harness-digest user message: ts=True rust=False
- [protocol] system prompt text differs (ts 23131 chars vs rust 13534 chars); full texts in protocol-request-diff.txt — evidence: protocol-request-diff.txt
### f3_tool

- [protocol] session entry types differ: ts-only=['custom_message', 'service_tier_change'] rust-only=['custom']; full counts in f3_tool-session-shapes.json — evidence: f3_tool-session-shapes.json
### f8_resume

- [protocol] session entry types differ: ts-only=['custom_message', 'service_tier_change'] rust-only=['custom']; full counts in f8_resume-session-shapes.json — evidence: f8_resume-session-shapes.json

## Passed checks

- [f1_launch/behavior] ts: first interactive prompt answered by the mock provider
- [f1_launch/protocol] ts: interactive model flags are authoritative (request model: mock-1)
- [f1_launch/behavior] rust: first interactive prompt answered by the mock provider
- [f1_launch/protocol] rust: interactive model flags are authoritative (request model: mock-1)
- [f2_prompt/behavior] print-mode stdout identical: 'battery hello from mock'
- [f3_tool/behavior] ts: ipython tool call executed and output captured
- [f3_tool/behavior] rust: ipython tool call executed and output captured
- [f4_commands/visual] ts: '/' shows a slash-command menu
- [f4_commands/visual] rust: '/' shows a slash-command menu
- [f5_side_questions/protocol] ts: start_side_question answered via mock with side_question_event stream
- [f5_side_questions/protocol] rust: start_side_question answered via mock with side_question_event stream
- [f5_side_questions/protocol] post-turn status-line request issued by both sides (ts=4, rust=2 requests, model qwen/qwen3-30b-a3b-instruct-2507)
- [f6_attach/protocol] ts: wire attach returned a snapshot (data keys: ['activeSessionId', 'client', 'lastEventCursor', 'lastEventSequence', 'protocol', 'replay', 'snapshot'])
- [f6_attach/protocol] ts: attached client received 14 events during the turn
- [f6_attach/behavior] ts: CLI 'attach' opened the session in tmux (frame captured)
- [f6_attach/protocol] rust: wire attach returned a snapshot (data keys: ['activeSessionId', 'client', 'lastEventCursor', 'lastEventSequence', 'protocol', 'replay', 'snapshot'])
- [f6_attach/protocol] rust: attached client received 13 events during the turn
- [f6_attach/behavior] rust: CLI 'attach' opened the session in tmux (frame captured)
- [f7_compaction/protocol] ts: daemon 'compact' succeeded: {"summary": "pre-compaction reply 6", "firstKeptEntryId": "fb127fe3", "tokensBefore": 110, "details": {"readFiles": [], "modifiedFiles": []}}
- [f7_compaction/protocol] rust: daemon 'compact' succeeded: {"firstKeptEntryId": "648ab0fe", "summary": "pre-compaction reply 6", "tokensBefore": 110}
- [f8_resume/behavior] print '-c' refuses a session active in the daemon on both sides: 569e8c4b79b2 (ts) vs 31cb8b270e4d (rust)
- [f9_agents_view/visual] ts: agents view frame captured (see evidence); frame-level diffing belongs to the visual-parity lane
- [f9_agents_view/visual] rust: agents view frame captured (see evidence); frame-level diffing belongs to the visual-parity lane
