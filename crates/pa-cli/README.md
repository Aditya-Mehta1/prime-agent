# pa-cli

The `prime-agent` binary.

## Scope
Argument parsing matching the TS CLI exactly, mode selection (interactive/headless/json/daemon subcommands), binary wiring of crates into processes, exit codes and user-facing errors. Client-side MCP auth flows (`mcp_login.rs`): the `/mcp login`/`/mcp logout` hook the TUI calls — the TS interactive client's placement — resolving a server (settings `mcpServers` + the builtin catalog), running the pa-core OAuth login on the suspended terminal, and persisting through the shared auth store.

## Non-goals
No business logic; it composes pa-daemon, pa-core, pa-tui, pa-ai via their public APIs only.

## Public API
The `prime-agent` executable surface (flags, subcommands, exit codes). Internal lib types `pub(crate)`, plus the daemon wiring (`interactive_mode::{ensure_daemon_running, ensure_daemon_running_with, resolve_socket_path}`) exported for the integration verifier.

## Depends on
pa-types, pa-ai, pa-agent, pa-core, pa-daemon, pa-tui (one-way, composition root).

## Daemon client
The daemon-backed public commands (`list`, `stop`, `rename`, `send`, `schedule`) talk to the
pa-daemon supervisor over its JSONL Unix socket through the crate-private client module
(`daemon_client.rs`). The client never spawns a daemon - the TS CLI only auto-starts one for
the internal `daemon start`/`open` commands, which are not reachable from the public surface.

## Packaged layout / kernel packaging

The release artifact is exe-adjacent (TS install.sh native layout): the binary
plus `package.json` (the version manifest `--version` reads at runtime, with
the compiled-in version as the fallback), `prime-agent-runtime/` (the kernel
sidecar), `skills/`, and `docs/`. `scripts/package_release.py` (`make package`)
assembles, validates, version-pins, hashes, and tars it; the hidden
`--prime-agent-bootstrap` flag is the installer handoff that pre-bootstraps the
kernel venv. Verifier: `tests/packaged_layout_e2e.rs`.

## Daemon discovery
The discovery commands (`status`, `doctor [--fix]`, `shutdown [--force]`, TS
`cli/daemon-ps.ts`) live in the crate-private `daemon_discovery` module: an OS census of
listening unix sockets owned by product processes, a socket-dir sweep, probing/classifying
each discovered daemon, and the reap/shutdown planners and executors. Containment is part of
the contract: every scan, probe, and stop is scoped to an explicit `DaemonStateRoot`
(the env-resolved current root for the CLI), with a hard never-touch exclusion list for this
sandbox's ambient mission daemons - see docs/PORTING-NOTES.md. All e2e daemons live in
test-created fixture directories only.
