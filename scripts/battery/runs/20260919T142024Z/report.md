# Parity battery run 20260919T142024Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/model-selector/target/release/prime-agent
- flows: f17_slash_model

## Findings

0 gaps, 0 EXPECTED-FAIL (known gaps, owner lanes), 9 parity checks passed.


## Passed checks

- [f17_slash_model/visual] ts: /model opens the selector with the configured model listed
- [f17_slash_model/visual] ts: picking a model in the selector shows the 'Model: <id>' confirm row
- [f17_slash_model/visual] ts: /effort shows the thinking-level picker or its unsupported-model row
- [f17_slash_model/visual] rust: /model opens the selector with the configured model listed
- [f17_slash_model/visual] rust: picking a model in the selector shows the 'Model: <id>' confirm row
- [f17_slash_model/visual] rust: /effort shows the thinking-level picker or its unsupported-model row
- [f17_slash_model/visual] model-selector: frames identical TS vs Rust (normalized)
- [f17_slash_model/visual] model-selected: frames identical TS vs Rust (normalized)
- [f17_slash_model/visual] effort-picker: frames identical TS vs Rust (normalized)
