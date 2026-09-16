# pa-cli

The `prime-agent` binary.

## Scope
Argument parsing matching the TS CLI exactly, mode selection (interactive/headless/json/daemon subcommands), binary wiring of crates into processes, exit codes and user-facing errors.

## Non-goals
No business logic; it composes pa-daemon, pa-core, pa-tui, pa-ai via their public APIs only.

## Public API
The `prime-agent` executable surface (flags, subcommands, exit codes). Internal lib types `pub(crate)`.

## Depends on
pa-types, pa-ai, pa-core, pa-daemon, pa-tui (one-way, composition root).
