# Parity battery run 20260919T040417Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/model-picker-2/target/release/prime-agent-mp2
- flows: f17_slash_model

## Findings

0 gaps, 1 EXPECTED-FAIL (known gaps, owner lanes), 8 parity checks passed.

## Known gaps (EXPECTED-FAIL — evidence for the owning fix lanes)

- [f17_slash_model/visual] EXPECTED-FAIL (lane: model-picker-2): model-selector: frames differ TS vs Rust (see frame-diff-model-selector.txt) — evidence: ts/f17_slash_model/frame-diff-model-selector.txt


## Passed checks

- [f17_slash_model/visual] ts: /model opens the selector with the configured model listed
- [f17_slash_model/visual] ts: picking a model in the selector shows the 'Model: <id>' confirm row
- [f17_slash_model/visual] ts: /effort shows the thinking-level picker or its unsupported-model row
- [f17_slash_model/visual] rust: /model opens the selector with the configured model listed
- [f17_slash_model/visual] rust: picking a model in the selector shows the 'Model: <id>' confirm row
- [f17_slash_model/visual] rust: /effort shows the thinking-level picker or its unsupported-model row
- [f17_slash_model/visual] model-selected: frames identical TS vs Rust (normalized)
- [f17_slash_model/visual] effort-picker: frames identical TS vs Rust (normalized)
