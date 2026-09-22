# AGENTS.md

Development rules for prime-agent-rs. Adapted from the codex-rs and prime-agent (TS) repo rules.
Every contributor (human or agent) must read this before working on this repo.

## Style and structure

- Workspace crates are prefixed `pa-`. See ARCHITECTURE.md for the hard ownership rules: one owned
  area per crate, pa-types is the only shared crate, cycle-free dependency direction,
  minimal public APIs, no god-modules.
- Prefer private modules with an explicitly exported public crate API. Internals are `pub(crate)`.
- Avoid large modules. Target Rust modules under 500 LoC excluding tests. Past ~800 LoC, put new
  functionality in a new module unless there is a strong documented reason not to. Be hardest on
  high-touch orchestration files (session engine, daemon supervisor, TUI app): those attract
  unrelated changes, so split early.
- When extracting code from a large module, move the related tests and docs with it so invariants
  stay close to the owning code.
- Inline format args: always prefer `format!("{x}")` over positional.
- Collapse if statements per clippy::collapsible_if.
- Prefer method references over closures per clippy::redundant_closure_for_method_calls.
- Make `match` statements exhaustive; avoid wildcard arms.
- New traits need doc comments explaining their role and how implementations are expected to behave.
- No opaque positional `bool`/`Option` parameters (`foo(false)` is unreadable). Prefer enums,
  named methods, or newtypes. If you must pass an opaque literal by position, use an exact
  `/*param_name*/` comment matching the callee signature.
- Prefer native RPITIT trait methods with explicit `Send` bounds
  (`fn foo(&self) -> impl Future<Output = T> + Send;`) over `#[async_trait]` or
  `#[allow(async_fn_in_trait)]`. Implementations may use `async fn` when they satisfy the contract.
- No single-use helper methods. Do not create a helper referenced only once.
- Instrument async work at the definition (`#[tracing::instrument(...)]`), not with
  `.instrument(...)` at call sites. Check whether the callee is already instrumented first.

## Change hygiene

- If you change dependencies (`Cargo.toml`), regenerate/commit `Cargo.lock` in the same change.
- If a change starts forcing edits across many crate internals, stop and fix the boundary instead.
- Cache-prefix stability is first-class: never adopt a pattern without checking its effect on the
  cacheable prompt prefix (see MISSION.md; cross-check against ~/codex).

## Tests

- Prefer whole-object equality comparisons over field-by-field checks.
- Do not add tests for statically defined values.
- Do not add negative tests for logic that was removed.
- Verifiers over self-assessment: tmux user-level tests, differential tests against the TS binary
  on PATH, golden corpora replayed against real captured data. No lane merges without its verifier
  passing, rerun by the reviewer where feasible.

## Merge gates

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace` must pass before every merge. Run `make check` (same gates; the GitHub
  token lacks `workflow` scope, so CI is local/PR-review enforced until then).
- **Parity-diff evidence is a merge gate** (the port's definition, not optional polish): every PR
  that touches a user-visible surface must include a "parity-diff evidence" section in its
  description showing the TS-binary comparison for what it changed: (1) rendered output —
  frame-diff vs the TS binary (extend `scripts/visual_parity.py` or the specific harness); 
  (2) interactive behavior — the same input handled identically (keys, mouse, timing); 
  (3) wire parity — byte-compare the TS daemon's traffic for protocol changes; (4) user-visible
  invariants — every user action produces the same visible reaction as TS (`/compact` shows
  started+completed; `/model` shows the selector; a refinement shows its decoration). A feature
  that "works" but was never diffed against the TS binary does not pass review. If TS shows it,
  Rust shows it identically; if Rust shows something TS does not, that is also a parity bug.
- PRs must state ownership compliance (crate README scope/non-goals/public API, dependency
  direction).

## Adoption telemetry

Every user-visible feature ships its adoption telemetry event in the same PR as the feature:
the event name + properties are added to `docs/telemetry-events.md` (schema versioned), and a
seam emits it from day one. Telemetry properties never carry prompt, session, or file content
(primitives only; see `pa-telemetry` and the privacy contract in `docs/telemetry-design.md`).

## Branding

The product is Prime Agent - we are not a pi fork. Scrub "pi"/"pi-mono"/"pi-ai"/
"Prime Intellect"-style naming from all user-visible surfaces (docs, READMEs,
CLI help text, error messages, splash/onboarding strings, keybinding hints, TUI
labels, and code comments that quote user-facing strings); brand everything
Prime Agent. Audit with a repo-wide grep and classify every hit (user-visible
vs wire-internal vs comment) before scrubbing, and list the preserved wire
identifiers in the PR body so the reviewer can verify none were wrongly scrubbed.

EXPLICIT EXCEPTION: wire-protocol identifiers that must stay byte-compatible with the TS product (e.g. the PI_PACKAGE_DIR env var, settings keys, provider IDs like prime-inference, harness _meta namespaces like ai.primeintellect.prime-agent, lockfile names) stay until/unless the TS side renames them — PARITY BEATS BRANDING ON THE WIRE.

## References

- TS prime-agent at ~/prime-agent is the parity ground truth (read-only).
- OpenAI Codex (~/codex, Apache 2.0) is the design reference for agent internals; attribute ports.

- PRs are squash-merged: one commit per PR, subject `scope: summary (#N)` (`gh pr merge --squash`).
  Do not rewrite merged history - the repo is append-only.
