# Architecture

Prime Agent, Rust rewrite. TS reference: `~/prime-agent` (read-only ground truth for behavior).
Parity contract = user experience + model-facing surface, not internal mechanisms.

## Surface contract (must not change)

- Tools exposed to the model: `bash`, `edit`, `ipython` (internal helpers: `rename`, `stdout`).
- RLM kernel API in the persistent Python REPL: `rlm.spawn/find_models/collect/list_subagents/
  delete_subagent/create_session/progress_note`, `rlm.harness` CRUD,
  `agent_message.send`, `agent_observe`, `compact`, `goal`, `refine`, `attach_image`,
  skills (markdown + Python) per the skill contract in the base system prompt.
- System prompt structure: identity, tools, skills inventory, harness digest, goal continuation.
- CLI shape: `prime-agent` with the same commands/flags as the TS product; headless modes
  (RPC/daemon/session-worker) with identical behavior.

## Crates

| crate | role | TS origin |
|---|---|---|
| `pa-types` | shared wire & domain types, protocol messages | coding-agent core types, daemon protocol |
| `pa-ai` | providers, model registry, streaming | packages/ai |
| `pa-agent` | agent loop | packages/agent |
| `pa-core` | session engine: tools, skills, prompts, compaction, refinement, kernel/RLM manager, subagents, session manager, settings | packages/coding-agent core/ |
| `pa-daemon` | supervision redesign: supervisor + per-session worker processes, wire protocol, cloud sandbox attach | modes/daemon, session-worker |
| `pa-tui` | terminal UI (ratatui) | packages/tui + modes/interactive |
| `pa-cli` | binary `prime-agent` | coding-agent cli/main |

## Reliability redesign (mechanism changes, same UX)

- The daemon becomes a supervisor: it spawns one worker process per active session instead of
  hosting sessions in-process. Workers are supervised, restarted with backoff, and sessions
  persist on disk so reattach works even if the supervisor restarts.
- Session state is append-only JSONL on disk (same layout as `~/.prime/sessions`) so TUI
  reattach, checkpoint/resume, and external tooling keep working.

## Conventions

- No stubs, no `todo!()`, no swallowed errors (`anyhow` bubbling to UI is fine).
- Read the TS file in full before porting its behavior. The TS product on PATH is ground truth.
- Every crate: `cargo fmt`, `cargo clippy -D warnings`, `cargo test` green before merge.
- Verifiers: tmux-driven UX checks and differential tests against the installed TS binary.
