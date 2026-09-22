# Parity battery run 20260921T192318Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /tmp/prompt-stash-rust/prime-agent
- flows: f24_prompt_stash

## Findings

0 gaps, 1 EXPECTED-FAIL (known gaps, owner lanes), 23 parity checks passed.

## Known gaps (EXPECTED-FAIL — evidence for the owning fix lanes)

- [f24_prompt_stash/visual] EXPECTED-FAIL (lane: prompt-stash): agents-view-back: frames differ TS vs Rust (see frame-diff-agents-view-back.txt) — evidence: ts/f24_prompt_stash/frame-diff-agents-view-back.txt


## Passed checks

- [f24_prompt_stash/behavior] ts: the reopened chat restored the stashed draft into the editor
- [f24_prompt_stash/behavior] rust: the reopened chat restored the stashed draft into the editor
- [f24_prompt_stash/visual] typed-draft: frames identical TS vs Rust (normalized)
- [f24_prompt_stash/visual] restored: frames identical TS vs Rust (normalized)
- [f24_prompt_stash/visual] submitted: frames identical TS vs Rust (normalized)
- [f23_keybindings/visual] ts: the prompt-context hint renders the user override (X), not the default
- [f23_keybindings/visual] ts: the override key cycled the conversation detail
- [f23_keybindings/visual] ts: the removed default key no longer cycles the detail
- [f23_keybindings/visual] ts: /hotkeys documents the effective override (X row)
- [f23_keybindings/visual] ts: the ? quick-shortcut guide mounted with the effective bindings
- [f23_keybindings/visual] ts: the submission cleared the quick-shortcut guide
- [f23_keybindings/visual] rust: the prompt-context hint renders the user override (X), not the default
- [f23_keybindings/visual] rust: the override key cycled the conversation detail
- [f23_keybindings/visual] rust: the removed default key no longer cycles the detail
- [f23_keybindings/visual] rust: /hotkeys documents the effective override (X row)
- [f23_keybindings/visual] rust: the ? quick-shortcut guide mounted with the effective bindings
- [f23_keybindings/visual] rust: the submission cleared the quick-shortcut guide
- [f23_keybindings/visual] detail-hint: frames identical TS vs Rust (normalized)
- [f23_keybindings/visual] override-fired: frames identical TS vs Rust (normalized)
- [f23_keybindings/visual] default-key: frames identical TS vs Rust (normalized)
- [f23_keybindings/visual] hotkeys-guide: frames identical TS vs Rust (normalized)
- [f23_keybindings/visual] shortcut-guide: frames identical TS vs Rust (normalized)
- [f23_keybindings/visual] guide-cleared: frames identical TS vs Rust (normalized)
