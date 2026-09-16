# pa-core

The session engine.

## Scope
Tools (bash, edit, ipython + internal rename/stdout), file mutation queue, truncation and rendering rules, RLM kernel lifecycle (IPython spawn/execute/revive), skills loading, system prompt assembly, compaction, harness refinement, settings/config, package manager (npm/git/local source install/remove/list/update against settings, plus `resolve()`: precedence-ranked session resource resolution over configured packages, settings arrays, auto-discovery, and bundled skills), session manager (persist/resume).

## Non-goals
No provider HTTP (pa-ai), no loop policy (pa-agent), no daemon supervision (pa-daemon), no TUI (pa-tui). No Prime Agent self-updates (native release machinery) and no extension *runner* (loading/executing extension modules - see docs/parity-checklist.md section 2 for the spec boundary); the package manager installs sources and resolves resource paths only.

## Public API
`SessionEngine` (message in -> events out), `ToolRegistry`, kernel manager, settings (`SettingsManager`), packages (`packages::PackageManager` + source types). All subsystem internals `pub(crate)`; the engine is the only facade - subsystems must not import each other directly (the package manager consumes the public settings API only).

## Depends on
pa-types, pa-ai, pa-agent (one-way).
