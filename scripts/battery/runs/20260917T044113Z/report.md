# Parity battery run 20260917T044113Z

- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/trial-send/target/release/prime-agent
- flows: f2_prompt, f3_tool

## Findings

1 gaps, 3 parity checks passed.

### f3_tool

- [protocol] session entry types differ: ts-only=['session_state'] rust-only=[]; full counts in f3_tool-session-shapes.json — evidence: f3_tool-session-shapes.json

## Passed checks

- [f2_prompt/behavior] print-mode stdout identical: 'battery hello from mock'
- [f3_tool/behavior] ts: ipython tool call executed and output captured
- [f3_tool/behavior] rust: ipython tool call executed and output captured
