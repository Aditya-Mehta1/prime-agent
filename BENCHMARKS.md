# BENCHMARKS.md — TS-vs-Rust performance

The standing differential benchmark (see the `perf-bench` lane for the wave
harness and the 2026-09-21 baseline table): every dimension runs the
deployed TS binary and the Rust build side by side on one quiet machine.

## Pending dimension (kernel-snapshot lane)

- **Resume with kernel snapshot: time from session-open to first cell with
  the namespace restored.** A session that ends with a kernel namespace
  (`kernel-state.dill` flushed on dispose) is reopened; the clock runs from
  session-open to the first `ipython` cell completing on the restored
  namespace. The Rust resume prewarm (TS `hasSnapshot` arm) boots
  spawn+restore+bootstrap in the background at session-open, so the
  first cell is a warm hit — the dimension is the full open-to-ready
  cost, not the lazy first-call latency. Measure through the same daemon
  wire a real user drives (attach + prompt), one trial per side.
