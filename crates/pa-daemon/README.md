# pa-daemon

Session supervision and wire serving.

## Scope
ACP stdio transport (`acp`): the JSON-RPC serve surface for Agent
Client Protocol clients - a thin transport over the pa-core session engine
(initialize, session/new, session/prompt, session/close, session/cancel,
and the outgoing session/update notification with namespaced `_meta`
correlation), owned by this crate because wire-protocol serving is its area.
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
`compaction_start`/`compaction_end` events), the agent-roster arms
(`roster_subscribe`/`roster_unsubscribe` with the full snapshot, live
`roster_update` pushes keyed by the TS roster `agentId` = session id, and
authenticated `worker_roster_delta` self-reports so live status reaches
subscribers without polling), cloud sandbox attach, session
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

Autonomous
continuation driving in the worker's engine: per-message usage accounting
runs in the agent-loop subscription, and after every settled turn the
engine consults the pa-core `AutonomousDriver` policy (product default:
shell quality gates in the session cwd; deterministic drivers injectable
for eval harnesses) — continuations are injected as durable user rows
and gate pass/fail or limit stops surface as durable `autonomous_status`
custom rows.

Live MCP product-path verifier
(`tests/mcp_product_path_e2e.rs`): a settings-declared stdio MCP server
round-trips through a real worker session and kernel - the kernel cell's
`mcp.list_tools`/`mcp.call_tool` resolve `mcp.config` through the
session's host handlers, spawn the fixture
(`tests/fixtures/mcp_echo_server.py`), and echo back.

## Non-goals
No agent behavior inside workers beyond hosting a pa-core engine; no UI.

## Public API
Supervisor entrypoint, worker entrypoint, client connection API for pa-tui/pa-cli, `acp::{run_acp_mode, AcpOptions}` (pa-cli dispatches `--mode acp` through it). Supervision internals `pub(crate)`.


## Depends on
pa-types, pa-core (one-way).
