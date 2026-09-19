# Parity battery run 20260919T151725Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /tmp/keybindings-rust4
- flows: f23_keybindings

## Findings

0 gaps, 0 EXPECTED-FAIL (known gaps, owner lanes), 18 parity checks passed.


## Passed checks

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
