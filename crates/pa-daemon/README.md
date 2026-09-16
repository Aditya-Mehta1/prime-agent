# pa-daemon

Session supervision and wire serving.

## Scope
Supervisor process (one worker process per active session), restart/backoff
supervision, session registry/roster + worker self-registration (session
identity survives supervisor restarts: workers re-register with backoff and
the roster rebuilds), append-only session store ownership, client
attach/detach (full-snapshot and chunked `session_snapshot_begin`/`chunk`/`end`
streaming), wire protocol serve/negotiation, cloud sandbox attach. Platform
wall (`platform`): per-OS endpoint naming and socket identity; the transport
itself is the shared trait in `pa_types::platform::transport`.

## Non-goals
No agent behavior inside workers beyond hosting a pa-core engine; no UI.

## Public API
Supervisor entrypoint, worker entrypoint, client connection API for pa-tui/pa-cli. Supervision internals `pub(crate)`.

## Depends on
pa-types, pa-core (one-way).
