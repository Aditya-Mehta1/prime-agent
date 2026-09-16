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
- PRs must state ownership compliance (crate README scope/non-goals/public API, dependency
  direction).

## References

- TS prime-agent at ~/prime-agent is the parity ground truth (read-only).
- OpenAI Codex (~/codex, Apache 2.0) is the design reference for agent internals; attribute ports.

- PRs are squash-merged: one commit per PR, subject `scope: summary (#N)` (`gh pr merge --squash`).
  Do not rewrite merged history - the repo is append-only.
