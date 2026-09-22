# Parity battery run 20260917T035217Z

- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/gap-worker-2/target/release/prime-agent
- flows: f2_prompt, f3_tool, f8_resume

## Findings

4 gaps, 3 parity checks passed.

### f3_tool

- [protocol] session entry types differ: ts-only=['service_tier_change', 'session_state'] rust-only=[]; full counts in f3_tool-session-shapes.json — evidence: f3_tool-session-shapes.json
### f8_resume

- [behavior] ts: print '-c' did not refuse the active session (exit=0): resumed turn reply — evidence: ts/f8_resume/continue-cmd.json
- [behavior] rust: print '-c' did not refuse the active session (exit=0): resumed turn reply — evidence: rust/f8_resume/continue-cmd.json
- [protocol] session entry types differ: ts-only=['service_tier_change', 'session_state'] rust-only=[]; full counts in f8_resume-session-shapes.json — evidence: f8_resume-session-shapes.json

## Passed checks

- [f2_prompt/behavior] print-mode stdout identical: 'battery hello from mock'
- [f3_tool/behavior] ts: ipython tool call executed and output captured
- [f3_tool/behavior] rust: ipython tool call executed and output captured
