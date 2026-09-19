# Parity battery run 20260919T042850Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /tmp/prime-agent-lane
- flows: f20_subagents

## Findings

0 gaps, 3 EXPECTED-FAIL (known gaps, owner lanes), 6 parity checks passed.

## Known gaps (EXPECTED-FAIL — evidence for the owning fix lanes)

- [f20_subagents/visual] EXPECTED-FAIL (lane: subagents-tui): spawn: frames differ TS vs Rust (see frame-diff-spawn.txt) — evidence: ts/f20_subagents/frame-diff-spawn.txt
- [f20_subagents/visual] EXPECTED-FAIL (lane: subagents-tui): child-status: frames differ TS vs Rust (see frame-diff-child-status.txt) — evidence: ts/f20_subagents/frame-diff-child-status.txt
- [f20_subagents/visual] EXPECTED-FAIL (lane: subagents-tui): scoped-agents: frames differ TS vs Rust (see frame-diff-scoped-agents.txt) — evidence: ts/f20_subagents/frame-diff-scoped-agents.txt


## Passed checks

- [f20_subagents/visual] ts: a kernel rlm.spawn renders the subagent summary line above the editor with the running/idle/inactive counts
- [f20_subagents/visual] ts: a child finishing without a reply renders the 'RLM child status' terminal-notice row in the parent transcript
- [f20_subagents/visual] ts: the scoped agents view lists the spawned child by name
- [f20_subagents/visual] rust: a kernel rlm.spawn renders the subagent summary line above the editor with the running/idle/inactive counts
- [f20_subagents/visual] rust: a child finishing without a reply renders the 'RLM child status' terminal-notice row in the parent transcript
- [f20_subagents/visual] rust: the scoped agents view lists the spawned child by name
