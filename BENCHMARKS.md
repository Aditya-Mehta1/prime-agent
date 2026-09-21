# BENCHMARKS.md — TS-vs-Rust performance

The standing differential benchmark: every dimension runs the deployed TS
binary (`prime-agent` 0.9.5) and the Rust build side by side, on the same
quiet machine, through the same tmux pane channel / daemon wire a real user
drives, against the same deterministic mock provider. All numbers below are
medians unless noted.

The standing differential benchmark: every dimension runs the deployed TS
binary (`prime-agent` 0.9.5) and the Rust build side by side, on the same
quiet machine, through the same tmux pane channel / daemon wire a real user
drives, against the same deterministic mock provider. All numbers below are
medians unless noted.

## Methodology (run `perf-wave-20260921`)

- Hardware: Prime sandbox, x86-64 (Intel family 6 model 85, AVX-512),
  4 CPU cores, 16 GB RAM, network-enabled, plain `rust:1` image + apt
  tmux/procps/time. Both binaries ran on this one machine, interleaved
  trial by trial (`ts` trial i, `rust` trial i, …), so background noise
  hits both sides evenly.
- Binaries:
  - TS: the deployed release `prime-agent` 0.9.5
    (`0.9.5-linux-x64-bc4b0ed…`, bun build, the same tree the mission box
    runs), installed on PATH, `ts_identity`-guarded.
  - Rust: `cargo build --release -p pa-cli` of this repo at the PR's
    merge-base (`a8b0241f`, the current `origin/main`), 0.1.0.
- Trial counts: 5 per side per dimension (3 for the RSS dims, which are
  stable to ±3%), interleaved; medians reported. Keystroke latency pools
  all keystroke samples of the trials (125 per side). Full raw evidence:
  `scripts/battery/runs/perf-wave-20260921/` (per-trial JSON per side).
- Harness: `scripts/battery/perf_wave.py` (drives both sides through
  `batterylib` — isolated agent dirs, fresh daemon sockets, deterministic
  mock provider), executed inside the sandbox
  (`python3 scripts/battery/perf_wave.py --trials 5`). The whole wave takes
  ~10 minutes on the 4-core sandbox.
- Same-box rule: timings are only comparable when both sides share one
  quiet machine. The mission box (10+ lanes) is explicitly NOT a valid
  host — earlier battery perf rows taken on it under load showed the TS
  startup median swinging 0.8s → 9.6s purely from box noise. The wave
  runs in a fresh sandbox (`.github/workflows/benchmark.yml` /
  `scripts/battery/ci_perf_wave.sh` automate exactly that).

## Headline results (run `perf-wave-20260921`)

Measured 2026-09-21 on the perf-bench lane's sandbox. The Rust binary is a
`cargo build --release` of the lane worktree whose product code equals
`main` @ `0bda20d4` (the lane's rebase point; main moved 14 parity-fix
commits ahead during the lane — none perf-sensitive, and the harness rerun
is `scripts/battery/run_perf_wave.sh`).

| Dimension | TS 0.9.5 | Rust (main) | TS/Rust | vs original (pre-feature era) |
|---|---|---|---|---|
| Cold startup to interactive-ready | 1.539 s | 0.332 s | **4.6x faster** | was 7.8x |
| Typing latency, median (p95) | 41.1 ms (63.3) | 39.5 ms (82.1) | **parity** | — |
| Idle RSS after startup (daemon + TUI + kernel tree) | 998 MB | 166 MB | **6.0x lighter** | was 4.9x |
| RSS under load (3 attached sessions, kernel cells run) | 2,831 MB | 784 MB | **3.6x lighter** | was 16x (under load) |
| Session resume, 1,078-row transcript cold-load to ready | 1.450 s | 0.683 s | **2.1x faster** | was 3x |
| Kernel spawn: daemon create → first ipython cell ready (cold) | 0.789 s | 0.696 s | **1.1x faster** | new dimension |
| Kernel spawn: prompt → cell output, warm kernel | 0.052 s | 0.050 s | **parity** | new dimension |
| Streaming: unpaced ~11.4k-token turn, settle through TUI | 1.49 s (≈7.7k tok/s) | 0.69 s (≈16.5k tok/s) | **2.2x faster** | new dimension |
| Compaction, 40-turn grown session (~120k tokens) | 0.045 s | 0.054 s | **parity** (rust 1.2x slower, inside the 1.5x battery gate) | new dimension |
| HTML export of the 1,078-row session | 0.280 s | 0.036 s | **7.8x faster** | new dimension |
| Daemon overhead, 10 idle sessions (RSS delta) | 3,387 MB (339 MB/session) | 1,040 MB (104 MB/session) | **3.3x lighter** | new dimension |

### What the numbers say

- **No feature-era regression in the Rust differentials.** 250+ features
  (images, session trees, compaction, MCP, …) landed since the original
  wave and the Rust absolute numbers stayed small: 0.33 s cold startup,
  166 MB idle, 104 MB per idle daemon session, 36 ms HTML export of a
  1,078-row session. Startup (4.6x) and resume (2.1x) sit inside the
  original 7.8x / 3x ballparks measured on a different quiet host with a
  much smaller feature surface; every dimension that did not exist then is
  either clearly ahead (export 7.8x, streaming 2.2x, kernel cold 1.1x) or
  at parity (typing, warm kernel, compaction).
- **The TS-side ratios moved toward parity on latency.** The TS 0.9.5
  release is itself faster at startup than the build the original wave
  measured, and the original "16x under load" came from a much heavier,
  noisier load pattern; the fresh reproducible 3-session load number is
  3.6x. None of that is a Rust regression — the regression gate
  (`perf_gate.py`) tracks the Rust side against the recorded baseline.
- **Typing and warm-kernel latency are at parity, not ahead**: both are
  bounded by the same fixed tmux keystroke/render poll path (typing) and
  the same kernel round trip (warm cell). The battery f10 differential
  gate (≤1.5x) holds on both.
- **Run-to-run variance** across three clean waves on this sandbox class:
  startup ratio 4.6–5.1x, resume 2.1–2.9x, streaming settle ratio
  1.8–2.2x, idle-RSS ratio 6.0–7.7x (the idle number varies with whether
  the pre-spawned kernel had finished booting at sample time; the settle
  gate waits it out, so the recorded run counts it). The recorded run is
  the one whose evidence is committed.

### Findings the wave surfaced

1. **Compaction parity gap (Rust)**: growing a daemon session through
   `import_jsonl` makes the TS daemon compact it, while the Rust daemon
   answers `Session is too short to compact` — the imported rows reach
   the provider context (the next request carries all 4.5k messages) but
   not the engine's compaction view. Reproduced twice on `main` in the
   perf sandbox (2026-09-21); the benchmark grows sessions through live
   `prompt_and_wait` turns instead (identical on both sides), and the gap
   is reported for the compaction-owning lane to verify against the TS
   binary.
2. **First-touch kernel provisioning costs ~7 s on both sides** (the first
   session in a fresh agent dir installs the kernel runtime into a fresh
   venv). Both products pay it on the first ipython cell of a fresh
   install; the cold median of 5 trials excludes it, but a real fresh
   user's very first cell sees it.
3. The `streaming` settle is renderer-bound on both sides (the faux
   provider streams unpaced); tokens/s figures are the harness's
   chars/4 estimate over the pane-settle time — useful as a differential,
   not as an absolute model-throughput claim.

## Rust-internal kernel numbers (supplemental, `kernel_bench`)

Measured with `crates/pa-core/examples/kernel_bench` in the same sandbox
(fresh HOME + venv, `skills/` from the checkout):

| Metric | cold (fresh HOME + venv) | warm (second boot) |
|---|---|---|
| `ensure-kernel-python` (one-time venv + runtime install) | 10,772 ms | ~100 ms |
| `kernel-boot-and-bootstrap` (provisioner boot + bootstrap execute) | 464 ms | 195 ms |
| per-execute overhead, trivial cell (50 rounds) | 0.54 ms | 0.52 ms |
| snapshot / restore of a ~5 MiB namespace | — | 107 ms snapshot, 17 ms restore (idempotent re-restore 7 ms) |

Raw values: `scripts/battery/runs/perf-wave-20260921/kernel_bench/kernel_bench.json`.

## Reproducing

```bash
# exactly what CI runs (fresh Prime sandbox, both binaries, gate vs baseline):
make perf-wave
# or directly (PA_BENCH_NO_SANDBOX=1 to run on the bare runner):
scripts/battery/ci_perf_wave.sh
# via act (the workflow is staged at ci/workflows/benchmark.yml):
act -W ci/workflows/benchmark.yml

# locally (box or sandbox; the binary + TS install must both be present):
PA_WAVE_SKIP_BUILD=1 scripts/battery/run_perf_wave.sh
python3 scripts/battery/perf_gate.py --results scripts/battery/runs/perf-wave-<stamp>/results.json
```

The regression gate compares the fresh Rust medians against
`scripts/battery/perf-baseline.json` (recorded from the 2026-09-21 wave)
and flags any metric >10% worse; run `scripts/battery/perf_gate.py` after
every wave and commit the refreshed baseline with any accepted change.

## Pending dimensions (owned by other lanes)

### Kernel-snapshot lane

- **Resume with kernel snapshot: time from session-open to first cell with
  the namespace restored.** A session that ends with a kernel namespace
  (`kernel-state.dill` flushed on dispose) is reopened; the clock runs from
  session-open to the first `ipython` cell completing on the restored
  namespace. The Rust resume prewarm (TS `hasSnapshot` arm) boots
  spawn+restore+bootstrap in the background at session-open, so the
  first cell is a warm hit — the dimension is the full open-to-ready
  cost, not the lazy first-call latency. Measure through the same daemon
  wire a real user drives (attach + prompt), one trial per side.
