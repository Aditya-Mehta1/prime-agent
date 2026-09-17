# pa-core

The session engine.

## Scope
MCP host side (`mcp.rs`): auth gating for built-in integrations (the
disabled-skill overrides and `/mcp list` status), the `mcpServers` settings
seam, and the `mcp.*` host-request handlers the kernel's generic MCP
client reaches through (`mcp.config`, `mcp.refresh`, optional
`mcp.begin_login`). The MCP protocol itself runs kernel-side (Python
stdio/HTTP); the engine never spawns MCP servers. Tools (bash, edit, ipython + internal rename/stdout), file mutation queue, truncation and rendering rules, RLM kernel lifecycle (IPython spawn/execute/revive), skills loading, system prompt assembly, compaction, harness refinement, settings/config, package manager (npm/git/local source install/remove/list/update against settings), session manager (persist/resume). Autonomous mode
(`autonomous`): runtime state with limit normalization, continuation and
gate-failure texts, shell quality gates with retry windows and git
worktree snapshotting, and the `AutonomousDriver` policy trait a turn
loop consults after every settled turn (the engine holds no autonomous
logic of its own). The RLM recursion host seam
(`session_engine::rlm_host`): the trait the kernel's `rlm.spawn`/
`rlm.create_session`/`rlm.list_subagents`/`rlm.collect`/
`rlm.delete_subagent` host requests call into, with the roster/collect/
selector-error vocabulary the daemon implements over the supervisor link.
Platform wall (`platform`): process control (signals/process groups), file locking, file permissions, and shell selection - every OS-specific behavior in the engine lives there behind cfg-gated implementations. Tools (bash, edit, ipython + internal rename/stdout), file mutation queue, truncation and rendering rules, RLM kernel lifecycle (IPython spawn/execute/revive), skills loading, system prompt assembly, compaction, harness refinement, settings/config, package manager (npm/git/local source install/remove/list/update against settings, plus `resolve()`: precedence-ranked session resource resolution over configured packages, settings arrays, auto-discovery, and bundled skills), session manager (persist/resume). Extension host (stage 1: Node sidecar process lifecycle - spawn/handshake/ping/orderly shutdown, NDJSON RPC client + framing, host script materialized content-addressed under <agentDir>/extension-host/; stage 2: module loading with vendored jiti 2.7.0 + pi API/import shims in the sidecar, registration landing in the registry mirror (first-wins tools, command collision suffixing, flag/shortcut rules), the `ExtensionRunner` facade, extension tools bridged into the loop tool surface over `tool_execute` RPC, session-engine assembly with prompt-guideline injection; docs/extensions-runner-design.md). origin/main

## Non-goals
No provider HTTP (pa-ai), no loop policy (pa-agent), no daemon supervision (pa-daemon), no TUI (pa-tui). No Prime Agent self-updates (native release machinery). Event emission at the session seams, ctx-action binding, slash-command dispatch at the product surface, and reload/stale-ctx semantics are design stages 3+ (docs/extensions-runner-design.md); the package manager installs sources and resolves resource paths only.

## Public API
`SessionEngine` (message in -> events out), `ToolRegistry`, kernel manager, settings (`SettingsManager`), packages (`packages::PackageManager` + source types), extensions (`extensions::ExtensionRunner` + the sidecar RPC client + the registration registry), `platform` (process control, file locking, permissions, shell selection - usable by higher crates, e.g. pa-cli's detached spawn), `session_engine::rlm_host::RlmSubagentHost` (implemented by pa-daemon for supervisor-backed children; the default is no children), `mcp::McpManager` (the session's host-side MCP manager, exposed as `SessionEngine.mcp_manager`: auth gating for built-in integrations, the `mcpServers` setting seam, and the `mcp.*` host-request handlers - `mcp.config`/`mcp.refresh`/optional `mcp.begin_login` - the kernel's generic MCP client resolves through). All subsystem internals `pub(crate)`; the engine is the only facade - subsystems must not import each other directly (the package manager consumes the public settings API only).

## Depends on
pa-types, pa-ai, pa-agent (one-way).
