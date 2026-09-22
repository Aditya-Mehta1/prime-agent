# Parity battery run 20260922T075126Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/blank-pane/target/release/prime-agent
- flows: f1_launch, f6_attach, f8_resume, f9_agents_view

## Findings

4 gaps, 0 EXPECTED-FAIL (known gaps, owner lanes), 20 parity checks passed.

### f6_attach

- [protocol] attach event sequences differ: ts=12 rust=11; ts-only=['message_update:assistant'] rust-only=[] — evidence: f6_attach
### f8_resume

- [protocol] session entry types differ: ts-only=['custom_message'] rust-only=[]; full counts in f8_resume-session-shapes.json — evidence: f8_resume-session-shapes.json
### f9_agents_view

- [visual] agents view frames differ at 120x36 (see frame-diff-120x36.txt) — evidence: ts/f9_agents_view/frame-diff-120x36.txt
- [visual] agents view frames differ at 220x50 (see frame-diff-220x50.txt) — evidence: ts/f9_agents_view/frame-diff-220x50.txt

## Passed checks

- [f1_launch/behavior] ts: first interactive prompt answered by the mock provider
- [f1_launch/protocol] ts: interactive model flags are authoritative (request model: mock-1)
- [f1_launch/behavior] rust: first interactive prompt answered by the mock provider
- [f1_launch/protocol] rust: interactive model flags are authoritative (request model: mock-1)
- [f1_launch/visual] first-run splash + trace-sharing notice rendered and answerable on both sides (fresh install)
- [f6_attach/protocol] ts: wire attach returned a snapshot (data keys: ['activeSessionId', 'client', 'lastEventCursor', 'lastEventSequence', 'protocol', 'replay', 'snapshot'])
- [f6_attach/protocol] ts: attached client received 14 events during the turn
- [f6_attach/behavior] ts: CLI 'attach' opened the session in tmux (frame captured)
- [f6_attach/protocol] rust: wire attach returned a snapshot (data keys: ['activeSessionId', 'client', 'lastEventCursor', 'lastEventSequence', 'protocol', 'replay', 'snapshot'])
- [f6_attach/protocol] rust: attached client received 11 events during the turn
- [f6_attach/behavior] rust: CLI 'attach' opened the session in tmux (frame captured)
- [f8_resume/behavior] print '-c' refuses a session active in the daemon on both sides: 5b67b9baefa8 (ts) vs 7ca6bd09f6ce (rust)
- [f9_agents_view/behavior] ts: agents view groups the roster into Running/Idle/Inactive sections with the scripted sessions in place
- [f9_agents_view/behavior] ts: opening the Running row attached to the live session and its in-flight reply rendered
- [f9_agents_view/behavior] ts: the selection stayed on the same session row through live roster churn ('battery-f9-idle')
- [f9_agents_view/behavior] rust: agents view groups the roster into Running/Idle/Inactive sections with the scripted sessions in place
- [f9_agents_view/behavior] rust: opening the Running row attached to the live session and its in-flight reply rendered
- [f9_agents_view/behavior] rust: the selection stayed on the same session row through live roster churn ('battery-f9-idle')
- [f9_agents_view/visual] agents view frames identical at filtered-120x36 (normalized: paths, ids, ages)
- [f9_agents_view/visual] agents view frames identical at transcript-120x36 (normalized: paths, ids, ages)
