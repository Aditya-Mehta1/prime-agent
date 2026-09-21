# Parity battery run 20260921T201932Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/panel-nav/target/release/prime-agent
- flows: f20_subagents

## Findings

0 gaps, 3 EXPECTED-FAIL (known gaps, owner lanes), 18 parity checks passed.

## Known gaps (EXPECTED-FAIL — evidence for the owning fix lanes)

- [f20_subagents/visual] EXPECTED-FAIL (lane: ipython-replay): reattached: frames differ TS vs Rust (see frame-diff-reattached.txt) — evidence: ts/f20_subagents/frame-diff-reattached.txt
- [f20_subagents/visual] EXPECTED-FAIL (lane: ipython-replay): panel-focused: frames differ TS vs Rust (see frame-diff-panel-focused.txt) — evidence: ts/f20_subagents/frame-diff-panel-focused.txt
- [f20_subagents/visual] EXPECTED-FAIL (lane: child-task-message): child-transcript: frames differ TS vs Rust (see frame-diff-child-transcript.txt) — evidence: ts/f20_subagents/frame-diff-child-transcript.txt


## Passed checks

- [f20_subagents/visual] ts: a kernel rlm.spawn renders the subagent summary line above the editor with the running/idle/inactive counts
- [f20_subagents/visual] ts: a child finishing without a reply renders the 'RLM child status' terminal-notice row in the parent transcript
- [f20_subagents/visual] ts: the scoped agents view lists the spawned child by name
- [f20_subagents/behavior] ts: the session-scoped mock routed the spawn turn deterministically (1 child-session request(s) to the child queue, 3 parent-session request(s) to the default queue)
- [f20_subagents/visual] ts: Down at the end of the prompt focuses the subagent panel (the hint flips to the focused open pair)
- [f20_subagents/visual] ts: Enter on the child row drills into the child transcript
- [f20_subagents/visual] ts: the agents-back key returns from the child transcript to the view
- [f20_subagents/visual] rust: a kernel rlm.spawn renders the subagent summary line above the editor with the running/idle/inactive counts
- [f20_subagents/visual] rust: a child finishing without a reply renders the 'RLM child status' terminal-notice row in the parent transcript
- [f20_subagents/visual] rust: the scoped agents view lists the spawned child by name
- [f20_subagents/behavior] rust: the session-scoped mock routed the spawn turn deterministically (1 child-session request(s) to the child queue, 3 parent-session request(s) to the default queue)
- [f20_subagents/visual] rust: Down at the end of the prompt focuses the subagent panel (the hint flips to the focused open pair)
- [f20_subagents/visual] rust: Enter on the child row drills into the child transcript
- [f20_subagents/visual] rust: the agents-back key returns from the child transcript to the view
- [f20_subagents/visual] spawn: frames identical TS vs Rust (normalized)
- [f20_subagents/visual] child-status: frames identical TS vs Rust (normalized)
- [f20_subagents/visual] scoped-agents: frames identical TS vs Rust (normalized)
- [f20_subagents/visual] back-to-view: frames identical TS vs Rust (normalized)
