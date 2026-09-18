# Parity battery run 20260918T121001Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/tui-interaction/target/release/prime-agent
- flows: f12_scroll, f13_ctrlc_exit

## Findings

0 gaps, 5 parity checks passed.


## Passed checks

- [f12_scroll/behavior] ts: PageUp pages history into view with the follow hint; paging back to the tail resumes following
- [f12_scroll/behavior] rust: PageUp pages history into view with the follow hint; paging back to the tail resumes following
- [f13_ctrlc_exit/behavior] ts: C-c C-c exits in 0.18s (case healthy, resume hint after exit)
- [f13_ctrlc_exit/behavior] rust: C-c C-c exits in 0.08s (case healthy, exit code 0)
- [f13_ctrlc_exit/behavior] rust: C-c C-c exits in 0.08s (case wedged, exit code 0)
