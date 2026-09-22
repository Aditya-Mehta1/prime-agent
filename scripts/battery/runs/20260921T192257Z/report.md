# Parity battery run 20260921T192257Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /tmp/av-base-musl
- flows: f9_agents_view

## Findings

1 gaps, 0 EXPECTED-FAIL (known gaps, owner lanes), 9 parity checks passed.

### f9_agents_view

- [behavior] rust: the selection teleported during roster churn (was 'battery-f9-idle', now 'battery-f9-busy') — evidence: rust/f9_agents_view/07-selection-post-churn-ansi.txt

## Passed checks

- [f9_agents_view/behavior] ts: agents view groups the roster into Running/Idle/Inactive sections with the scripted sessions in place
- [f9_agents_view/behavior] ts: opening the Running row attached to the live session and its in-flight reply rendered
- [f9_agents_view/behavior] ts: the selection stayed on the same session row through live roster churn ('battery-f9-idle')
- [f9_agents_view/behavior] rust: agents view groups the roster into Running/Idle/Inactive sections with the scripted sessions in place
- [f9_agents_view/behavior] rust: opening the Running row attached to the live session and its in-flight reply rendered
- [f9_agents_view/visual] agents view frames identical at 120x36 (normalized: paths, ids, ages)
- [f9_agents_view/visual] agents view frames identical at 220x50 (normalized: paths, ids, ages)
- [f9_agents_view/visual] agents view frames identical at filtered-120x36 (normalized: paths, ids, ages)
- [f9_agents_view/visual] agents view frames identical at transcript-120x36 (normalized: paths, ids, ages)
