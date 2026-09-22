# Parity battery run 20260922T081549Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /tmp/prime-agent-stripped
- flows: f10_perf

## Findings

0 gaps, 0 EXPECTED-FAIL (known gaps, owner lanes), 5 parity checks passed.


## Passed checks

- [f10_perf/perf] ts: cold startup to interactive-ready median 1.100s over 3 launches (first frame median 1.076s); typing latency median 9.5ms, p95 21.9ms over 75 keystrokes
- [f10_perf/perf] rust: cold startup to interactive-ready median 0.147s over 3 launches (first frame median 0.056s); typing latency median 8.0ms, p95 13.5ms over 75 keystrokes
- [f10_perf/perf] startup: rust 0.147s vs ts 1.100s cold-ready median (ratio 0.13, threshold 1.5)
- [f10_perf/perf] typing: rust p95 13.5ms vs ts p95 21.9ms keystroke-to-render (ratio 0.62, threshold 1.5)
- [f10_perf/perf] rust binary measured: /tmp/prime-agent-stripped (38.6MB, release posture)
