# Parity battery run 20260919T001719Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/exit-hang/target/release/prime-agent
- flows: f13_ctrlc_exit

## Findings

0 gaps, 4 parity checks passed.


## Passed checks

- [f13_ctrlc_exit/behavior] ts: C-c C-c exits in 0.14s (case healthy, resume hint after exit)
- [f13_ctrlc_exit/behavior] rust: C-c C-c exits in 0.37s (case healthy, exit code 0)
- [f13_ctrlc_exit/behavior] rust: C-c C-c exits in 0.36s (case wedged, exit code 0)
- [f13_ctrlc_exit/behavior] rust: C-c C-c exits in 0.37s (case daemon_dead, exit code 0)
