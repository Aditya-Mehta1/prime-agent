# Parity battery run 20260918T091229Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/replay-perf/target/release/prime-agent
- flows: f12_scale_resume

## Findings

0 gaps, 3 parity checks passed.


## Passed checks

- [f12_scale_resume/perf] ts: 5000-turn (15261-row) interactive resume ready in 5.527s (first frame 5.43s)
- [f12_scale_resume/perf] rust: 5000-turn (15261-row) interactive resume ready in 7.321s (first frame 7.321s)
- [f12_scale_resume/perf] scale resume: rust 7.321s vs ts 5.527s for 15261 rows (ratio 1.32, thresholds: ratio <= 2.0, rust <= 30.0s)
