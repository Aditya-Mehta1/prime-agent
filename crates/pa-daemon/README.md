# pa-daemon

Session supervision and wire serving.

## Scope
Supervisor process (one worker process per active session), restart/backoff supervision, append-only session store ownership, client attach/detach, wire protocol serve/negotiation, cloud sandbox attach.

## Non-goals
No agent behavior inside workers beyond hosting a pa-core engine; no UI.

## Public API
Supervisor entrypoint, worker entrypoint, client connection API for pa-tui/pa-cli. Supervision internals `pub(crate)`.

## Depends on
pa-types, pa-core (one-way).
