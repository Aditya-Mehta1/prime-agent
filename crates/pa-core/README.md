# pa-core

The session engine.

## Scope
Tools (bash, edit, ipython + internal rename/stdout), file mutation queue, truncation and rendering rules, RLM kernel lifecycle (IPython spawn/execute/revive), skills loading, system prompt assembly, compaction, harness refinement, settings/config, session manager (persist/resume).

## Non-goals
No provider HTTP (pa-ai), no loop policy (pa-agent), no daemon supervision (pa-daemon), no TUI (pa-tui).

## Public API
`SessionEngine` (message in -> events out), `ToolRegistry`, kernel manager, settings. All subsystem internals `pub(crate)`; the engine is the only facade - subsystems must not import each other directly.

## Depends on
pa-types, pa-ai, pa-agent (one-way).
