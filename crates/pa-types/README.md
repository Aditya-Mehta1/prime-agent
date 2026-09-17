# pa-types

The single shared vocabulary crate. Nothing else is shared between crates.

## Scope
Wire and domain types + serde only: AI messages/content blocks/tool calls/usage/stream events, session JSONL entry schema, daemon wire protocol messages, worker frames.

Daemon wire mechanics shared by the serving side (pa-daemon) and clients (pa-tui/pa-cli) live here because pa-tui depends on pa-types alone:
- `daemon::framing`: the private-frame codec of the worker socket (direct-attach clients speak it too).
- `daemon::plane`: the session/control command-plane table (worker-side peer gating and client-side socket routing both read it).
- `daemon::{DaemonPeerTransportTicket, DaemonWorkerPeerGrant, DaemonPeerCommand}`: the direct-transport ticket and grant wire shapes.
- `slash_commands`: the builtin slash-command table every surface shares
  (the TUI dispatch + autocomplete, the session engine's command admission,
  CLI suggestion help) plus its pure parse/suggestion helpers — the TS
  product keeps the same single table in core and imports it from its TUI.
- `extension_rpc`: the private, versioned NDJSON-over-stdio protocol between the pa-core extension host and the Node sidecar (handshake, RPC envelopes, registration payloads, `ExtensionError`). Both ends ship in the same release, so these types are strict (no catch-alls).

Platform contracts (`platform`): the cross-crate transport, process-identity, and socket-identity helpers. pa-types is the only crate every transport consumer can depend on (pa-tui depends on pa-types alone), so the shared trait vocabulary and its cfg-gated Unix implementations live here. Windows support later means implementing these traits, not re-plumbing callers.

## Non-goals
No provider logic, no session logic, no UI. Beyond pure data helpers and the platform contracts, no behavior: a domain type that wants a method belongs in the owning crate.

## Public API
Everything in this crate is deliberately `pub` - it is the cross-crate contract. Unknown fields survive round-trips via catch-all maps so schema revisions stay compatible.

## Depends on
serde, serde_json, thiserror, anyhow, tokio. No workspace crates.
