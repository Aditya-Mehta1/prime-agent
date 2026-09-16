Recreate the prime-agent project in Rust, from scratch. The end state: a Prime Agent that looks and feels the exact same as today and has every current feature, except it is faster, easier to build on, and far more reliable.

The TypeScript implementation to study is the private repo PrimeIntellect-ai/prime-agent — cloned at ~/prime-agent in the dev sandbox (~/pi/prime-agent on Kevin's Mac). Scan it end to end and understand how it is implemented before writing any code — read it to learn the behavior, not to copy the design.

## Why we're rebuilding

The product behavior is right; the implementation is broken. Over the last four months it has grown to ~199K lines of product TypeScript plus another ~188K lines of tests, and the architecture itself is now the problem:

- agent-session.ts is a 12.5K-line god class (AgentSession, 59 imports) that owns session queueing, tool dispatch, compaction, harness refinement, subagents, checkpoint/resume, kernel lifecycle, and provider wiring all at once. Every feature change cuts through it, so nothing can be refactored safely.
- The mode layer is no better: interactive-mode.ts (10K lines), daemon-mode.ts (7.8K), daemon-supervisor.ts (7K) — each a single class doing everything.
- Reliability is a recurring failure mode: the daemon gets overloaded and dies, the Python kernels crash, and so on — precisely in the session-infra paths users depend on.
- The code is verbose and not pretty: copy-pasted defensive logic, giant handlers, poor separation of concerns. Slow to read, slow to change, easy to break.
- The tests are half the codebase (~188K lines, single test files up to 10K lines) and most of them are useless — huge, brittle, low-value.

The only thing worth keeping is the behavior contract: a good inference harness for the RLM paradigm, many providers, a reliable agent loop, a polished TUI, thousands of users. Preserve that contract — not that code.

## Requirements

- Full feature parity: the exact same product and eval experience — TUI, verifiers flow, end-user experience, and the model-facing surface RLM-1 is post-trained on (tool names, rlm recursion API, skill contract, system-prompt structure).
- Much faster, far more reliable, and much lighter: a fraction of the ~200K product LOC, and tests that actually catch regressions instead of the current low-value volume.
- Clean, modular design with hard ownership boundaries, since we now know what the final state looks like: TUI, provider APIs, agent loop, RLM primitives (persistent IPython-kernel state, recursive subagents, skills), and coding-agent features fully decoupled, so each layer can be extended independently in the future. Enforce it structurally:
  - Every crate owns exactly one area, declared in its own README (scope, explicit non-goals, and its public API). No shared kitchen-sink crates: one crate holds the cross-cutting type vocabulary and nothing else.
  - Public API at crate boundaries stays minimal — `pub(crate)` inside; no re-exporting internals across crates. A crate may never reach into another crate's internals.
  - The dependency graph is layered and cycle-free; document the direction in the workspace README and ARCHITECTURE.md, and keep it true in Cargo.toml.
  - If a change forces edits across many crates' internals, the boundary is wrong — fix the boundary, not the symptom. This is the anti-agent-session.ts rule: no file, module, or crate may become the place everything flows through.
  - Parity is behavioral, not structural. Port behaviors, wire formats, and edge cases from the TS reference — but organize the Rust code Rust-first: modules and files are designed by Rust concerns, not by mirroring the TS file tree, and prefer idiomatic traits, enums, and data modeling over transcriptions of TS classes. Do not write "ported from <TS path>" framing in code comments — document what the code IS (role, invariants, contract); provenance belongs in PORTING-NOTES, not the source. Where a 1:1 port produced awkward structure, refactor under the module-cap and ownership rules — the differential verifiers are the safety net that lets structure change while behavior stays locked.
- The interactive agent view is the most important surface — the TUI session experience is what users live in, so build and polish it first and hardest. But every capability must also run headlessly with identical behavior, so the same product serves evals, sandboxes, and embedding inside other applications.
- Config-defined models: users must be able to declare model entries (id, provider, endpoint, pricing, context window) in their config, and those entries survive catalog refreshes — custom or internal endpoints must not depend on API listing (the current product loses such models whenever its catalog cache is overwritten by the network-scoped /models response).
- The end deliverable is an OS-agnostic compiled Prime Agent, without the bloat and fluff.

Parity means user experience, not implementation internals. You may — and should — rethink mechanisms where the current design is broken, provided the user-visible behavior is preserved or improved. The daemon is the model case:

- Today one daemon process gets overloaded and dies under load. Rethink how it works and how much it handles — supervision model, process boundaries, restartability. Reliability here is a first-class goal, not an afterthought.
- It should also be able to connect to prime-agent sessions running in Prime sandboxes in the cloud, not just local sessions.
- But do not take away the core property: sessions keep running when you close the TUI, and you can reattach later. That persistence is the feature; the mechanism is yours to redesign.
- Apply the same standard everywhere else: keep the user-visible contract, rethink the mechanism when the mechanism is what's broken.

## Process

- GitHub continuously: commit and push in small increments so progress is never lost. ALL changes go through PRs — branch per unit of work, run checks, self-merge with **squash merges** (one commit per PR, subject like `scope: summary (#N)`); no direct commits to main. Keep main always green.
- Parallelize with subagents: after the initial scan, decompose into independent lanes (e.g. crates: TUI, providers, agent loop, RLM/kernel, coding-agent features, CLI), spawn one subagent per lane with its own branch/worktree, and integrate through PRs. Verify each subagent's work before merging; don't redo lanes inline. Parallelize everything that can be parallelized.
- Work responsibly, not lazily: no stubs, no placeholder implementations, no todo!()/unimplemented!(), no swallowed errors. Read the TypeScript before porting a behavior and match it — don't guess. Nothing is done until it is verified against real product behavior, not just the compiler.

## Verification

Never judge your own work by self-assessment — always check against an objective metric. Two high-signal verifiers to use constantly:

- User-level tmux testing. When testing interactive behavior, drive the product like a user: spin up tmux, launch the built binary, send real keys, capture panes, and judge exactly what the screen shows. The TS repo's AGENTS.md documents this protocol for the original — reuse it for the rewrite.
- Differential testing against the original prime-agent. The TS product is installed on this box (`prime-agent` on PATH). When any behavior is ambiguous, run the identical flow in both the original and the Rust build — same input, same settings — and compare. The OG product is the ground truth for parity.

Before a subagent starts a lane, give it a verifier: a concrete objective check (tmux-driven, differential against the original, or a real integration test) that says unambiguously whether the lane is done. No lane merges without passing its verifier.

## Hosting

Build it in a private repo under kevinjosethomas. Do not share it with anyone at the company until I'm happy with the implementation.

## Resources

Take as much time as you need, and use as many tokens as you need — we're self-hosting. But you and every subagent may only use the prime-inference internal GLM-5.3 fast endpoint. The final output should be as clean, reliable, and modular as possible.

## Reference

Before writing Rust, read Jarred Sumner's "Rewriting Bun in Rust" (https://bun.com/blog/bun-in-rust) and take what applies — the porting guide, parity-through-testing, and dozens of agents working in parallel across git worktrees (same workflow as the process above).

Second reference for agent-specific implementation details: OpenAI Codex (https://github.com/openai/codex) — a mature Rust agent harness, cloned at ~/codex on the dev box. It has accurate implementations of the internals that are easy to get wrong: token counting, maintaining the KV-cacheable prefix across turns, streaming provider responses, context/compaction management, session persistence, and tool execution. When implementing these, cross-check your approach against codex's code (and against the TS reference, which stays the parity ground truth — remember cache-prefix stability is a first-class concern; never adopt a pattern without checking what it does to the cacheable prefix). Codex is Apache 2.0: study freely; if you port specific code, attribute it.

Also take infrastructural discipline from codex — its AGENTS.md (~/codex/AGENTS.md) is the template for repo governance. Adapt these into the new repo's own AGENTS.md and enforce them in lane prompts and PR review:
- Module-size caps — the concrete anti-god-module rule: target modules under 500 LoC (excluding tests); past ~800 LoC, new functionality goes in a new module unless documented otherwise; applies hardest to high-touch orchestration files (the agent-session.ts class of file). When extracting, move the related tests and docs with the code.
- Lint discipline as a merge gate: `cargo fmt --check` and `clippy -D warnings` clean before every merge. Clippy style rules: inline format args, collapse ifs, method references over closures, exhaustive matches without wildcard arms.
- API shape: no opaque bool/Option positional params — enums, named methods, newtypes, or `/*param_name*/` comments when unavoidable. New traits get doc comments explaining their role. Prefer native RPITIT trait methods with explicit `Send` bounds over `async_trait`/`#[allow(async_fn_in_trait)]`.
- Privacy: private modules with an explicitly exported public crate API (this is the ownership rule made mechanical).
- Test discipline: compare whole objects, not field-by-field; no tests for statically-defined values; no negative tests for removed logic.
- Change hygiene: dependency edits include their lockfile update in the same change; no single-use tiny helper methods; `#[tracing::instrument]` on definitions rather than `.instrument()` at call sites.
Skip codex-specific machinery (Bazel, just, sandbox env vars, CODEX_ internals) — adopt the discipline, not their build system.

## Environment

Dev work happens on the dev box (ubuntu@195.242.10.125 — 4 vCPU, 15 GB, 485 GB; Rust toolchain, gh and prime CLIs authenticated). The Rust code lives in ~/prime-agent-rs (this repo). Agent state (~/.prime: sessions, subagents, skills, memories) syncs to kevinjosethomas/prime-agent-state every 15 minutes — never commit credentials. The TS reference is at ~/prime-agent (read-only; do not modify).
