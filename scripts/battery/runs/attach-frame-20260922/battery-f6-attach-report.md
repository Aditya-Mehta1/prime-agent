# Parity battery run 20260922T075433Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/blank-pane/target/release/prime-agent
- flows: f6_attach

## Findings

1 gaps, 0 EXPECTED-FAIL (known gaps, owner lanes), 6 parity checks passed.

### f6_attach

- [protocol] attach event sequences differ: ts=12 rust=11; ts-only=['message_update:assistant'] rust-only=[] — evidence: f6_attach

## Passed checks

- [f6_attach/protocol] ts: wire attach returned a snapshot (data keys: ['activeSessionId', 'client', 'lastEventCursor', 'lastEventSequence', 'protocol', 'replay', 'snapshot'])
- [f6_attach/protocol] ts: attached client received 14 events during the turn
- [f6_attach/behavior] ts: CLI 'attach' opened the session in tmux (frame captured)
- [f6_attach/protocol] rust: wire attach returned a snapshot (data keys: ['activeSessionId', 'client', 'lastEventCursor', 'lastEventSequence', 'protocol', 'replay', 'snapshot'])
- [f6_attach/protocol] rust: attached client received 11 events during the turn
- [f6_attach/behavior] rust: CLI 'attach' opened the session in tmux (frame captured)
