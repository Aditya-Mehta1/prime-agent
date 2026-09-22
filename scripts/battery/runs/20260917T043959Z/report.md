# Parity battery run 20260917T043959Z

- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/trial-send/target/release/prime-agent
- flows: f1_launch, f6_attach, f8_resume

## Findings

3 gaps, 11 parity checks passed.

### f1_launch

- [visual] ts shows a first-run notice on fresh install: splash + 'Share agent traces with Prime Intellect?' dialog (Share / Not now, /traces hint) — evidence: ts/f1_launch/01-launch.txt
- [visual] Rust launches straight into the TUI: no splash ASCII art and no first-run trace-sharing notice — evidence: rust/f1_launch/01-launch.txt
### f8_resume

- [protocol] session entry types differ: ts-only=['custom_message'] rust-only=[]; full counts in f8_resume-session-shapes.json — evidence: f8_resume-session-shapes.json

## Passed checks

- [f1_launch/behavior] ts: first interactive prompt answered by the mock provider
- [f1_launch/protocol] ts: interactive model flags are authoritative (request model: mock-1)
- [f1_launch/behavior] rust: first interactive prompt answered by the mock provider
- [f1_launch/protocol] rust: interactive model flags are authoritative (request model: mock-1)
- [f6_attach/protocol] ts: wire attach returned a snapshot (data keys: ['activeSessionId', 'client', 'lastEventCursor', 'lastEventSequence', 'protocol', 'replay', 'snapshot'])
- [f6_attach/protocol] ts: attached client received 14 events during the turn
- [f6_attach/behavior] ts: CLI 'attach' opened the session in tmux (frame captured)
- [f6_attach/protocol] rust: wire attach returned a snapshot (data keys: ['activeSessionId', 'client', 'lastEventCursor', 'lastEventSequence', 'protocol', 'replay', 'snapshot'])
- [f6_attach/protocol] rust: attached client received 13 events during the turn
- [f6_attach/behavior] rust: CLI 'attach' opened the session in tmux (frame captured)
- [f8_resume/behavior] print '-c' refuses a session active in the daemon on both sides: a4100ebd3208 (ts) vs 65fe8b51283b (rust)
