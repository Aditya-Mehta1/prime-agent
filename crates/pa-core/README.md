# pa-core

The session engine.

## Scope
Tools (bash, edit, ipython + internal rename/stdout), file mutation queue, truncation and rendering rules, RLM kernel lifecycle (IPython spawn/execute/revive), skills loading, system prompt assembly, compaction, harness refinement, settings/config, package manager (npm/git/local source install/remove/list/update against settings), session manager (persist/resume). Autonomous mode
(`autonomous`): runtime state with limit normalization, continuation and
gate-failure texts, shell quality gates with retry windows and git
worktree snapshotting, and the `AutonomousDriver` policy trait a turn
loop consults after every settled turn (the engine holds no autonomous
logic of its own). The RLM recursion host seam
(`session_engine::rlm_host`): the trait the kernel's `rlm.spawn`/
`rlm.create_session`/`rlm.list_subagents`/`rlm.collect`/
`rlm.delete_subagent` host requests call into, with the roster/collect/
selector-error vocabulary the daemon implements over the supervisor link.
Platform wall (`platform`): process control (signals/process groups), file locking, file permissions, and shell selection - every OS-specific behavior in the engine lives there behind cfg-gated implementations. Tools (bash, edit, ipython + internal rename/stdout), file mutation queue, truncation and rendering rules, RLM kernel lifecycle (IPython spawn/execute/revive), skills loading, system prompt assembly, compaction, harness refinement, settings/config, package manager (npm/git/local source install/remove/list/update against settings, plus `resolve()`: precedence-ranked session resource resolution over configured packages, settings arrays, auto-discovery, and bundled skills), session manager (persist/resume). Extension host (stage 1: Node sidecar process lifecycle - spawn/handshake/ping/orderly shutdown, NDJSON RPC client + framing, bundled protocol-peer host script materialized under <agentDir>/extension-host/; docs/extensions-runner-design.md). origin/main

## Non-goals
No provider HTTP (pa-ai), no loop policy (pa-agent), no daemon supervision (pa-daemon), no TUI (pa-tui). No Prime Agent self-updates (native release machinery). Extension *loading* (vendored jiti + pi API shim in the sidecar), registration landing, and tool execution over RPC are design stages 2+ (docs/extensions-runner-design.md); the package manager installs sources and resolves resource paths only.

## Public API
`SessionEngine` (message in -> events out), `ToolRegistry`, kernel manager, settings (`SettingsManager`), packages (`packages::PackageManager` + source types), extensions (`extensions::ExtensionHost` + the sidecar RPC client), `platform` (process control, file locking, permissions, shell selection - usable by higher crates, e.g. pa-cli's detached spawn), `session_engine::rlm_host::RlmSubagentHost` (implemented by pa-daemon for supervisor-backed children; the default is no children). All subsystem internals `pub(crate)`; the engine is the only facade - subsystems must not import each other directly (the package manager consumes the public settings API only).

## Depends on
pa-types, pa-ai, pa-agent (one-way).
