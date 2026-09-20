# Parity battery run 20260920T024622Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/prime-agent-rs/target/release/prime-agent
- flows: f11_provider_failure, f14_compact, f22_provider_failover, f23_keybindings

## Findings

0 gaps, 2 EXPECTED-FAIL (known gaps, owner lanes), 33 parity checks passed.

## Known gaps (EXPECTED-FAIL — evidence for the owning fix lanes)

- [f14_compact/behavior] EXPECTED-FAIL (lane: compact-fb-2): rust: threshold crossing produced no visible auto-compaction outcome — evidence: rust/f14_compact/05-auto-after.txt
- [f14_compact/visual] EXPECTED-FAIL (lane: compact-fb-2): auto-compact: frames differ TS vs Rust (see frame-diff-auto-compact.txt) — evidence: ts/f14_compact/frame-diff-auto-compact.txt


## Passed checks

- [f11_provider_failure/behavior] ts: provider failure surfaces (retry banner + 4 error row(s))
- [f11_provider_failure/behavior] rust: provider failure surfaces (retry banner + 4 error row(s))
- [f11_provider_failure/visual] provider-failure rendering parity: 4 error row(s) on both sides
- [f14_compact/visual] ts: /compact shows the durable '◆ Context compacted' summary row
- [f14_compact/visual] ts: the Ctrl+O detail cycle expands the compaction summary block
- [f14_compact/visual] ts: the third Ctrl+O re-collapses the compaction summary block
- [f14_compact/behavior] ts: crossing the compaction threshold auto-compacts and shows the summary row
- [f14_compact/visual] rust: /compact shows the durable '◆ Context compacted' summary row
- [f14_compact/visual] rust: the Ctrl+O detail cycle expands the compaction summary block
- [f14_compact/visual] rust: the third Ctrl+O re-collapses the compaction summary block
- [f14_compact/visual] manual-compact: frames identical TS vs Rust (normalized)
- [f14_compact/visual] manual-expanded: frames identical TS vs Rust (normalized)
- [f22_provider_failover/behavior] ts: the TS product has no provider failover — the same flow exhausts its quick retries and surfaces the failure (the resilience feature is Rust-side only; intentional divergence)
- [f22_provider_failover/behavior] rust: provider failure re-routed to prime-backup/mock-1, the backup answered, and the primary was restored (switch surface rendered: False)
- [f22_provider_failover/visual] rust: the provider-switch loader row was not captured (it is transient; the switch itself settled: primary restored: True, backup answered: True)
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
