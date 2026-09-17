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
- done: resource resolution (`resolve()` -> `ResolvedPaths`).
  `crates/pa-core/src/packages/resolve/` ports `DefaultPackageManager
  .resolve()`: precedence-ranked resolution of extensions/skills/prompts/
  themes from configured packages (pi manifest in package.json, convention
  dirs, filter patterns with the `!`/`+`/`-` override forms), settings
  top-level arrays (paths relative to the settings base, `applyPatterns`),
  auto-discovery (`<base>/skills|prompts|themes|extensions` dirs,
  `.agents/skills` ancestor scan up to the git root, pi-mode SKILL.md
  stopping rule, `.gitignore`/`.ignore`/`.fdignore` rules), and bundled
  skills (exe-adjacent `skills/`, websearch + builtin-override excludes).
  First-wins collision order sorts by the TS `resourcePrecedenceRank`
  (project settings > project auto > user settings > user auto > package >
  builtin); dedupe by canonicalized path (symlinked trees resolve once).
  `resolveExtensionSources` covers the CLI-extension temporary scope with
  auto-refresh of unpinned temporary git sources; missing configured sources
  install on resolve unless offline (or skipped via the on-missing policy).
  The resource loader (`crates/pa-core/src/resources`) consumes the enabled
  paths and provenance (SourceInfo) so sessions see package-provided
  skills/prompts; extension *paths* are resolved and surfaced for the
  extension-runner lane.
  Verifier: `crates/pa-core/src/packages/resolve/tests.rs` (54 ported TS
  test cases: settings entries, auto-discovery, ignore files, symlinks,
  pattern forms, package dedupe, offline/missing-source policies, bundled
  skills) plus `crates/pa-cli/tests/package_resources_e2e.rs`: a
  settings-configured fixture package provides a skill that appears in a
  created session's skill list through the full binary pipeline.
- missing (spec below): the extension runner.

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

Update: the host design decision landed as `docs/extensions-runner-design.md`
(sidecar node runtime with a typed RPC surface; discovery ports to Rust, the
TS module surface is preserved by the sidecar host script, staged plan with
per-stage verifiers inside).

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

## 4. Chunked snapshot streaming - done

TS: `daemon-supervisor.ts` `streamSnapshot` (L5413) +
`createStreamedAttachResult` (L5397), `daemon-mode.ts`
`snapshotTransferId` + `createSnapshotTranscriptChunks`
(`snapshot-transcript-cache.ts`, `SNAPSHOT_TARGET_CHUNK_BYTES` = 512 KiB):
when the client advertises `chunked_snapshot`, the attach response omits the
transcript (`messages` arrays emptied, `snapshotStream` descriptor added)
and the snapshot streams as `session_snapshot_begin` /
`session_snapshot_chunk` / `session_snapshot_end` records, one message
array per record under the byte budget, ids shared across response and
records.

- done: `crates/pa-daemon/src/snapshot_stream.rs` (chunking, streamed-result
  rewrite, event/failed record construction, capability normalization);
  `crates/pa-daemon/src/supervisor.rs` routes attach/reattach for chunked
  clients through it; legacy clients keep the full snapshot in the response.
  The attach result now echoes the client's own capability set (live TS
  golden; previously the supervisor's worker-facing set leaked).
- verifier: `crates/pa-daemon/tests/supervisor_e2e.rs`
  `chunked_snapshot_attach_streams_begin_chunk_end` (scripted 700 KB
  transcript -> multi-chunk stream; reassembled transcript equals the
  legacy client's full snapshot; snapshot id is
  `<activeSessionId>-<generation>-<sequence>` from the event cursor) +
  unit tests in `snapshot_stream.rs` (budget, oversized-message, failed
  path, capability normalization). Live-TS golden:
  `crates/pa-daemon/tests/goldens/chunked-attach-live-ts.json`, captured
  read-only from `/tmp/prime-agent-1000/daemon.sock` (10-message session ->
  1 chunk; 2219-message session -> 5 chunks of <= 512 KiB; client
  capability echo; purpose `attach`).
- design deviation (PORTING-NOTES): the Rust supervisor materializes the
  whole snapshot before writing the streamed response, so the TS async
  abort/reservation machinery collapses into the synchronous dispatch;
  a malformed worker transcript surfaces as `session_snapshot_failed`
  after the response, a snapshotless worker payload fails the attach
  before any record exists.


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

## Battery findings (live A/B parity battery; first run 20260916T210320Z, fixed rows verified by run 20260916T221725Z)

Standing battery harness committed at `scripts/battery/` (see
`docs/parity-battery.md` for the flow list and re-run instructions). All
gaps below are reproduced by the committed first run
(`scripts/battery/runs/20260916T210320Z/`) except B-3, whose evidence is
`runs/20260916T203149Z/` (superseded by the short-TMPDIR harness fix, the
product gap remains). B-1, B-7, and B-11 are fixed (run
`runs/20260916T221725Z/`); the same lane also ported the models.json
`apiKey` resolution for daemon-worker request auth (no battery row).
Categories: visual/behavior/protocol/timing.

- B-1 (protocol, f1): FIXED. Rust interactive dropped `--provider`/`--model`
  CLI flags (the daemon worker resolved its model from env or its fallback;
  a capture with `--provider battery --model mock-1` answered with
  `prime-inference/z-ai/glm-5.3`). The TUI create config now carries the
  flags over the wire (TS runtime-config propagation), the supervisor
  persists them into the durable create, and the worker binds them onto its
  engine; env stays the no-flag fallback. Verified by run
  `runs/20260916T221725Z/{ts,rust}/f1_launch/first-prompt-mock-requests.json`
  (both sides request `mock-1`). Historical evidence:
  `runs/20260916T210320Z/extras/rust-interactive-model-flags-session.jsonl`.
- B-2 (visual, f1): TS shows the splash + first-run "Share agent traces
  with Prime Intellect?" notice (Share / Not now, `/traces` hint); Rust
  launches straight into the TUI with neither. Evidence:
  `runs/20260916T210320Z/{ts,rust}/f1_launch/01-launch.txt`.
- B-3 (timing, f1): FIXED (run `runs/20260917T041145Z/`). The worker connect
  budget is 30s (TS `WORKER_CONNECT_TIMEOUT_MS`): socket probes, connect,
  and the auth handshake share one deadline and a stuck child is killed at
  timeout. Over-limit AF_UNIX paths (107-byte `sun_path`) now re-anchor
  through an O_PATH directory fd (`/proc/self/fd/<fd>/<name>`), the same
  mechanism the TS runtime applies transparently (strace of the installed
  product shows `bind(13, {sun_path="/proc/self/fd/12/worker-...sock"}) = 0`),
  so worker sockets bind at arbitrary TMPDIR depth. Evidence:
  `runs/20260917T041145Z/extras/b3-deep-tmpdir/` (worker socket bound at a
  159-char path, full interactive turn over it; per-file session prefix now
  matches TS). Historical evidence: `runs/20260916T203149Z/rust/`.
- B-4 (protocol, f2) - RESOLVED: model tool surface differs - TS exposes only
  `ipython`; Rust exposed `bash`, `edit`, `ipython`. Evidence:
  `runs/20260916T210320Z/{ts,rust}/f2_prompt/mock-requests.json`.
  Fixed (run `runs/20260916T224512Z/`): both sides expose only `ipython`
  (byte-identical tool schemas); `bash`/`edit` stay kernel-resident.
- B-5 (protocol, f2) - RESOLVED: TS prepends a `[harness-digest]` user
  message; Rust sent none. Fixed (same run): the Rust session engine composes
  and delivers the digest at cold context boundaries; the captured digest
  messages are byte-identical on both sides.
- B-6 (protocol, f2) - RESOLVED: system prompt differed (23119 vs 13526
  chars): Rust omitted the conversation-log path, installed skill modules
  line, available-skills inventory, and harness-refinement guidance. Fixed
  (same run): normalized prompts are identical; golden test
  `crates/pa-core/tests/golden/system_prompt.rs` pins the assembly against
  the TS `buildSystemPrompt` over the vendored skills.
- B-7 (protocol, f2/f5) - RESOLVED: TS issues a post-turn status-line request
  to a small model (`qwen/qwen3-30b-a3b-instruct-2507`); Rust now issues the
  same request (daemon-session-summarizer port in
  `crates/pa-daemon/src/status_line.rs`: same trigger, model, system prompt,
  and max_tokens; recap broadcast as `session_status`). Verified by run
  `runs/20260916T221725Z/{ts,rust}/f5_side_questions/statusline-requests.json`.
  Historical evidence: `runs/20260916T210320Z/extras/ts-statusline-request.json`.
- B-8 (protocol, f3/f8): FIXED (run `runs/20260917T041145Z/`). Session
  entry sets now match on both sides: `service_tier_change` is emitted in
  the creation prefix (fresh + resume, settings default, engine and daemon
  store), `custom_message` (harness_digest) and `compaction` landed in
  #83/#85, and the queue snapshot moved from session-file `custom` entries
  into the worker recovery journal. Settled status verdicts now persist as
  `agent_status` entries (real model classifications and transcript error
  verdicts only; the needs_input fallback and sweeps never grow the journal;
  respawned workers seed the in-memory verdict from the persisted entry).
  Differential evidence: `runs/20260917T041145Z/extras/b8-agent-status/`.
  Historical evidence: `runs/20260916T210320Z/f3_tool-session-shapes.json`,
  `f8_resume-session-shapes.json`.
- B-9 (visual, f4): TS `/` opens the slash-command menu; Rust `/` types into
  the composer. Evidence:
  `runs/20260916T210320Z/rust/f4_commands/01-slash-menu.txt`.
- B-10 (protocol, f7): TS daemon `compact` works and returns
  `{summary, firstKeptEntryId, tokensBefore, details{readFiles, modifiedFiles}}`;
  Rust answers `{"command":"unknown"}`. Evidence:
  `runs/20260916T210320Z/{ts,rust}/f7_compaction/compact-response.json`.
  (Same as item 5 above, now with live mock-provider evidence.)
- B-11 (behavior, f8): FIXED. TS print `-c` refuses while the session is
  active in the daemon ("Session is already active in <id>: <path>"); the
  Rust print path now guards `-c`/`-r` with a daemon live-roster probe and
  refuses with the exact message (message + canonicalization from the
  session-lease port). Verified by run
  `runs/20260916T221725Z/{ts,rust}/f8_resume/continue-cmd.json` (both sides
  exit 1 with the same shape). Historical evidence:
  `runs/20260916T210320Z/ts/f8_resume/continue-cmd.json`.

Passed-check highlights (both products agree, live through the mock):
print-mode stdout identical; `ipython` tool turns execute in both;
side questions stream `side_question_event` running->complete on both;
wire attach returns the same snapshot data keys; CLI `attach` opens in both.

## 9. Interactive TUI visual parity - done for the scripted core states

Verified by a tmux frame-diff harness (`scripts/visual_parity.py`): the harness
drives the installed TS binary and the Rust binary side by side in tmux, runs
the same scripted turn on both (faux model with content-block responses: a
thinking block, a text block, an `ipython` tool call, and a final answer), and
compares `tmux capture-pane -e` frames after normalizing volatile content
(versions, session ids, durations, token counts, spinner frames).

Verified states (both at 120x36 and 220x50, PASS on 2025-06-27):
- (a) fresh start: splash + header (model/cwd), prompt, footer, collapsed-mode
  indicator, empty editor on `userMessageBg`.
- (b) idle after a turn with a tool-call card: user block, assistant text,
  collapsed ipython card (`\u2713 python \u00b7 <code preview> \u00b7 \u2191 N \u2193 M lines \u00b7 <duration>`),
  final answer, tray stats.
- (c) thinking visible (Ctrl+O): dim thinking block between user message and
  assistant text, `Details mode` indicator.
- (d) spinner/working state: loader row with activity label, elapsed seconds,
  token estimate, spinner frames.

Deliberate deviations / notes:
- The Rust binary does not ship the `prime-agent-runtime` sidecar next to the
  binary like the TS release does; the harness sets `PI_PACKAGE_DIR` to the
  installed TS release directory so both run the same kernel runtime
  (`find_runtime_package_dir` in the harness).
- `code_preview` helpers are vendored under `crates/pa-tui/src/code_preview/`
  (pa-tui may not depend on pa-core); consolidation into pa-types is a
  follow-up.
- Tool-card output rows beyond the collapsed line ("all" mode) and markdown
  block types outside the scripted content are not yet frame-verified.
- The harness normalizes boundary foreground resets
  (`\x1b[39m` at end-of-row vs before next row's margin): tmux emits the same
  reset at either position for identical screens.
- The daemon worker exposes an event-log seam (`PA_DAEMON_EVENT_LOG=<path>`)
  used to verify wire parity during harness development.

Run it: `python3 scripts/visual_parity.py --sizes 120x36 220x50` (requires the
TS binary on PATH and a built `target/debug/prime-agent`).

## 10. Agent-to-agent messaging (`send_message` / kernel `agent_message.*`) - partial

TS: `modes/daemon/daemon-supervisor.ts` `send_message` block, `daemon-mode.ts`
`worker_deliver_message` + `sendAgentSessionMessage`, `core/agent-messages.ts`
(receipt/prompt/validation), `core/kernel/shared.ts` (sent-message bridge),
`modes/daemon/supervisor-link.ts`.

- done: supervisor `send_message` arm (`crates/pa-daemon/src/messaging.rs`):
  source/target resolution with the TS unknown-session errors, self-target
  refusal, sender endpoint from the source session's live summary (CLI-origin
  sender is the client id), `worker_deliver_message` routing to the target.
  Worker delivery (`crates/pa-daemon/src/worker.rs`
  `handle_worker_deliver_message`): renders the exact TS
  `[agent-message from ...]` prompt, steer lane by default / `follow_up` on
  request, pending-capacity guard, `createAgentSessionMessageReceipt`-shaped
  receipt. Worker->supervisor link (`crates/pa-daemon/src/supervisor_link.rs`,
  TS supervisor-link.ts port) and the kernel `agent_message.send` /
  `agent_observe.*` host controllers wired through the engine's
  `extra_host_handlers` (`crates/pa-daemon/src/agent_engine.rs`).
  Verifiers: unit tests per landed piece (`messaging.rs`,
  `worker::agent_message_tests`, the `supervisor_link.rs` echo round-trip)
  and the unknown-target e2e in `crates/pa-daemon/tests/supervisor_e2e.rs`.
- known issue (superseded): the client-to-client `send_message` e2e (second
  session created, `send_message` with `fromActiveSessionId` from the first)
  hangs in flight - the client gets no reply within its 15s deadline;
  suspected `route_command` deadlock on the supervisor's in-loop route. Do not
  re-add that e2e as-is: the thin-supervisor Stage 3 peer-messaging work
  removes the routed path (delivery over direct worker peer links), which
  supersedes the bug. Worker-side delivery and the unknown-target path stay
  covered by the unit tests above.
- deferred gaps: the TS supervisor's saved-session wake-up for non-resident
  targets (catalog resolve + worker reuse) is not ported - an unknown target
  always answers `Unknown active session: <selector>` where the TS CLI would
  wake the saved session and print `Sent to <name>`; the family-reach
  assertion needs the session family catalog the thin supervisor does not
  keep; delivered agent messages persist as plain user prompts, not TS
  `custom` messages (`customType: "agent_message"` with a details block); the
  sender relationship in the delivered prompt derives from `runtimeKind`
  (subagent -> child) instead of the family graph; the TS `deliveryMode` wire
  field is legacy-ignored in TS but honored here (default `steer` matches TS).
