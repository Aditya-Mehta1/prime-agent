# pa-daemon

Session supervision and wire serving.

## Scope
Supervisor process (one worker process per active session), restart/backoff
supervision, session registry/roster + worker self-registration (session
identity survives supervisor restarts: workers re-register with backoff and
the roster rebuilds), append-only session store ownership (including the
compaction-entry fold for compacted message reads), client attach/detach
(full-snapshot and chunked `session_snapshot_begin`/`chunk`/`end`
streaming), direct-attach transport (supervisor-issued single-use tickets
with a 10s TTL, worker-side peer grants burned on first use, session-plane
command gating on peer links), worker-to-worker peer messaging (stage 3:
`worker`-purpose single-use grants minted by `get_worker_peer_transport`
for a source worker's kernel `agent_message.send`, direct
`worker_deliver_message` on the target worker's socket with the supervisor
routed `send_message` as the never-retried fallback), wire protocol serve/negotiation (including the
`compact`/`abort_compaction`/`set_auto_compaction` commands and their
`compaction_start`/`compaction_end` events), cloud sandbox attach, session
leases (`core/session-lease.ts` port). Supervisor-backed RLM child sessions
(`rlm_children.rs`, the daemon side of the pa-core `RlmSubagentHost` seam):
`rlm.spawn`/`rlm.create_session` create real daemon sessions through the
worker's supervisor link - one supervised worker process per child - and the
parent-side registry serves `rlm.list_subagents`/`rlm.collect`/
`rlm.delete_subagent` with TS-parity selector errors; child model resolution
and thinking-level validation live in `rlm_child_model.rs`; the create
command carries the RLM recursion identity (`rlmDepth`/`rlmMaxDepth`/
`parentSessionPath`/`thinking`) so respawned children keep their depth. Per-session model binding: the
create-config `provider`/`model`/`apiKey` are authoritative for worker model
resolution (explicit CLI flags reach the worker; env remains the no-flag
fallback). Post-turn status-line requests (dashboard recap,
`daemon-session-summarizer.ts` port) issued by workers, with settled idle
verdicts persisted as `agent_status` session entries (real classifications
and transcript error verdicts only; respawns seed from the persisted
verdict). Worker session files carry the TS creation prefix
(`model_change`/`thinking_level_change`/`service_tier_change`), and queue
snapshots persist to the worker recovery journal, not the session file. Platform wall
(`platform`): per-OS endpoint naming and socket identity; the transport itself
is the shared trait in `pa_types::platform::transport`, and the private-frame
codec plus command planes are the shared wire contract in
`pa_types::daemon` (clients in pa-tui/pa-cli speak them).

## Non-goals
No agent behavior inside workers beyond hosting a pa-core engine; no UI.

## Public API
Supervisor entrypoint, worker entrypoint, client connection API for pa-tui/pa-cli. Supervision internals `pub(crate)`.

## Depends on
pa-types, pa-core (one-way).
