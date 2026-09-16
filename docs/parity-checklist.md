# Remaining-parity checklist

Audit of the TS product surface vs the Rust crates, per candidate. Status:
`done` (verifier proves it), `partial` (surface exists, behavior gap), `missing`
(not implemented). Evidence cites TS paths under `~/prime-agent` (read-only
ground truth) and Rust paths in this repo. Wire goldens come from the live TS
daemon (`/tmp/prime-agent-1000/daemon.sock`, protocol 7, schema 28) captured
with read-only `get_*` commands.

## 1. Session-info scan/list machinery - partial

TS: `packages/coding-agent/src/core/session-manager.ts` `SessionManager.list`
(L2449, cwd-filtered) / `listAll` (L2468) over `listSessionsFromDir` with
incremental per-file scan state (`foldSessionScanLine`, session scan queue) and
streaming `onProgress`/`onSession` callbacks.

- done: daemon surface. `crates/pa-daemon/src/supervisor.rs`
  `handle_saved_session_list` streams `session_list_item` +
  `session_list_progress` events then the final response (TS
  `handleSavedSessionList` parity); `crates/pa-daemon/src/session_store.rs`
  `read_session_info`/`list_sessions` extract the row fields.
  Verifier: `crates/pa-daemon/tests/supervisor_e2e.rs` asserts the event
  sequence and saved-session row shape against live-TS goldens (commit #58);
  `list --all` summary rows differentially asserted (commit #56).
- missing: the CLI seam. `crates/pa-cli/src/public_command.rs`
  `run_internal_agent_command` fails with `unavailable("the daemon client")`
  for `list --all` / `agents` / `attach` / `send` / `schedule`, so the saved
  scan never reaches users outside the daemon protocol.
- partial (perf only, no behavior gap): TS scans incrementally (chunked reads +
  per-file fold state) while `read_session_info` re-reads whole files; row
  output is identical, so this is an optimization follow-up, not a parity bug.

Follow-up spec (CLI lane): wire a pa-cli daemon client (connect to
`~/.prime/agent/daemon.sock`, hello + envelope commands) behind
`list`/`agents`/`attach`; the supervisor side is already parity-tested.

## 2. Extensions / package-manager - partial (CLI half done)

TS: `packages/coding-agent/src/core/package-manager.ts` (2443 LoC: install/
remove/update of npm/git/local packages, settings `packages` records, and
session resource resolution) plus the extension runner under
`packages/coding-agent/src/core/extensions/` (loader.ts/runner.ts/types.ts,
~3.3k LoC, jiti-based TS module loading).

- done: the package-manager CLI subsystem. `crates/pa-core/src/packages/`
  ports install/remove/list/update for `npm:` (npm CLI child process via the
  `npmCommand` setting, global root via `npm root -g`, bun `pm bin -g`
  special case), git (`git clone`/`checkout`, fetch/reset/clean updates,
  remove with empty-parent pruning, GIT_TERMINAL_PROMPT=0 remote probes),
  and local-dir sources (bind by path, settings stored relative to the
  settings base). Settings mutation goes through
  `SettingsManager::set_packages`/`set_project_packages` (field-scoped
  merge into the current file; a scope whose file failed to parse is never
  written). Source parsing matches TS exactly, including the quirks:
  `git://host/path` parses as a LOCAL path (the `git:` prefix strips the
  protocol), shorthand only with the `git:` prefix, hosted shortcut forms
  (`github:`, `gitlab:`, `bitbucket:`, `gist:`), and `#`/`@` ref pinning
  (pinned packages never auto-update).
  `crates/pa-cli/src/package_command.rs` runs every subcommand to
  completion: `install`/`remove`/`list`/`update [source]`, progress lines,
  `Installed`/`Removed`/`Updated` output, "No matching package found"
  errors with suggestions, and settings-load warnings
  (`Warning (package command, <scope> settings): ...`).
  Verifier: `crates/pa-cli/tests/package_e2e.rs` drives a 25-step corpus
  (local-dir, npm-shim, git-ssh-shim fixture) against BOTH the TS binary
  and the Rust binary and asserts identical stdout/stderr/exit codes after
  path/hash normalization, plus direct Rust-sandbox assertions (settings
  documents, npm project prefix, git dir pruning). No network is used;
  npm-network behavior needs no `#[ignore]` marker because the shim covers
  the flows offline.
- missing (typed boundary): Prime Agent self-update - the `prime-agent
  update` / `package update --self` half needs the native release plan
  (`cli/native-update.ts`, release manifests, rollback) and the daemon
  update-restart coordinator (`cli/daemon-update-restart.ts`). The Rust CLI
  reports a typed "self-update is not available in this build yet" error
  until that lane lands.
- missing (spec below): resource resolution (`resolve()` -> `ResolvedPaths`)
  and the extension runner.

Follow-up spec - resource resolution (skills/resources lane, ~600-800 LoC):
port `DefaultPackageManager.resolve()`: precedence-ranked resolution of
extensions/skills/prompts/themes from (a) configured packages (pi manifest in
`package.json`, convention dirs, filter patterns with `!`/`+`/`-` override
forms), (b) settings top-level arrays (paths relative to the settings base,
`applyPatterns` enable/disable), (c) auto-discovery (`<base>/skills|prompts|
themes|extensions` dirs, `.agents/skills` ancestor scan up to the git root,
pi-mode SKILL.md stopping rule), (d) bundled skills with the websearch and
builtin-override exclude patterns. Consumers: `crates/pa-core/src/resources`
(resource-loader.ts) and startup notices (`check_for_available_updates` +
`modes/shared/startup-notices.ts` - already ported on the manager).
First-wins name collision resolution sorts by the TS `resourcePrecedenceRank`
(project settings > project auto > user settings > user auto > package >
builtin); dedupe by canonicalized path. `resolveExtensionSources` (temporary
scope + auto-refresh of unpinned temporary git sources) belongs to this port
as well.

Follow-up spec - extension runner (design lane, needs a runtime decision):
TS extensions are TypeScript/JS modules loaded with jiti into the agent
process (loader.ts aliases `@earendil-works/pi-*` packages into the built
dist), exposing lifecycle hooks and custom tools through `ExtensionRunner`.
A Rust process cannot import TS modules; the port needs a deliberate host
design before any code: either a sidecar JS runtime (node/bun subprocess with
a typed RPC surface mirroring `extensions/types.ts`), or a wasm/plugin
contract, plus the resource loading of resolved extension paths from the
resolution lane above. Surface to preserve: the `pi.extensions`/
`extensions/index.ts|js` discovery conventions and the extension event
surface consumed by sessions (`BeforeProviderRequest`, `AgentEnd`, custom
tools, slash commands, keybindings). This is the largest remaining gap in
the extensions area and blocks only extension-authored content, not the
package install/remove flows landed here.

## 3. Side questions (`side_question_transcript`) - done

TS: `packages/coding-agent/src/core/side-question.ts` (startSideQuestion:
second LLM turn over `previousTurns` with its own retry policy, events
`side_question_event`), daemon handlers `daemon-mode.ts` L4730
(`start_side_question`/`abort_side_question`).

- done: `crates/pa-daemon/src/protocol.rs` `KNOWN_COMMAND_TYPES` accepts both
  commands and routes them (`command_active_session_id` +
  `command_type_name`); the worker runs them
  (`crates/pa-daemon/src/worker.rs` `handle_start_side_question` /
  `handle_abort_side_question`: TS error strings, one live run per client per
  session, abort by owner, runs aborted on detach/kill). The turn behavior is
  one extra `SessionEngine::run_side_question` call per run:
  `crates/pa-daemon/src/engine.rs` (seam + `side_question_event` wire values),
  `ScriptedEngine` (scripted side-question provider calls with a scriptable
  retry policy), and `AgentSessionEngine` over
  `pa-core::session_engine::side_question::run_side_question` (conversation
  clone with the KV-cacheable prefix preserved, tool block, turn cap, retry
  policy from `pa-core::session_engine::provider_retry`).
- done: verifier. Unit: retry-policy decisions + driver
  (`pa-core provider_retry`), side-thread clone/replay/retry/abort/tool-block
  (`pa-core side_question`). E2e:
  `crates/pa-daemon/tests/supervisor_e2e.rs`
  `side_questions_start_abort_and_events_scripted` (scripted engine, asserts
  TS error strings, event sequence running->complete, abort -> cancelled).
  Differential against the live TS daemon (protocol 7): `start_side_question`
  response shape, guard errors ("Side question already exists: <id>", "A side
  question is already running for this client and session", "Unknown active
  session: <id>"), `abort_side_question` `{aborted: false}` for unknown ids /
  `{aborted: true}` for live runs, and the live event stream
  (running empty answer -> running partial -> complete; abort -> cancelled with
  the partial answer).

## 4. Chunked snapshot streaming - missing (wire types present)

TS: `daemon-supervisor.ts` `streamSnapshot` (L5413): when the client
advertises `chunked_snapshot`, attach snapshots stream as
`session_snapshot_begin` / `session_snapshot_chunk` / `session_snapshot_end`
over a transcript cache with reservations, duplicate validation, and abort
signals.

- missing: `crates/pa-types/src/daemon.rs` has `SessionSnapshotBegin`/`Chunk`/
  `End`/`Failed` event shapes and `crates/pa-daemon/src/protocol.rs`
  advertises the `chunked_snapshot` capability, but the supervisor never emits
  them: attach always returns the full snapshot inside the response
  (`crates/pa-daemon/src/supervisor.rs` route + `worker.rs` handle_attach).

Follow-up spec (daemon lane): emit begin/chunk/end from the existing attach
payload when the client capability is present, chunking the `messages` array
by a target byte budget (~256 KiB); keep the non-chunked path for legacy
clients. Differential verifier: golden event sequence captured from the TS
supervisor with a chunked-snapshot client.

## 5. Compaction daemon wiring (`compact` on the daemon session) - missing

TS: `daemon-mode.ts` L5236 `case "compact"` -> `session.compact(customInstructions)`
returning `CompactionResult`; `abort_compaction`; `set_auto_compaction`.

- missing on the daemon surface: `crates/pa-daemon/src/protocol.rs`
  `KNOWN_COMMAND_TYPES` has no `compact`/`abort_compaction`; the worker keeps
  only `is_compacting` display fields.
- done (engine side): `crates/pa-core/src/session_engine/mod.rs`
  `AgentSession::compact` + `compact_session.rs` (cut resolution, summarizer
  call, compaction entry persistence, context rebuild) and the `CompactionSettings`
  decision logic in `compaction.rs`.

Follow-up spec (daemon lane): add `compact`/`abort_compaction` to the worker
command set, routed to `AgentSessionEngine` via a `SessionEngine::compact`
trait method (pa-core `AgentSession::compact` already returns
`CompactionResult`); scripted engine answers with the TS error for engineless
sessions. Verifier: faux-script session, assert compaction entry appears in
the session file and get_state reports `compactionCount > 0`.

## 6. `rlm.create_session` through the daemon - missing

TS: kernel `prime-agent-runtime/src/rlm/__init__.py` `create_session` ->
`host_request("rlm.create_session")`; daemon-mode `createRlmRootSession`
(L2627) creates a depth-0 resident session over the supervisor link and
prompts it.

- missing: no host handler is registered for `rlm.create_session` (or
  `rlm.run`/`rlm.find_models`) anywhere in the product path.
  `crates/pa-core/src/session_engine/runtime_wiring.rs` registers only
  `goal.*`, `rlm_heartbeat.*`, `agent_message.send`, `agent_observe.*`, and
  `mcp.*`; `crates/pa-core/src/kernel/rlm_runtime.rs` holds the pure helpers
  (name/thinking/model validation, model search) with handlers unbuilt.
- The system prompt already documents the surface
  (`crates/pa-core/src/prompts/mod.rs` L165), so the model believes it exists.

Follow-up spec (RLM lane, large): register `rlm.run` (spawn), `rlm.find_models`,
`rlm.create_session`, `rlm.progress_note`, `rlm.list_subagents`,
`rlm.delete_subagent` host handlers in `runtime_wiring.rs`; the subagent
machinery (child sessions, roster, collect) is the prerequisite for everything
except `rlm.find_models`.

## 7. Read-command surface (`get_session_stats` / `get_context_tree` / `get_commands` / `get_resource_snapshot`) - partial after this PR

TS: `daemon-mode.ts` L183352+ delegates to `getSessionStats` (TS
`core/session-stats.ts` shape), `getContextTree` (`core/context-tree.ts`),
`createAgentConnectionCommands` / `createAgentConnectionResourceSnapshot`
(`modes/agent-connection/snapshot.ts`).

- done in this PR: `get_session_stats` and `get_session_header` - worker
  handlers computed from the session store, routed supervisor -> worker.
  Verifier: `crates/pa-daemon/tests/supervisor_e2e.rs`
  `session_stats_and_header_match_live_daemon_goldens` asserts the response
  shape against goldens captured from the live TS daemon (see below), plus
  unit tests in `crates/pa-daemon/src/session_stats.rs`.
- missing: `get_context_tree` - needs RLM child-session machinery (live child
  nodes + disk-loaded completed children); root-only would diverge whenever a
  session has children. Blocked on item 6.
- missing: `get_commands` / `get_resource_snapshot` - need per-worker resource
  loading (skills/prompts/themes/extensions). pa-core
  `resources::load_resources` provides the data; wire an engine-level accessor
  plus the command handlers. Note: `crates/pa-core/src/skills/mod.rs`
  `SourceOrigin` currently serializes `topLevel`, but the TS wire uses
  `top-level` (see `core/source-info.ts`); fix to kebab-case when landing
  these commands.

Live-TS goldens (read-only captures, protocol 7):
- `get_session_stats` data: `{ sessionFile, sessionId, userMessages,
  assistantMessages, toolCalls, toolResults, totalMessages, tokens: { input,
  output, cacheRead, cacheWrite, total }, cost, contextUsage? }` where
  `contextUsage` is `{ tokens, contextWindow, percent }` and is omitted when
  the session has no model context window.
- `get_session_header` data: `{ header: { type: "session", version, id,
  timestamp, cwd, parentSession?, rlmDepth?, git? } }`.

## 8. Adjacent gaps found during the audit

- CLI daemon client: every daemon-backed public command in
  `crates/pa-cli/src/public_command.rs` fails with an "unavailable" error
  (`list`, `agents`, `attach`, `send`, `schedule`, `status`, ...). The TS
  binary on PATH answers all of them. Largest single user-visible gap.
- Daemon command vocabulary: `crates/pa-daemon/src/protocol.rs`
  `KNOWN_COMMAND_TYPES` accepts 26 command types; the TS supervisor accepts
  ~100 (`daemon-supervisor.ts` `DAEMON_COMMAND_TYPES`). `pa-types` already
  models all variants, so enabling them is per-command work in pa-daemon only.
- Tool-result persistence: TS session files contain `toolResult` message
  entries; the pa-core persistence listener writes only user/assistant
  messages (`crates/pa-core/src/session_engine/mod.rs` `persist_event`).
  Affects `get_session_stats.toolResults`, transcripts, and external
  tooling reading session files.
