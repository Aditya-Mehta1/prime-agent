# pa-types

The single shared vocabulary crate. Nothing else is shared between crates.

## Scope
Wire and domain types + serde only: AI messages/content blocks/tool calls/usage/stream events, session JSONL entry schema, daemon wire protocol messages, worker frames.

Platform contracts (`platform`): the cross-crate transport and process-identity traits. pa-types is the only crate every transport consumer can depend on (pa-tui depends on pa-types alone), so the shared trait vocabulary and its cfg-gated Unix implementations live here. Windows support later means implementing these traits, not re-plumbing callers.

## Non-goals
No provider logic, no session logic, no UI. Beyond pure data helpers and the platform contracts, no behavior: a domain type that wants a method belongs in the owning crate.

## Public API
Everything in this crate is deliberately `pub` - it is the cross-crate contract. Unknown fields survive round-trips via catch-all maps so schema revisions stay compatible.

## Depends on
serde, serde_json, thiserror, anyhow, tokio. No workspace crates.
