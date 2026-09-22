# Parity battery run 20260920T024419Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/prime-agent-rs/target/release/prime-agent
- flows: f20_subagents

## Findings

0 gaps, 0 EXPECTED-FAIL (known gaps, owner lanes), 11 parity checks passed.


## Passed checks

- [f20_subagents/visual] ts: a kernel rlm.spawn renders the subagent summary line above the editor with the running/idle/inactive counts
- [f20_subagents/visual] ts: a child finishing without a reply renders the 'RLM child status' terminal-notice row in the parent transcript
- [f20_subagents/visual] ts: the scoped agents view lists the spawned child by name
- [f20_subagents/behavior] ts: the session-scoped mock routed the spawn turn deterministically (1 child-session request(s) to the child queue, 3 parent-session request(s) to the default queue)
- [f20_subagents/visual] rust: a kernel rlm.spawn renders the subagent summary line above the editor with the running/idle/inactive counts
- [f20_subagents/visual] rust: a child finishing without a reply renders the 'RLM child status' terminal-notice row in the parent transcript
- [f20_subagents/visual] rust: the scoped agents view lists the spawned child by name
- [f20_subagents/behavior] rust: the session-scoped mock routed the spawn turn deterministically (1 child-session request(s) to the child queue, 3 parent-session request(s) to the default queue)
- [f20_subagents/visual] spawn: frames identical TS vs Rust (normalized)
- [f20_subagents/visual] child-status: frames identical TS vs Rust (normalized)
- [f20_subagents/visual] scoped-agents: frames identical TS vs Rust (normalized)
