# Parity battery run 20260921T154422Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/autonomous-ordering/target/debug/prime-agent
- flows: f18_goal_autonomous

## Findings

0 gaps, 6 EXPECTED-FAIL (known gaps, owner lanes), 9 parity checks passed.

## Known gaps (EXPECTED-FAIL — evidence for the owning fix lanes)

- [f18_goal_autonomous/visual] EXPECTED-FAIL (lane: goal-autonomous): goal-start: frames differ TS vs Rust (see frame-diff-goal-start.txt) — evidence: ts/f18_goal_autonomous/frame-diff-goal-start.txt
- [f18_goal_autonomous/visual] EXPECTED-FAIL (lane: goal-autonomous): goal-status: frames differ TS vs Rust (see frame-diff-goal-status.txt) — evidence: ts/f18_goal_autonomous/frame-diff-goal-status.txt
- [f18_goal_autonomous/visual] EXPECTED-FAIL (lane: goal-autonomous): goal-pause: frames differ TS vs Rust (see frame-diff-goal-pause.txt) — evidence: ts/f18_goal_autonomous/frame-diff-goal-pause.txt
- [f18_goal_autonomous/visual] EXPECTED-FAIL (lane: goal-autonomous): goal-resume: frames differ TS vs Rust (see frame-diff-goal-resume.txt) — evidence: ts/f18_goal_autonomous/frame-diff-goal-resume.txt
- [f18_goal_autonomous/visual] EXPECTED-FAIL (lane: goal-autonomous): goal-complete: frames differ TS vs Rust (see frame-diff-goal-complete.txt) — evidence: ts/f18_goal_autonomous/frame-diff-goal-complete.txt
- [f18_goal_autonomous/visual] EXPECTED-FAIL (lane: goal-autonomous): autonomous-on: frames differ TS vs Rust (see frame-diff-autonomous-on.txt) — evidence: ts/f18_goal_autonomous/frame-diff-autonomous-on.txt


## Passed checks

- [f18_goal_autonomous/visual] ts: /goal start renders the goal context row / active goal label
- [f18_goal_autonomous/visual] ts: /goal pause renders the paused row
- [f18_goal_autonomous/visual] ts: goal.complete() renders the completion row
- [f18_goal_autonomous/visual] ts: /autonomous on renders the autonomous status row
- [f18_goal_autonomous/visual] rust: /goal start renders the goal context row / active goal label
- [f18_goal_autonomous/visual] rust: /goal pause renders the paused row
- [f18_goal_autonomous/visual] rust: goal.complete() renders the completion row
- [f18_goal_autonomous/visual] rust: /autonomous on renders the autonomous status row
- [f18_goal_autonomous/visual] autonomous-off: frames identical TS vs Rust (normalized)
