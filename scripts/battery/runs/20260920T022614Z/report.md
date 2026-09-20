# Parity battery run 20260920T022614Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/prime-agent-rs/target/release/prime-agent
- flows: f11_provider_failure, f12_scroll

## Findings

0 gaps, 0 EXPECTED-FAIL (known gaps, owner lanes), 5 parity checks passed.


## Passed checks

- [f11_provider_failure/behavior] ts: provider failure surfaces (retry banner + 4 error row(s))
- [f11_provider_failure/behavior] rust: provider failure surfaces (retry banner + 4 error row(s))
- [f11_provider_failure/visual] provider-failure rendering parity: 4 error row(s) on both sides
- [f12_scroll/behavior] ts: PageUp pages history into view with the follow hint; paging back to the tail resumes following
- [f12_scroll/behavior] rust: PageUp pages history into view with the follow hint; paging back to the tail resumes following
