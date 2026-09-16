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
- Clean, modular design, since we now know what the final state looks like: TUI, provider APIs, agent loop, RLM primitives (persistent IPython-kernel state, recursive subagents, skills), and coding-agent features fully decoupled, so each layer can be extended independently in the future.
- The interactive agent view is the most important surface — the TUI session experience is what users live in, so build and polish it first and hardest. But every capability must also run headlessly with identical behavior, so the same product serves evals, sandboxes, and embedding inside other applications.
- The end deliverable is an OS-agnostic compiled Prime Agent, without the bloat and fluff.

Parity means user experience, not implementation internals. You may — and should — rethink mechanisms where the current design is broken, provided the user-visible behavior is preserved or improved. The daemon is the model case:

- Today one daemon process gets overloaded and dies under load. Rethink how it works and how much it handles — supervision model, process boundaries, restartability. Reliability here is a first-class goal, not an afterthought.
- It should also be able to connect to prime-agent sessions running in Prime sandboxes in the cloud, not just local sessions.
- But do not take away the core property: sessions keep running when you close the TUI, and you can reattach later. That persistence is the feature; the mechanism is yours to redesign.
- Apply the same standard everywhere else: keep the user-visible contract, rethink the mechanism when the mechanism is what's broken.

## Process

- GitHub continuously: commit and push in small increments so progress is never lost. Use PRs wherever possible — branch per unit of work, run checks, self-merge (direct commits only for trivial fixes). Keep main always green.
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

## Environment

Dev work happens on the dev box (ubuntu@195.242.10.125 — 4 vCPU, 15 GB, 485 GB; Rust toolchain, gh and prime CLIs authenticated). The Rust code lives in ~/prime-agent-rs (this repo). Agent state (~/.prime: sessions, subagents, skills, memories) syncs to kevinjosethomas/prime-agent-state every 15 minutes — never commit credentials. The TS reference is at ~/prime-agent (read-only; do not modify).
