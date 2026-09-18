# Parity battery

NOTE: battery greenness ("0 gaps") covers only the scripted flows f1-f11 run
here over the deterministic mock provider - it is not a product-parity
statement. See `docs/completion-matrix.md` for the evidence-based
completion picture, and `docs/parity-battery.md` for the flow list.

One-command re-run (from the repo root):

    python3 scripts/battery/run_battery.py

Each run writes `scripts/battery/runs/<UTC stamp>/` with per-side evidence
(`ts/`, `rust/`), a `report.md` comparison table, and machine-readable
`findings.json`. Requires the TS binary `prime-agent` on PATH (ground truth)
and a built Rust binary (default `target/release/prime-agent`).

Files: `run_battery.py` (driver), `batterylib.py` (shared harness), 
`mock_provider.py` (deterministic mock provider), `perf.py` (f10 perf rows),
`framediff_first_run.py` (first-run frame diff),
`streaming_render.py` (live token-stream rendering verifier: pane captures
must grow progressively mid-turn over a paced faux provider, TS vs Rust
differential on the settled frame),
`scale_corpus.py` (f12 heavy-scale corpus generator).

Heavy flow (opt-in, not part of the default battery run):

    PA_BATTERY_HEAVY=1 python3 scripts/battery/run_battery.py --flows f12_scale_resume

`f12_scale_resume` generates a deterministic PA_BATTERY_HEAVY_TURNS-turn
session (default 5,000 turns = 15,261 transcript rows of user / assistant /
ipython-tool turns plus harness-digest rows) and measures interactive
`--resume` -> ready for both binaries through tmux. Gates: the Rust side
must reach ready within SCALE_RESUME_MAX_READY_S (30s) and within
SCALE_RESUME_MAX_RATIO (2.0x) of the TS side in the same run - a
regression gate for the snapshot replay/render path (per-row re-layout or
uncached preview work must not come back).
