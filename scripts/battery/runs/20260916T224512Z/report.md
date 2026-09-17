# Parity battery run 20260916T224512Z

- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/gap-model/target/release/prime-agent
- flows: f1_launch, f2_prompt, f3_tool, f4_commands, f5_side_questions, f6_attach, f7_compaction, f8_resume, f9_agents_view

## Findings

7 gaps, 18 parity checks passed.

### f1_launch

- [visual] ts shows a first-run notice on fresh install: splash + 'Share agent traces with Prime Intellect?' dialog (Share / Not now, /traces hint) — evidence: ts/f1_launch/01-launch.txt
- [visual] rust launch frame shows no splash/welcome text
### f3_tool

- [protocol] session entry types differ: ts-only=['service_tier_change'] rust-only=['custom']; full counts in f3_tool-session-shapes.json — evidence: f3_tool-session-shapes.json
### f4_commands

- [visual] rust: '/' did not show a command menu — evidence: rust/f4_commands/01-slash-menu.txt
### f7_compaction

- [protocol] rust: daemon 'compact' failed: {"command": "unknown", "error": "Unknown active session: ", "id": "k1", "success": false, "type": "response"} — evidence: rust/f7_compaction/compact-response.json
### f8_resume

- [behavior] ts: print '-c' failed (exit=1): Error: Session is already active in 5779fb523aa1: /home/ubuntu/lane-worktrees/gap-model/scripts/battery/runs/20260916T224512Z/ts/agent/sessions/01a0ac66-ba20-739a-965c-d97df0bbabf5.jsonl
- [protocol] session entry types differ: ts-only=['compaction', 'service_tier_change'] rust-only=['custom']; full counts in f8_resume-session-shapes.json — evidence: f8_resume-session-shapes.json

## Passed checks

- [f1_launch/behavior] ts: first interactive prompt answered by the mock provider
- [f1_launch/behavior] rust: first interactive prompt answered by the mock provider
- [f2_prompt/behavior] print-mode stdout identical: 'battery hello from mock'
- [f3_tool/behavior] ts: ipython tool call executed and output captured
- [f3_tool/behavior] rust: ipython tool call executed and output captured
- [f4_commands/visual] ts: '/' shows a slash-command menu
- [f5_side_questions/protocol] ts: start_side_question answered via mock with side_question_event stream
- [f5_side_questions/protocol] rust: start_side_question answered via mock with side_question_event stream
- [f6_attach/protocol] ts: wire attach returned a snapshot (data keys: ['activeSessionId', 'client', 'lastEventCursor', 'lastEventSequence', 'protocol', 'replay', 'snapshot'])
- [f6_attach/protocol] ts: attached client received 14 events during the turn
- [f6_attach/behavior] ts: CLI 'attach' opened the session in tmux (frame captured)
- [f6_attach/protocol] rust: wire attach returned a snapshot (data keys: ['activeSessionId', 'client', 'lastEventCursor', 'lastEventSequence', 'protocol', 'replay', 'snapshot'])
- [f6_attach/protocol] rust: attached client received 12 events during the turn
- [f6_attach/behavior] rust: CLI 'attach' opened the session in tmux (frame captured)
- [f7_compaction/protocol] ts: daemon 'compact' succeeded: {"summary": "pre-compaction reply 6", "firstKeptEntryId": "6634ebb2", "tokensBefore": 110, "details": {"readFiles": [], "modifiedFiles": []}}
- [f8_resume/behavior] rust: print '-c' continued the previous session
- [f9_agents_view/visual] ts: agents view frame captured (see evidence); frame-level diffing belongs to the visual-parity lane
- [f9_agents_view/visual] rust: agents view frame captured (see evidence); frame-level diffing belongs to the visual-parity lane
