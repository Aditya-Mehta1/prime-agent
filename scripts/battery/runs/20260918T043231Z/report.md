# Parity battery run 20260918T043231Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/roster-wire/target/release/prime-agent
- flows: f3_tool

## Findings

1 gaps, 2 parity checks passed.

### f3_tool

- [protocol] session entry types differ: ts-only=['session_state'] rust-only=[]; full counts in f3_tool-session-shapes.json — evidence: f3_tool-session-shapes.json

## Passed checks

- [f3_tool/behavior] ts: ipython tool call executed and output captured
- [f3_tool/behavior] rust: ipython tool call executed and output captured
