# Parity battery run 20260916T223657Z

- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/gap-cli/target/release/prime-agent
- flows: f1_launch, f2_prompt, f3_tool, f4_commands, f5_side_questions, f6_attach, f7_compaction, f8_resume, f9_agents_view

## Findings

9 gaps, 22 parity checks passed.

### f1_launch

- [visual] ts shows a first-run notice on fresh install: splash + 'Share agent traces with Prime Intellect?' dialog (Share / Not now, /traces hint) — evidence: ts/f1_launch/01-launch.txt
- [visual] Rust launches straight into the TUI: no splash ASCII art and no first-run trace-sharing notice — evidence: rust/f1_launch/01-launch.txt
### f2_prompt

- [protocol] model tool surface differs: ts=['ipython'] rust=['bash', 'edit', 'ipython']
- [protocol] message roles differ: ts=['system', 'user', 'user'] rust=['system', 'user']
- [protocol] harness-digest user message: ts=True rust=False
- [protocol] system prompt text differs (ts 23119 chars vs rust 13526 chars); full texts in protocol-request-diff.txt — evidence: protocol-request-diff.txt
### f3_tool

- [protocol] session entry types differ: ts-only=['custom_message', 'service_tier_change'] rust-only=['custom']; full counts in f3_tool-session-shapes.json — evidence: f3_tool-session-shapes.json
### f7_compaction

- [protocol] rust: daemon 'compact' failed: {"command": "unknown", "error": "Unknown active session: ", "id": "k1", "success": false, "type": "response"} — evidence: rust/f7_compaction/compact-response.json
### f8_resume

- [protocol] session entry types differ: ts-only=['compaction', 'custom_message', 'service_tier_change'] rust-only=['custom']; full counts in f8_resume-session-shapes.json — evidence: f8_resume-session-shapes.json

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
- [f7_compaction/protocol] ts: daemon 'compact' succeeded: {"summary": "pre-compaction reply 6", "firstKeptEntryId": "2695b07a", "tokensBefore": 110, "details": {"readFiles": [], "modifiedFiles": []}}
- [f8_resume/behavior] print '-c' refuses a session active in the daemon on both sides: 8d2a956cfa4b (ts) vs 3678e2605ec1 (rust)
- [f9_agents_view/visual] ts: agents view frame captured (see evidence); frame-level diffing belongs to the visual-parity lane
- [f9_agents_view/visual] rust: agents view frame captured (see evidence); frame-level diffing belongs to the visual-parity lane
