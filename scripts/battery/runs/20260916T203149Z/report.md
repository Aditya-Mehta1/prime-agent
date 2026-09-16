# Parity battery run 20260916T203149Z

- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/battery/target/release/prime-agent
- flows: f1_launch

## Findings

3 gaps, 1 parity checks passed.

### f1_launch

- [visual] TS shows a first-run notice on fresh install: splash + 'Share agent traces with Prime Intellect?' dialog (Share / Not now, /traces hint) — evidence: ts/f1_launch/01-launch.txt
- [visual] rust launch frame shows no splash/welcome text
- [behavior] rust: first interactive prompt did not reach the mock provider (mock requests: 0) — evidence: rust/f1_launch/02-after-first-prompt.txt

## Passed checks

- [f1_launch/behavior] ts: first interactive prompt answered by the mock provider
