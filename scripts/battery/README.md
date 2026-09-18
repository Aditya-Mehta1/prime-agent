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
differential on the settled frame).
