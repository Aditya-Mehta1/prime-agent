# Completion matrix: user-facing product surface vs status

Audit of the Rust rewrite against the TS product (`~/prime-agent`, ground truth),
taken from code on `main` at `7d67021` (through PR #93). Status vocabulary:

- **complete** - the surface works end to end with a verifier against the TS
  product or live wire goldens.
- **partial** - the surface exists and users reach it, but named behavior is
  missing or unverified.
- **in-flight** - a lane branch carries the work; `main` does not have it yet.
- **missing** - not implemented in the product path.

**Battery greenness is not product parity.** The live A/B battery
(`scripts/battery/`, `docs/parity-battery.md`) last ran `20260917T062810Z`
with "0 gaps, 35 parity checks passed". That means only its scripted flows
(f1-f11 over a deterministic mock provider) agree between the binaries. It
does not cover the families marked partial/missing below; today's interactive
`--thinking` blocker (see "Thinking control") was invisible to it. Every row
below is evidence-based, not battery-based.

| # | Family | Status | One-line remaining scope |
|---|--------|--------|--------------------------|
| 1 | Interactive session TUI | partial | verified core states only; missing all-expanded detail mode, OSC 133, ambient skill commands, broad markdown breadth |
| 2 | Thinking control (`--thinking`, `/effort`) | missing (interactive) | flag parsed but never reaches the worker; interactive unusable until the fix lane lands |
| 3 | Slash commands | partial | registry/menu/autocomplete/session forwarding landed; almost all client command UIs report "not available" |
| 4 | Agents view | in-flight | roster protocol + TUI mode + entry points on `lane/agents-view`, unmerged |
| 5 | Daemon supervision & protocol | partial | 32 of ~106 TS command types; read commands, saved-session wake, reconnect missing |
| 6 | Compaction | complete (daemon wire) | kernel `compact.run` host handler missing (model cannot compact itself) |
| 7 | Side questions | complete | - |
| 8 | Agent-to-agent messaging | partial | peer transport done; saved-session wake, `custom` persistence, family-graph relations missing |
| 9 | RLM recursion (`rlm.spawn`/`collect`/subagents) | in-flight | all `rlm.*` host handlers unregistered on `main`; `lane/rlm` in flight |
| 10 | Kernel host-request surface & continual harness | partial | only goal/heartbeat/messaging handlers registered; `refine.*`, `compact.run`, `harness.*`, `model.info` unregistered despite prompt advertising them |
| 11 | RLM dogfood (this harness, run by the Rust binary) | missing | mission-host dogfood untested; depends on rows 9-10 |
| 12 | MCP | partial | CLI config/catalog/gating only; product-path wiring, OAuth/login UI, generic connector execution unproven |
| 13 | Extensions | partial | package manager + resource resolution done; sidecar runner stages 1-6 pending |
| 14 | Skills | partial | loading + prompt inventory done; skills-as-commands and attach-image product backing missing |
| 15 | CLI command surface | partial | list/attach/stop/rename/send/schedule wired; status/doctor/shutdown unavailable, self-update missing, model-list catalog + config UI missing |
| 16 | Headless modes | partial | print/json done; RPC and ACP modes missing |
| 17 | Session persistence | partial | entry-set parity landed; `toolResult` entries missing |
| 18 | First-run onboarding | complete | - |
| 19 | Trace sharing | missing | opt-in setting persists; no upload subsystem, `/traces` UI unavailable |
| 20 | Eval / verifiers Prime flow | missing | not started |
| 21 | Native release / installer / CI | missing | binary does not ship the kernel runtime sidecar; `PI_PACKAGE_DIR` workaround |
| 22 | Platform readiness (Windows) | in-flight | traits audited (`docs/windows-readiness.md`); no shipping support yet |

Details and evidence per family follow.

## 1. Interactive session TUI - partial

Done, frame-diff verified at 120x36/220x50 against the TS binary
(`scripts/visual_parity.py`, checklist §9, PR #80): fresh start, idle
post-turn with tool-call card, thinking visible, spinner/working state.
Provider-failure surfacing (#90) and the splash/onboarding (#93) are battery
verified (`runs/20260917T062810Z`). Attach through the daemon (#69), direct
transport (#82/#92).

Remaining:

- **Ctrl+O detail mode**: Rust has two levels only. `pa-tui/src/chat.rs` L18
  (`Detail` enum: `Overview`/`Details`), toggled in
  `pa-tui/src/session_ui.rs` L583-584. TS has three -
  `interactive-mode.ts` L7726 `setChatDetail(detail: "overview" | "details" |
  "all")`. The all-expanded (tool output rows) mode is missing.
- **OSC 133 markers**: TS emits shell-integration markers
  (`\x1b]133` sequences in `modes/interactive/components/user-message.ts` and
  `assistant-message.ts`); no Rust crate emits them (repo-wide grep finds none).
- **Markdown breadth**: only the scripted content block types are
  frame-verified (checklist §9 deviations).
- **Ambient skill/template commands**: the TS daemon lists skills and
  templates as slash commands; the Rust registry is builtins-only
  (`pa-types/src/slash_commands.rs`; battery B-9 note).

## 2. Thinking control (`--thinking`, `/effort`) - missing on the interactive path

- The CLI parses `--thinking` (`pa-cli/src/args.rs` L15, L339-346), but the
  interactive create payload carries no thinking field
  (`pa-tui/src/session.rs` L40 carries provider/model only - the B-1 fix
  propagated those, not thinking).
- The daemon worker hardcodes thinking off: `pa-daemon/src/agent_engine.rs`
  L264 (`thinking_level: None` in `build_session`) and the session file
  records `thinking_level_change: "off"` (`pa-daemon/src/worker.rs` L1881-1904:
  "The daemon engine runs with thinking off").
- `/effort` therefore cannot take effect interactively, and a
  local trial hit this as a live blocker (fix lane `lane/thinking-parity`
  in flight, no commits yet).
- Print mode DOES resolve thinking (`pa-cli/src/print_runtime.rs` L111,
  `resolve_thinking_level` L241).

Status: interactive thinking is NOT usable until the fix lane lands.

## 3. Slash commands - partial

Done (#91): shared registry in `pa-types/src/slash_commands.rs` (38 builtin
commands, TS name/description/alias parity), the `/` menu with fuzzy filter,
argument-hint column, directional scroll, suggestions ("Unknown command: /x.
Did you mean /y?"), and session-command forwarding to the worker.

Remaining:

- Client command UIs: `pa-tui/src/session_ui.rs` L418-445 implements only
  `help`, `list`, `switch`, `exit`, `new`, `quit`; every other client command
  (model/settings/mcp/hotkeys/theme/traces/export...) prints "/x is not
  available in this client yet" (L442).
- `/effort` is additionally blocked by family 2.
- Dynamic `/effort` argument hint and model-eligibility filtering of `/fast`
  (battery B-9 note) remain.

## 4. Agents view - in-flight

`prime-agent agents` is routed (`pa-cli/src/public_command.rs` L148 sets
`explicit_agents_view`; `pa-cli/src/mode.rs` L134 carries it into
`RunOptions`) but nothing on `main` consumes it. The lane branch
`lane/agents-view` (commits `2ec737d`..`2efec39`, unmerged) adds the roster
protocol, the TUI mode (unified roster/catalog rows, sections, search,
open/attach), and entry points (`agents`, bare `-r`, `/resume` return).
Battery flow f9 only captures frames on both sides; frame diffing belongs to
the lane.

## 5. Daemon supervision & protocol - partial

Done: thin supervisor + per-session worker processes (stages 1-3, #79/#82/#89),
session self-registration/adoption, chunked snapshot streaming (#74),
compaction (#85), side questions (#70), status-line recap (#81/#83),
queue/retry/restart/kill, `send_message` arm, worker robustness (#88),
32 of ~106 TS command types (`pa-daemon/src/protocol.rs` L25-58
`KNOWN_COMMAND_TYPES` vs `daemon-supervisor.ts` L244 `DAEMON_COMMAND_TYPES`).

Remaining (checklist §7/§8 stay authoritative):

- Read commands `get_context_tree`, `get_commands`, `get_resource_snapshot`
  (blocked on RLM children / per-worker resource loading).
- Saved-session wake for non-resident `send_message` targets (checklist §10).
- `DaemonRoutedClient` reconnect semantics: the TS client
  (`modes/daemon/daemon-routed-client.ts`) reconnects and replays; the Rust
  CLI client (`pa-cli/src/daemon_client.rs`) is single-shot, no reconnect.
- Daemon command vocabulary breadth (32 vs ~106 types; per-command work in
  pa-daemon only - `pa-types` already models the variants).

## 6. Compaction - complete on the daemon wire

`compact`/`abort_compaction`/`set_auto_compaction` are worker commands
(`pa-daemon/src/protocol.rs` L51-53; `pa-daemon/src/worker.rs` L843,
L1498-1500), differential-tested by battery flow f7
(`runs/20260917T062810Z`: both sides succeed; shape delta: TS includes
`details{readFiles, modifiedFiles}`, Rust omits the details block).

Remaining nuance: the kernel `compact.run`/`compact.status` host handler (TS
`agent-session.ts` L3703-3731) is not registered in the Rust product path, so
the model cannot compact its own session (see family 10).

## 7. Side questions - complete

Protocol + worker + engine + retry policy + differential goldens (checklist
§3, #70; battery f5).

## 8. Agent-to-agent messaging - partial

Done (#84, #89): supervisor `send_message` arm, worker delivery with the TS
prompt rendering, kernel `agent_message.send`/`agent_observe.*` controllers,
worker-to-worker peer transport with peer tickets, restart-survival e2e
(`pa-daemon/tests/peer_messaging_e2e.rs`).

Remaining (checklist §10 "deferred gaps" stands): saved-session wake,
`customType: "agent_message"` persistence, family-graph-derived sender
relations.

## 9. RLM recursion - in-flight

On `main`, no `rlm.*` host handler is registered anywhere in the product
path: `pa-core/src/session_engine/runtime.rs` L54 registers only
`goal.{get,create,complete}` and `rlm_heartbeat.{create,list,update,delete}`;
the daemon worker adds only `agent_message.send` + `agent_observe.*`
(`pa-daemon/src/agent_engine.rs` L228-256 `extra_host_handlers`). The TS
equivalents are `agent-session.ts` L10375-10415 (`rlm.run`, `rlm.create_session`,
`rlm.find_models`, `rlm.list_subagents`, `rlm.collect`, `rlm.progress.note`,
`rlm.delete_subagent`). The pure kernel helpers exist
(`pa-core/src/kernel/rlm_runtime.rs`), and the system prompt already advertises
the whole surface (`pa-core/src/prompts/mod.rs` L165) - so the model believes
`rlm.spawn` works when it does not. Lane `lane/rlm` (unmerged,
`5d09e36`/`5426b3b`/`4c2254c`/`7ae53d0`) carries the child machinery.

## 10. Kernel host-request surface & continual harness - partial

Registered in the product path (evidence above): `goal.*`, `rlm_heartbeat.*`,
`agent_message.send`, `agent_observe.*`. The harness digest is delivered at
cold-context boundaries (#83).

Missing product-path handlers (all advertised by the system prompt,
`pa-core/src/prompts/mod.rs` L33 - "Continual harness state is available as
`rlm.harness`..." and L217 - "`await refine.run()`"):

- `refine.status`/`refine.run` (TS `agent-session.ts` L3754-3770).
- `harness.*` CRUD / `record_refinement` / `overview` (the continual-harness
  entries this session's own harness digest advertises).
- `model.info` (TS L10416) - which also blocks the bundled `attach_image`
  skill (`skills/attach-image/src/attach_image/attach_image.py` L237).
- `rlm.*` (family 9).
- `mcp.*` (family 12): the handlers exist in `pa-core/src/mcp.rs` L383
  (`mcp.refresh`, `mcp.config`, `mcp.begin_login`) but no product path calls
  `McpManager::register_host_handlers` (only in-crate tests, L677/L704).

## 11. RLM dogfood - missing

The mission's own success criterion - running Prime Agent sessions on the
Rust binary as the harness (this very session's control loop) - is untested:
it needs families 9 and 10 first (child spawn/collect, harness CRUD, refine).
The handler/child machinery exists only on the unmerged `lane/rlm`.

## 12. MCP - partial

Done: the CLI management command (`pa-cli/src/mcp_command.rs`, 475 LoC:
`mcpServers` settings store, name/env validation, builtin catalog gating
`linear`/`notion`), and the core manager (`pa-core/src/mcp.rs`: catalog,
integrations, ACP server slots, auth-gated skill overrides).

Missing: product-path wiring (the daemon worker passes
`generic_mcp_servers: vec![]`, `pa-daemon/src/agent_engine.rs` L271, and
registers no `mcp.*` handlers), OAuth/login UI (`mcp.begin_login` only
registers when an interactive login callback is provided, `pa-core/src/mcp.rs`
L445-447), and generic connector execution (no MCP client protocol
implementation). No end-to-end proof that a configured server's tools reach a
session.

## 13. Extensions - partial

Done (#66, #75): package install/remove/list/update (npm/git/local with the
TS quirks) and full resource resolution (packages, settings arrays,
auto-discovery, ignore rules, bundled skills, precedence) - checklist §2.

Remaining: the sidecar extension runner per `docs/extensions-runner-design.md`
(staged 0-6; stage 0 discovery is part of #75's resolution, stage 1 host
lifecycle is in flight on `lane/extensions` `3df39c0`/`6638aa3`/`f6ad5b7`).
Stages 2-6 (tool execution, events, commands/keybindings/UI, reload, failure
policy) are unstarted on `main`.

## 14. Skills - partial

Done: markdown + Python skill loading, resource resolution (packages/settings/
auto/bundled), the skill list in the system prompt, and kernel pre-imports
(`pa-core/src/skills/`, `pa-core/src/session_engine/runtime_wiring.rs`).

Remaining:

- Skills-as-commands: the TS daemon surfaces skills/templates as slash
  commands; the Rust registry is builtins-only (family 1/3 overlap).
- `attach_image` (bundled skill) cannot work: its `model.info` host request is
  unregistered (family 10).

## 15. CLI command surface - partial

Wired through the daemon client (#67, `pa-cli/src/daemon_command.rs`):
`list [--all]`, `attach`, `stop` (kill), `rename`, `send`, `schedule`/cron.
`package` (#66) and `mcp` (family 12) run to completion; flag/error parity
rows are differential-tested (`pa-cli/tests/differential_cli.rs`).

Missing:

- `model list` (found by this audit): the TS binary prints the model catalog
  table (`cli/list-models.ts`); the Rust binary falls through to print mode
  and answers "No response produced." - `RunOptions.list_models` is parsed
  (`pa-cli/src/mode.rs` L128) but no runtime consumes it
  (`pa-cli/src/print_runtime.rs` has no arm; only the dead
  `UnavailableRuntime` in `pa-cli/src/mode.rs` L188 checks it).
- `config`: "the config command needs the interactive resource configuration
  UI (pa-tui), which is not linked into this build yet"
  (`pa-cli/src/lib.rs` L66-69).
- `status`/`doctor`/`shutdown`: typed "daemon discovery ... is not available
  in this build yet" (`pa-cli/src/public_command.rs` L375-398; drivers exist on
  the unmerged `lane/cli-discovery-2` `c4c285c`).
- Self-update: `update` reports "self-update is not available in this build
  yet" (checklist §2, native release plan + update-restart coordinator).
- `session export` (HTML): `MissingSubsystem::SessionExport`
  (`pa-cli/src/lib.rs` L107-111).

## 16. Headless modes - partial

Done: print/text and json single-shot modes over the pa-core engine with real
providers, thinking resolution, resume/-c guard (#81/#87), faux-script seam;
the daemon `--mode daemon` supervisor.

Missing: `--mode rpc` and `--mode acp` exit with the typed
`MissingSubsystem::SessionEngine` error (`pa-cli/src/print_runtime.rs` L61) -
neither the RPC line protocol nor the ACP (Agent Client Protocol) server is
implemented (TS `modes/rpc/`, `modes/acp/`).

## 17. Session persistence - partial

Done: append-only JSONL session files with TS-parity entry sets (#88: session,
session_state, message, model_change, service_tier_change,
thinking_level_change, custom_message, compaction, agent_status),
checkpoint/restart recovery journal, status-line request parity (#81/#83).

Remaining: `toolResult` entries are never written - the persistence listener
records user/assistant messages only (`pa-core/src/session_engine/mod.rs`
L325-331 `persist_event`); affects `get_session_stats.toolResults`,
transcripts, and external tooling (checklist §8).

## 18. First-run onboarding - complete

Splash + "Share agent traces with Prime Intellect?" notice, answerable,
persisted completion flag (`pa-tui/src/onboarding.rs`;
`pa-tui/src/interactive.rs` L186-207). Battery-verified
(`runs/20260917T062810Z` f1: "first-run splash + trace-sharing notice
rendered and answerable on both sides").

## 19. Trace sharing - missing

The opt-in setting persists (`set_agent_traces_enabled`,
`pa-tui/src/interactive.rs` L62), but the trace upload subsystem
(TS `core/agent-traces.ts`: outbox, credential flow, session upload) does not
exist in Rust, and `/traces` (the command the onboarding note advertises) is a
client command without a UI (family 3).

## 20. Eval / verifiers Prime flow - missing

The Prime Intellect evals/verifiers integration surface has no Rust
implementation and no lane yet.

## 21. Native release / installer / CI - missing

The Rust binary does not ship the `prime-agent-runtime` kernel sidecar next
to the binary; battery and parity harnesses run with `PI_PACKAGE_DIR` pointing
at the installed TS release (checklist §9 note, `scripts/battery/batterylib.py`
`find_runtime_package_dir`). No native release manifests, installer, or
release CI (a reference sketch exists at `docs/ci.yml.reference`; merge gates
run locally via `make check`).

## 22. Platform readiness - in-flight

Windows-readiness audit and daemon-critical platform traits landed as docs +
trait seams (#72/#76, `docs/windows-readiness.md`); no shipping platform
support.

---

Cross-references: per-candidate detail and live-wire golden evidence live in
`docs/parity-checklist.md`; battery flow definitions in
`docs/parity-battery.md`; model-facing contract in `docs/MODEL-SURFACE.md`.
