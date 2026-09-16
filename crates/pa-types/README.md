# pa-types

The single shared vocabulary crate. Nothing else is shared between crates.

## Scope
Wire and domain types + serde only: AI messages/content blocks/tool calls/usage/stream events, session JSONL entry schema, daemon wire protocol messages, worker frames.

## Non-goals
No behavior, no I/O, no provider logic, no session logic, no UI. If a type wants a method beyond pure data helpers, it belongs in the owning crate.

## Public API
Everything in this crate is deliberately `pub` - it is the cross-crate contract. Unknown fields survive round-trips via catch-all maps so schema revisions stay compatible.

## Depends on
serde, serde_json, thiserror. No workspace crates.
