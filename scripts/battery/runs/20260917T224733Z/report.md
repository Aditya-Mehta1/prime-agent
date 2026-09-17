# Parity battery run 20260917T224733Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/fd-leak-audit/target/release/prime-agent
- flows: f9_agents_view

## Findings

1 gaps, 5 parity checks passed.

### f9_agents_view

- [visual] agents view frames differ at 220x50 (see frame-diff-220x50.txt) — evidence: ts/f9_agents_view/frame-diff-220x50.txt

## Passed checks

- [f9_agents_view/behavior] ts: agents view groups the roster into Running/Idle/Inactive sections with the scripted sessions in place
- [f9_agents_view/behavior] ts: opening the Running row attached to the live session and its in-flight reply rendered
- [f9_agents_view/behavior] rust: agents view groups the roster into Running/Idle/Inactive sections with the scripted sessions in place
- [f9_agents_view/behavior] rust: opening the Running row attached to the live session and its in-flight reply rendered
- [f9_agents_view/visual] agents view frames identical at 120x36 (normalized: paths, ids, ages)
