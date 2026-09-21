# pa-tui porting notes (from TS prime-agent)

## Verifier
- tmux 80x24: run `pa-tui --resume <session.jsonl> --cwd <dir>`, capture pane, compare structure vs TS `prime-agent --offline --resume <same> --cwd <same>`.
- GT captures in ~/gt-captures (ts_collapsed/ts_expanded/ts_scrolled .txt/.ansi).

## Layout (fullscreen, default on)
- pin (top): TopBar = centered chat name (session name ?? basename(cwd)), `  $<cost.toFixed(2)>` dim after. name: text color; strip ctrl chars, collapse ws.
- scroll: headerContainer(splash), mainViewContainer[chat, shortcutGuide, pendingMessages, status], widgetAbove, [queued, sideQuestion], widgetBelow
- dock: promptDock[recapContainer, editorContainer, subagentSummaryLine, footerSlot]; FULLSCREEN_MIN_TRANSCRIPT_ROWS=3; dock height clipped to height-3.
- order in mainContainer: mainView, widgetsAbove, queued, sideQuestion, recap, editor, subagentSummary, widgetsBelow, footer.

## Editor
- promptPrefix "> ", paddingX 0; background surface (userMessageBg) => paddingX min(max(pad,2),maxPad).
- top border line: "─"*w or "─── ↑ N more " + "─"*(w-len) (borderMuted color); bottom similar ↓.
- cursor: reverse video (rendered by terminal; we output reverse style). maxVisibleLines = max(5, floor(rows*0.3)).
- history: unshift trimmed, dedupe if == history[0], cap 100. Up when empty OR browsing&firstVL -> navigateHistory(-1); down similar w/ last VL; at first VL (not browsing) -> moveToLineStart; at last VL -> moveToLineEnd.
- kill ring: push(prepend: backward deletes, accumulate if lastAction==kill); yank ctrl+y; yankPop alt+y rotates.
- undo: fish coalescing (word chars coalesce; space snapshots before itself); ctrl+- undo. submit clears stack.
- paste: bracketed paste; >10 lines or >1000 chars -> [paste #N +L lines]/[chars]; markers atomic; expanded on submit.
- word ops: whitespace/punct/word classes (PUNCTUATION_REGEX below).
- ctrl+u delete to start (kill ring prepend), ctrl+k to end (append), ctrl+w/alt+backspace word back, alt+d/alt+delete word fwd, ctrl+d delete fwd, shift+backspace=deleteCharBackward.
- enter: submit; shift+enter/\x1b\r/newline -> addNewLine; backslash+enter workaround (delete backslash, newline).
- tab: autocomplete; escape cancels; up/down navigate list; enter confirms (fall-through to submit if typed exact).
- bash mode: line starting `!` or `!!` -> promptPrefix "! "/"!! ", hidden prefix length (text hidden); border color bashMode.
- command token: leading /\s*/\/(\S+)/ with isArgumentCommand -> commandColor (accent).
- historyIndex -1 when any edit.

## Keybindings (defaults; see TS core/keybindings.ts + tui/keybindings.ts)
- editor: up/down/left/right, ctrl+b/ctrl+f left/right, alt+left|ctrl+left|alt+b wordLeft, alt+right|ctrl+right|alt+f wordRight, home|ctrl+a start, end|ctrl+e end, ctrl+] jump fwd, ctrl+alt+] jump back, pageUp/pageDown, backspace, delete|ctrl+d delFwd, ctrl+w|alt+backspace delWordBack, alt+d|alt+delete delWordFwd, ctrl+u delLineStart, ctrl+k delLineEnd, ctrl+y yank, alt+y yankPop, ctrl+- undo, shift+enter newline, enter submit, tab, ctrl+c copy(sel)
- app: ctrl+c app.clear(interrupt then exit on second), escape app.input.clear, ctrl+d app.exit(empty), ctrl+z suspend, ctrl+l model select, alt+m / shift+alt+m cycle, ctrl+o tools expand cycle(editor scope), alt+a subagents focus, ctrl+r heartbeats, ctrl+g external editor, ctrl+s stash, alt+enter followUp, alt+up/alt+down navigate older/newer, ctrl+alt+up/down move earlier/later, ctrl+v paste image, left agents.back, right agents.open/heartbeats.openSelected, space agents.reply, ? shortcuts.
- viewport: pageUp/pageDown scroll page, shift+alt+up top, ctrl+shift+down follow.
- select: up/down/pageUp/pageDown/enter confirm/escape,ctrl+c cancel.

## View behaviors
- ctrl+o cycles detail: overview -> details -> all -> overview. overview: hideThinking=true, toolExpanded=false, editDiffs=false; details: hideThinking=false, toolExpanded=false, diffs shown; all: expanded=true.
- status text right of recap line: "Collapsed mode (Ctrl+O to expand)" / "Details mode (...)" / "Expanded mode (Ctrl+O to collapse)"; from formatConversationDetailStatus + keyText (Ctrl+O).
- recap line (PromptContextLine): blank line then "Recap: <text>" dim, right side = status label; paddingX=1; recap collapses ws.
- tray info line (subagentSummaryLine.renderInfoLine): left = location label (agents hint/depth) or override (ctrl+c hint "Press Ctrl+C again to exit" / "Alt+Enter to queue message"), right = context label = goal + heartbeats + model joined " · ". muted color.
- goal label: "Pursuing goal (Xm Ys)"; heartbeat: "N heartbeats (Ctrl+R)"; model: modelId (provider prefix stripped):effortlower + " · N (P%)" context.
- subagents box: top "╭─ subagents ───...╮", body " ● N running   ◐ N idle   ○ N inactive" + right hint "↓ select" dim, "╰...╯" border color=accent; counts colors: success/warning/dim.
- working loader: line above status? in statusContainer (scroll area, after chat): "" + "<frame> <msg>" spinner accent, msg muted; frames ⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ @80ms; message "Thinking · Xs · ↓ Nk tokens" etc.
- queued messages: spacer + "» ..." TruncatedText.
- escape: 1st: interrupt or clear input (arm repeat window 500ms?); 2nd within window: "tree" (if nothing to interrupt) or "clear" input. ESCAPE_REPEAT_WINDOW_MS in TS (check). Ctrl+C: first show hint "Press Ctrl+C again to exit" (override label), second exits. Ctrl+D exits when empty.
- pageUp/PageDown scroll transcript by pageSize=window-1; wheel=3; following pauses on scroll-up; ctrl+shift+down re-follows; shift+alt+up top. Not-following hint appears at bottom-right? (check renderFullscreen pin area) — the capture shows "ctrl+shift+down to follow" hint inside transcript bottom line.

## Conversation rendering
- user msg: Box(paddingX=2,paddingY=1,bg userMessageBg) with markdown (color userMessageText) inside.
- assistant: Container: Spacer(1) if visible content; per content block Markdown(paddingX=1, color mdBody, thinking dim); spacer between thinking and next; abort/error Text error color px=1; trailing Spacer(1) if toolCalls && (visible || aborted || !precededByToolActivity).
- tool call: ToolPanel: header " <label> · <status>" on toolPanelBg (px=2); status: "done" success / "error" error / "◇ running" bashMode / "queued" muted; then blank panel line + children (renderers) each on panel bg. ipython uses IPythonCellComponent (self render, no panel).
- ipython collapsed line: " <marker> python · <preview> · ↑in ↓out lines · <dur> · <err>" marker ✓ success/✗ error/◇ queued/running pulse bashMode; language muted, preview dim.
- expand (all): full code with gutter "╰─ " dim + highlighted source, output " › " prefix then "   " lines; "no output" muted when empty.
- file diffs: "    ╰─ <path> +N -M" (path muted, counts toolDiffAdded/Removed); expanded rich diff rows.
- agent messages (child): spacer? then " ◆ Agent message received · <participant>" accent ◆, muted label, dim participant; expanded body " ╰─ ..." lines customMessageText.
- custom_message injected (goal_context/heartbeat/rlm_child_terminal_notice/harness_digest?): InjectedPromptMessageComponent — header muted "Goal continuation · <objective truncated>" + hint "(Ctrl+O to expand)"; expanded shows markdown body customMessageText px=1.
- async_bash_completion: ShellCompletionComponent: " ✓ Background shell command finished" muted / "✗ ... failed · exit N" error; expanded: header with pid+time + Text body px=1.
- refinement outcome: Spacer + " ◆ Harness refined" refinementHeader + EventSummary " <summary>" refinementSummary color px=1 (collapse ws, max 2 lines w/ " …").
- harness_digest custom_message w/ display:true → InjectedPromptMessage? customType not in list → falls to createDisplayedCustomMessageComponent (custom-message box). CHECK actual capture: harness_digest shows as "◆ Harness refined ..."? No — refinement_outcome renders that. harness_digest custom_message: check what component renders it (createDisplayedCustomMessageComponent switch).
- agent_status custom: no component (not displayed?) — check.
- spacing: createConversationSpacing: leading spacer for user/custom/etc when previous is not compact neighbor (agentmsg/toolexec/ipython/bash/shellcompletion); assistant handles own spacing.

## Theme (prime)
- fg via truecolor when supported else 256; fg reset \x1b[39m; bg reset \x1b[49m; bold 1/22, italic 3/23, underline 4/24, strike 9/29.
- key colors from prime.json (see file). empty "" = default terminal.
- mdBody #d8d8dc, mdHeading primarySoft #8d7fc0, mdLink info #38bdf8, mdCode #c8c8cd, mdCodeBlock stringMint #8ba888, mdQuote muted, mdQuoteBorder grid #52525b, listBullet muted, hr grid, borderMuted grid, accent primary #7c6faf, userMsgBg #1a1a1f, customMsgBg #151518, toolPanelBg surface #0d0d10, toolSuccessBg #0e1510, toolErrorBg #1a0d12, selectedBg #222226, thinkingText #8b8b94.

## Markdown theme mapping
- heading: fg(mdHeading); l1 adds bold+underline; l4 bold+italic; l5+ italic. paragraph: default color mdBody. code block: indent "  " + mdCodeBlock (or highlight). list: "- " bullet mdListBullet muted; nested "  ". blockquote: "│ " quoteBorder + italic quote color. hr: "─"*min(w,80). table: box drawing ┌─┬─┐ bold header. space token: "".
- strict strikethrough ~~x~~. inline math $..$ pandoc rules; block $$..$$ or \[..\].

## Select list (autocomplete)
- items prefix "› "/"  ", selected accent; metadata columns (slash cmds): primary width clamp(12..32 by widest+2), argumentHint mdCode, sourceTag dim, gap 2; scroll info "  ↑ N more  ↓ N more" muted; selected description wrapped below.
- maxVisible=5 (3..20).

## Session JSONL types (from ~/.prime/agent/sessions)
- session, model_change, thinking_level_change, service_tier_change, custom(thread_goal_state|prime-agent.refinement...), custom_message(harness_digest|goal_context|async_bash_completion|refinement_outcome|refinement_notice|rlm_child_terminal_notice|..., with display flag), message{message:{role:user|assistant|toolResult, content[], usage, stopReason,...}}, session_state, agent_status{status:{summary,taskState,basedOnMessageCount}}, git_state, child_usage_attributed.
- content block types: text, thinking(+thinkingSignature), toolCall{id,name,arguments}, image.
- toolResult: {role:"toolResult", toolCallId, toolName, content[], details{durationMs,status,stdout,stderr,...}, isError}.

## pa-tui lane notes

- `editor.rs` ports `packages/tui/src/components/editor.ts` faithfully: grapheme
  segmentation (unicode-segmentation), atomic `[paste #N]`/`[image #N]` markers,
  word wrap with wrap opportunities, sticky vertical column decision table,
  atomic-segment cursor snapping, kill ring (ctrl+k/u/w, alt+d, yank/yank-pop),
  fish-style undo coalescing, prompt history (up/down), jump mode (ctrl+]),
  backslash-enter newline, large-paste markers (>10 lines / >1000 chars).
- `keybindings.rs` carries the TS DEFAULT_* tables (tui + coding-agent app
  bindings, incl. `app.tools.expand` = ctrl+o); user overrides load from
  `~/.prime/agent/keybindings.json`.
- `markdown.rs` implements the markdown subset used in sessions (headings,
  paragraphs, fenced code, lists, quotes, hr, inline bold/italic/code/links,
  wrapping). Not yet ported: tables, math (latex), syntax highlighting.
- `session.rs` drives the same view from pa-types `FileEntry`s: `SessionStream`
  is the seam between JSONL replay and a future live daemon feed.
- `view.rs`/`app.rs` render the interactive layout: user-message background
  blocks, assistant markdown, tool panels (`⏺ name` header, toolPanelBg),
  `─` separator (dynamic-border parity), `> ` prompt, footer model label.

### Verifier
`crates/pa-tui/tests/tmux_replay.sh` renders a real captured session at 80x24,
captures the pane, and asserts the structural contract + editor keys
(type/backspace/arrows/ctrl+o/escape/ctrl+c). All checks pass.

### Ambiguities
- The TS `prime-agent resume <id>` flow opens the agents view and spawns live
  activity rather than statically replaying history, and `-r <path>` fails to
  spawn a session worker inside this sandbox (EACCES). The tmux differential
  therefore asserts the pa-tui structure against TS UI captures (separator,
  prompt, footer, collapsed-mode line) instead of a same-session side-by-side.
- Thinking blocks are excluded from replay transcripts (TS renders them
  collapsed); `--show-thinking` placeholder flag retained for a future port.


## pa-ai / real-provider lane notes

- `PRIME_INFERENCE_BASE_URL` now mirrors the TS reference exactly: the TS
  binary's provider config (`packages/coding-agent/src/core/prime-inference-model-catalog.ts`)
  declares `https://api.pinference.ai/api/v1`; the old Rust value
  (`https://inference.primeintellect.ai/v1`) does not resolve on this box. The
  generated catalog (`pa-ai models.generated.json`) already carried the pinference
  URL; only the private-model / live-catalog constant and the catalog-refresh
  fetch diverged. A differential test reads the TS source as the golden.
- `pa_agent::types::UserPart` was `serde(untagged)`, but the TS wire format
  (`packages/ai/src/types.ts` `UserMessage.content`) is `type`-tagged parts
  (`{"type":"text", ...}`). The untagged form failed the pa-agent -> pa-ai
  JSON round-trip in `real_stream_fn`, so every prompt admitted through
  `AgentPromptInput::Text` was silently dropped before reaching the provider.
  `session_engine::provider_adapter` has a regression test for the boundary.
- `AgentSession::prompt` eagerly appended the user message to the session;
  the TS reference persists user prompts only from the agent `message_end`
  event (`_processAgentEvent`). The eager append double-persisted once the
  wire shape was fixed; it was removed (in-memory persistence parity verified
  by the existing `prompt_persists_user_and_assistant` test).

## package-manager lane notes

- Source parsing (`crates/pa-core/src/packages/source.rs`) ports
  `core/utils/git.ts` `parseGitUrl` + `utils/paths.ts` `isLocalPath`. The
  hosted-git-info dependency is covered by a subset (shortcut prefixes
  `github:`/`gitlab:`/`bitbucket:`/`gist:`, known domains, `git+` schemes,
  `#committish`); the generic fallback already covers every other host, so
  the observable behavior matches the TS on all documented forms.
- Product quirks preserved deliberately (all verified against the TS binary):
  - `git://host/path` parses as a LOCAL path (the `git:` prefix is stripped
    before the protocol check), so `package install git://...` reports
    "Path does not exist".
  - `github.com/user/repo` shorthand is local without the `git:` prefix and
    git with it.
  - Local settings entries store paths relative to their settings base
    (agent dir / project config dir), so `package remove` only matches by
    equivalent resolved identity - cwd-relative and settings-relative forms
    of the same stored entry do NOT match each other.
  - Update order: npm version probes run first, then one batched npm install
    per scope (`install -g pkg@latest`), then git fetch/reset/clean. Update
    is sequential in this port; the TS runs probes at concurrency 4, which
    only affects wall time, not output.
- Settings writes use a field-scoped read-merge-write under the settings
  lock (`persist_scope_field`), matching the TS `persistScopedSettings`
  (fields added to the file after this manager loaded survive); a scope
  whose settings file failed to parse is never written (the TS load-error
  guard).
- CLI output parity is plain-text: pa-cli carries no color layer (chalk
  levels are dropped when piped anyway), so transcript comparisons with the
  TS binary use piped output, which strips chalk colors.
- Self-update (`prime-agent update`) is a typed boundary: native release
  manifests + daemon update-restart coordination are a separate lane.
  `package update` (extensions-only) completes fully.
- Differential verifier: `crates/pa-cli/tests/package_e2e.rs` uses an
  embedded-path npm shim via the `npmCommand` setting and an ssh shim
  (GIT_SSH_COMMAND) mapping `ssh://localhost/...` onto a local bare repo -
  the only git transports the source parser accepts are https/ssh/git, so a
  local fixture needs the ssh shim (git daemon `git://` URLs are not
  parseable sources).

## Side questions / provider-retry lane notes

- Provider retry policy (TS `core/provider-retry.ts`) lives in
  `pa-core::session_engine::provider_retry`: pure decision functions
  (`provider_retry_delay`, `is_permanent_provider_failure_kind`, lifecycle /
  faux-queue checks) plus the retryable one-shot driver
  `complete_with_provider_retry` with an injectable wait future. The wait is
  injectable so the scripted engine can drive it under a plain `futures`
  executor while the real engine races it against an `AbortSignal` on tokio.
- `start_side_question` / `abort_side_question` (TS daemon-mode L4730) map onto
  the worker `SessionEngine` trait as one extra engine call
  (`run_side_question`): the worker owns the run registry (one live run per
  client per session, TS error strings), the abort controller, and the
  `side_question_event` frames; the engine owns the turn behavior.
- The side-thread clone (TS `core/side-question.ts`) is
  `pa-core::session_engine::side_question::run_side_question`: it re-clones the
  live conversation per turn (same system prompt, model, thinking level, and
  tool declarations so the provider KV-cacheable prefix is preserved), replays
  `previousTurns` after the clone, blocks tool execution via `before_tool_call`,
  caps the run at 3 turns, and streams partial answers to a caller sink. The
  retry loop is inlined there because streaming must interleave with attempts
  (the generic driver stays available for one-shot consumers).
- Statuses follow the TS `SideQuestionStatus` vocabulary:
  `running` / `complete` / `cancelled` / `error` (not "completed"/"aborted").
- Event delivery parity: the live TS supervisor (0.9.5) fans every worker
  outbound - including `side_question_event` - out to the clients *attached*
  to the session (`daemon-supervisor.ts` `handleWorkerFrame` skips clients
  without the session in `attachedActiveSessionIds`). Verified live: an
  unattached client gets the `start_side_question` success response but no
  events; after attach the events arrive. The Rust supervisor reproduces this
  (side-question frames ride the same attached-session routing as
  session events).
- Known deviations: (1) the real engine (`AgentSessionEngine`) uses the
  default retry policy (settings `retry.*` wiring is a follow-up); (2) a
  model-resolution failure surfaces as an `error` side-question event rather
  than the TS synchronous "Select a model before asking a side question"
  command failure (the TS worker returns the start response before the engine
  call in the Rust redesign); (3) the worker keys runs by the client id the
  supervisor injects into the routed command (the TS single-process daemon
  compares socket objects; under the supervisor split its observable behavior
  matches for the single-client flow).
origin/main

## Chunked snapshot streaming lane notes

- Chunked attach (`chunked_snapshot` capability) is
  `pa-daemon/src/snapshot_stream.rs`: the supervisor rewrites the worker's
  attach result the way TS `createStreamedAttachResult` does - the snapshot
  keeps an empty `messages` array, the top-level `messages` copy is dropped
  (slim results never had one), and a `snapshotStream` descriptor
  (`{id, messageCount, targetChunkBytes}`) is added - then emits
  `session_snapshot_begin` / `session_snapshot_chunk` /
  `session_snapshot_end` records after the response.
- Byte budget is TS `SNAPSHOT_TARGET_CHUNK_BYTES` (512 KiB, not the ~256 KiB
  the task sketch guessed): each chunk record's `messages` array is
  serialized compact and flushed before exceeding the budget; a single
  oversized message travels alone and is never split.
- Snapshot id parity: the live TS daemon names it
  `<activeSessionId>-<generation>-<sequence>` from the event cursor
  (`daemon-mode.ts` `snapshotTransferId`), e.g. `d3ad819c5e92-427baf366601-567`.
  The supervisor-side sha256 revision in `daemon-supervisor.ts`
  `getOrCreateTranscriptCache` is the fallback for workers that do not
  stream; since the Rust worker returns full snapshots, the cursor format is
  the one clients actually see on the wire and is what the Rust port uses.
- `purpose` on `session_snapshot_begin` is `attach` for attach and
  `replacement` for reattach; the TS catch-up purpose value is `resync` on
  the wire, so `pa_types::daemon::SnapshotPurpose::Catchup` now serializes
  as `resync` (was a latent `catchup` wire divergence).
- Design deviation: the Rust supervisor materializes the whole snapshot
  before writing the streamed response (TS streams chunks asynchronously
  after the response with abort controllers and transcript reservations).
  The synchronous dispatch keeps the same client-visible record order and
  makes mid-command aborts impossible; a malformed worker transcript
  surfaces as `session_snapshot_failed` after the response (TS: transcript
  cache failure mid-stream), and a snapshotless worker payload fails the
  attach itself before any record exists (TS: `attachClient` throws).
- `session_snapshot_failed` for genuine mid-stream aborts (TS aborts the
  transfer on `session_closed`) does not occur: the supervisor's write loop
  breaks the connection on socket errors exactly when TS destroys it.
- The attach result now echoes the client's own (normalized) capability
  set, matching the live TS golden; a capability-less client sees
  `["attach_snapshot","event_sequence"]`.

HEAD
## Thin-supervisor stage 2 (direct-attach transport) lane notes

- Shared wire mechanics moved to pa-types because pa-tui (pa-types only) must
  speak the worker socket as a direct-attach client: the private-frame codec
  (`daemon::framing`, served from pa-daemon as a re-export), the command-plane
  table (`daemon::plane`, TS `DAEMON_COMMAND_PLANE` verbatim - the worker gates
  peer links with it and the routed client picks the socket with it), and the
  platform socket-identity stat (`platform::identity`). The TS peer-grant and
  ticket wire shapes (`DaemonWorkerPeerGrant`, `DaemonPeerCommand`,
  `DaemonPeerTransportTicket`) already lived there.
- Ticket issuance (`peer_tickets.rs`, TS `issuePeerTransport`): the supervisor
  resolves a registered session, requires a ready/connected/peer-capable worker
  (capability captured from the worker's `worker_auth` response,
  TS `workerAuthAdvertisesPeerTransport`), pins the exact worker instance id
  and socket-filesystem identity (dev+ino), mints a single-use grant with the
  TS `PEER_TRANSPORT_GRANT_TTL_MS` = 10s TTL, pushes it into the worker
  (`worker_register_peer_transport`, 3s round trip), and returns the ticket.
  Deviations, documented in code: (1) the process-identity check is live-pid
  liveness rather than TS's `processStartId` pin (the Rust supervisor never
  populated `process_start_id`); (2) the client-owned-worker refusal has no
  Rust equivalent (every spawned/adopted worker is a resident session, and
  `owner_client_id` today records the creating client for all workers - a TS
  semantic that predates this lane); (3) the grant token is one v4 UUID's hex
  (122 bits, single-use + 10s TTL) instead of TS's 32 random bytes in
  base64url; comparison is sha256-then-constant-time like TS
  `timingSafeEqual`.
- Worker grant store (`peer.rs`, TS `peerGrants` + `peer_auth` +
  `worker_register_peer_transport`): grants live in worker memory only, burn
  on first use BEFORE the token is checked (a failed presentation also burns),
  expire at their TTL (registration rejects grants expiring more than 30s out,
  `PEER_GRANT_TTL_LIMIT_MS`), and are capped at 1024 after an expired sweep.
  Registration additionally validates the grant's `issuerGeneration` against
  the authenticated supervisor connection's generation (TS compares against
  the `boundClaim`).
- Worker connection roles: `worker_auth` promotes a connection to
  `Supervisor` (full command set, always streams events);
  `peer_auth` promotes to `SessionClient` (session-plane commands for the
  grant's session only - the TS `peerClaims` gate with the exact TS failure
  string "Command is not allowed on this direct peer transport"). Event
  fan-out is now role-gated: unauthenticated connections never receive the
  session stream, and a session client streams only while it holds an attach
  (its `attach` succeeded, `detach` stops the stream) - TS streams to
  `state.clients`, not to every socket.
- Client routed transport (`pa-tui/src/direct_transport.rs` + `DaemonClient`,
  TS `daemon-routed-client.ts`): `upgrade_direct` is TS
  `createDaemonSessionTransport` - require the `direct_peer_transport`
  capability, request `get_direct_worker_transport` (5s, no recovery), validate
  the ticket (shape, target session, freshness, socket identity re-stat),
  connect (1s), read hello, `peer_auth` (3s). Any failure silently keeps the
  supervisor-routed path (transition-period fallback). Session-plane commands
  for the link's session then ride the worker socket; control stays on the
  supervisor. A dead link discovered before the frame is queued falls back to
  the supervisor; a sent request that times out surfaces the error and is
  never retried (no double execution of prompts, TS comment parity). A failed
  direct attach retries once over the supervisor (TS
  `DaemonAgentConnection.attach`), and switching sessions drops the link (a
  grant is bound to one session).
- Direct attach commands are stamped with the client id and the same
  slim-snapshot capability set the supervisor's routed attach injects
  (`["attach_snapshot","event_sequence","slim_attach"]`), so both paths return
  identical attach results. The TS routed client sends its own raw
  capabilities (including `chunked_snapshot`); the Rust worker returns full
  snapshots on the direct path, so the slim set is the honest contract for now.
- e2e (`tests/direct_attach_e2e.rs`): ticket field/ttl assertions, grant
  single-use (replay rejected with the TS string), control-plane denial on a
  peer link, mid-stream kill -9 of the supervisor with the direct stream
  continuing, supervisor restart + roster rebuild + fresh ticket + reattach +
  second scripted turn, and live grant expiry after the 10s TTL.


## Thin-supervisor stage 3 (peer messaging) lane notes

- TS has no worker-to-worker peer transport for agent messages: the TS worker
  routes kernel sends supervisor-mediated (`sendRemoteAgentSessionMessage`
  -> supervisor `send_message` -> `worker_deliver_message`). Stage 3 extends
  the stage-2 ticket machinery with `worker`-purpose single-use grants so the
  delivery bypasses the supervisor's route plane; TS parity holds for every
  user-visible shape (sender identity, rendered prompt, receipt).
- Kernel `agent_message.send` contract ported from TS
  `createAgentMessageHostHandlers` (core/agent-messages.ts): the runtime
  skill sends `{message, receiver_role, receiver_name}` (or `target:"all"`),
  never a positional target - the old Rust handler expected `{target}` and
  always failed against the installed runtime. Role/name resolution goes
  through the family roster; the thin supervisor's family is the supervisor
  `list` roster (every other resident session is a sibling). In-worker
  parents/children and the TS family catalog (`selectAgentFamily`,
  `awaitPendingChildPublication`) are deferred with the catalog itself.
- Direct delivery semantics: the peer ticket burns on first use, so a send
  is never retried once the `worker_deliver_message` command went out - a
  failed/refused/lost delivery surfaces as the error; only pre-delivery
  failures (no ticket, connect failure, failed grant burn) fall back to the
  supervisor-routed `send_message` (the TS path, kept verbatim).
- Sender identity: TS renders the sender from the sending session's live
  summary; the worker pushes its summary to the engine at create/rename and
  the direct path builds `{activeSessionId, sessionId, sessionName?,
  runtimeKind, clientId: "agent"}` (the TS agent-origin sender shape when no
  client id is in play). The supervisor-routed fallback keeps the supervisor
  building the sender from the source worker's `get_state` (TS behavior).
- Supervisor-link restart window: TS tears the link down through its
  DaemonClient close listener, so the next request reconnects. The Rust link
  discovers death lazily, so a write-phase failure (the command never reached
  the supervisor) reconnects and retries exactly once; read-phase failures
  never retry (the command may have been processed). Verified by the
  supervisor kill -9 e2e in tests/peer_messaging_e2e.rs.

## Daemon model selection + status line (cli-flags-parity lane)

- TS `main.ts` `runtimeConfigFromArgs` builds `AgentSessionRuntimeConfig`
  (cwd, provider, model, apiKey, ...) and every daemon `create` carries it
  (`daemon-protocol.ts` `create.config`). Ported: `pa-tui` forwards
  `ModelSelection` (provider/model/apiKey) in the create config; the
  supervisor persists it into the durable create command so respawned workers
  resolve the same model; the worker binds it onto its engine
  (`SessionEngine::configure_model`), which treats explicit flags as
  authoritative over the process env fallback
  (`PRIME_AGENT_MODEL_PROVIDER`/`PRIME_AGENT_MODEL` remain the fallback when a
  create carries no flags).
- TS API-key precedence for worker request auth (`main.ts`
  `setRuntimeApiKey` for `--api-key`, `model-registry.ts`
  `getApiKeyAndHeaders`): create-config key, then auth storage, then the
  models.json provider `apiKey`; the pa-ai provider env-key map stays the
  last resort inside the provider. Ported in `agent_engine.rs`
  (`resolve_request_api_key`); custom provider names (no env mapping) now
  authenticate from models.json.
- TS daemon-session-summarizer.ts (status line): after each completed turn
  (`turn_end`/`compaction_end` broadcast, 2s debounce) and every 25s sweep
  for working sessions, the daemon asks a small model
  (prime-inference/qwen/qwen3-30b-a3b-instruct-2507) for a dashboard recap:
  fixed system prompt, `<agent-state>` + trailing-8-message conversation
  body, max_tokens 400; result broadcast as `session_status` with the recap
  text. Ported in `pa-daemon/src/status_line.rs`, including the settled
  idle verdict persistence to the session journal (`appendAgentStatus`):
  real model classifications and transcript error verdicts
  (`terminalTurnError` -> `taskState: "error"`) persist as `agent_status`
  entries, the needs_input fallback never grows the journal, and a
  respawned worker seeds its in-memory verdict from the latest persisted
  entry (`DaemonSessionSummarizer.seed`).
- TS print-mode `-c`/`-r` active-session guard (`session-lease.ts`
  `SessionAlreadyActiveError` -> supervisor `assertWorkerCreateOwner` on the
  daemon create): a headless continue/resume refuses when the target session
  file is live in the daemon ("Session is already active in <id>: <path>").
  Ported in the Rust print path as a daemon-list probe before opening the
  file (the Rust print path runs in-process, not over the daemon, so the
  guard probes the supervisor's live roster first). The supervisor-level
  reuse-vs-guard semantics of TS `createOrReuseWorker` (same owner reuses,
  different owner refuses) and interactive resume's pre-resolution to attach
  are not ported yet — a separate lane item.origin/main

## Worker robustness + session entry parity (worker-robustness lane, B-3/B-8)

- AF_UNIX over-limit paths: the installed TS product survives deep-TMPDIR
  worker sockets because its runtime (Bun) transparently re-anchors long unix
  socket paths through an O_PATH directory fd — an strace of the live worker
  shows `bind(13, {sa_family=AF_UNIX,
  sun_path="/proc/self/fd/12/worker-....sock"}, 110) = 0`, and the socket
  file lands at the original deep path. No TS source references this; it is
  purely runtime behavior (Node fails the same bind with `EINVAL`). The
  Rust port puts the same rewrite in `pa_types::platform::transport`
  (`UnixSocketAddress`): paths within the 107-byte `sun_path` limit pass
  through; longer ones (Linux) open the parent directory with `O_PATH` and
  bind/connect via `/proc/self/fd/<fd>/<file name>`, keeping the fd open for
  the address lifetime. Non-Linux Unix platforms surface the natural
  path-length error. Filesystem cleanup (unlink/chmod) always uses the
  original path (no 108-byte limit there).
- Worker connect budget: TS `WORKER_CONNECT_TIMEOUT_MS` is 30s on Unix
  (90s Windows) and covers probes (500ms), connect, and the auth handshake
  under one deadline (`connectWorker` + `handshakeBudgetMs`); a worker that
  never comes up throws `DaemonWorkerProbeTimeoutError` and the failed
  launch stops the child. The Rust supervisor uses the same 30s shared
  deadline (`worker_connect_deadline`), probes every 25ms (TS backoff
  min=max=25ms), kills the spawned child when the deadline trips, and
  bounds the auth request to the remaining budget.
- `service_tier_change` entries (TS `sdk.ts` `createAgentSession`): fresh
  sessions append `model_change` + `thinking_level_change` +
  `service_tier_change` (settings default, TS `getDefaultServiceTier`
  fallback `"default"`); resumed sessions append thinking and service tier
  only when no earlier entry set them. Ported in the pa-core engine
  creation path (engine-owned session files) and mirrored onto the daemon
  worker's own session file at create.
- Queue snapshots: the Rust worker used to persist steering/follow-up lanes
  as `custom` session-file entries (`prime-agent-rs.queue_snapshot`) —
  a Rust-only entry type that broke the B-8 shape diff. TS keeps session
  files free of daemon bookkeeping and journals worker-private state
  separately, so the snapshot moved into the worker recovery journal
  (`WorkerRecoveryJournal::record_queue_snapshot`, latest-wins per session,
  survives journal compaction).
- `agent_status` entries (TS `daemon-session-summarizer.ts`
  `appendAgentStatus`): settled idle verdicts persist as `agent_status`
  session entries — real model classifications and transcript error
  verdicts (`terminalTurnError` → `taskState: "error"`, recap
  "Model request failed: …") — never the needs_input fallback, sweeps, or
  working refreshes (no verdict), and only when the verdict differs from
  the latest persisted one. Respawned workers seed the in-memory status from
  the latest persisted entry (`seed`) so a restart does not re-ask the
  model. Persisted shapes match the live-session goldens
  (`{"type":"agent_status",…,"status":{"summary","taskState",
  "basedOnMessageCount"}}`).

- Slash-command dispatch (PR: registry + dispatch + autocomplete): the
  builtin command table is pure data in `pa-types::slash_commands` (the TUI
  cannot import pa-core; pa-core re-exports it for the session engine, and
  pa-daemon consumes it through pa-core). The TUI client dispatch runs
  local handlers first (`/help` `/list` `/switch` `/exit` are Rust-client
  dev commands, not TS builtins), then the registry: `SlashCommandExecution`
  splits client commands (locally implemented subset: `/new` incl. the
  no-argument `/clear` alias, `/quit` → detach-and-exit; everything else
  reports `/{name} is not available in this client yet` — there is no exact
  TS string for an unimplemented client command, so the repo's
  `not available in this build yet` convention is used) from session
  commands (`/compact` `/refine` `/goal` `/autonomous` forward verbatim as
  prompts; the worker parses them before admission and never runs a model
  turn). Unknown names reproduce the TS suggestion error exactly
  (`Unknown command: /x. Did you mean /y?` via the shared
  `find_slash_command_suggestion`); names over 64 chars and names without a
  suggestion pass through as prompts, like the TS product.
- Slash autocomplete (port of `packages/tui/src/autocomplete.ts` +
  `select-list.ts` + `fuzzy.ts`): the editor installs a
  `CombinedAutocompleteProvider` (registry commands via the fuzzy filter,
  plus the readdir-based file/path completion with TS `extractPathPrefix`
  trigger rules) in `Editor::new`. The TS product's `@`-attachment
  completion is fd-backed; this build has no fd dependency, so `@` tokens
  yield no suggestions — the same behavior as the TS product without fd on
  PATH. `getSlashCommandContext` is a full port (prompt-start command vs
  argument position, mid-line `/token` contexts). The dropdown renders
  above the editor on the toolPanel popup background with the TS slash
  layout (primary column clamped 12–32, argument-hint column, directional
  scroll info, selected-item description below the list).
- Slash echo/result rows (port of `slash-command-message.ts` +
  `slash-command-result-message.ts`): the durable `session_slash_command` /
  `session_slash_command_result` custom rows render as user-message blocks
  (`Box(2,1)` on `userMessageBg`), the echo with the `/name` token in
  `accent` and `@path`/`--flag` argument tokens highlighted (`success` /
  `mdLink`), a leading spacer when the chat is non-empty; non-display rows
  (the `/refine` result) and unknown custom types render nothing; a
  session-command custom row with an invalid payload renders the
  `[Malformed session command message]` notice. The OSC 133 prompt markers
  the TS components emit are not ported (the Rust TUI draws no OSC zone
  markers yet).
- Verification seam: a plain `{"responses": [...]}` script uses the echo
  `ScriptedEngine`, whose `run_prompt` does not parse session commands —
  use the `{"engine": "faux", "responses": [...]}` script form to drive the
  real agent engine (with session-command admission) in TUI e2e tests.


## Eval/verifiers composition (eval-composition lane)

- TS inventory: the coding-agent has no `eval` command. Verifier flows ride
  the autonomous gate mechanism (TS `core/autonomous.ts`, ported in #98) plus
  headless modes: print (`-p` / non-TTY) and `--mode json` stream
  `session_event` frames on stdout; ACP carries `_meta.autonomous` per
  completion. `prime eval ...` on PATH is the Prime Intellect platform CLI
  (a separate product, hosted evals over verifiers environments) — outside the
  coding-agent parity surface; recorded in docs/completion-matrix.md family 20.
- Headless autonomous loop (pa-cli `headless_autonomous.rs`, port of
  `modes/print-mode.ts` + the gate half of `headless-completion.ts`): CLI
  autonomous flags build `AutonomousRuntimeState` (TS
  `runtimeAutonomousConfigFromArgs`: any autonomous flag enables the run);
  a subscription forwards every settled assistant message to the driver
  (per-message accounting); after each settled prompt the driver decides —
  gate-failure continuation injected as the next turn (a durable user row),
  or stop, persisting the `autonomous_status` stop row into the session and
  emitting it as `message_start`+`message_end` events (the daemon worker's
  custom-row wire shape).
- Exit-code contract (print-mode.ts): a configured gate still failing after
  its retry window (with or without a limit) -> stderr
  `Autonomous quality gate still failing after attempt N/M: <exit text>[;
  autonomous limit reached: ...]` + exit 1; an autonomous run without gates
  stopped by a limit -> stderr `Autonomous run stopped before terminal
  evidence; <limit reached (used/cap)>` + exit 1; gate pass -> exit 0. The
  contract applies to both text and json output modes; the stop row never
  prints to stdout in text mode.
- Scope cut vs TS: `waitForHeadlessCompletion`'s host-side gate-failure
  re-injection after an errored model turn (TS retries failing gates even
  when the turn ended `error`/`aborted`) is not ported yet; the per-turn
  driver loop covers the gate-failed and limit paths. Same cut as the ACP
  transport.
- Verifier: `crates/pa-cli/tests/eval_composition_e2e.rs` drives the built
  binary with fixture verifier scripts (counter-file verifier + always-fail
  verifier) over the faux provider in isolated HOMEs — no network, no daemon
  sockets.


## Passive-RLM roster + ledger (roster-wire lane)

- `crates/pa-daemon/src/rlm_ledger.rs` (port of
  `modes/daemon/rlm-ledger.ts`): the daemon-owned RLM spawn ledger, one
  append-only JSONL per sessions dir (`<agent-dir>/rlm-ledger/<sha256-16-of-canonical-dir>.jsonl`),
  same record grammar (v:1 meta/spawn/rename/delete, unknown-op forward
  compat, version violations fail closed), same read bounds (32MiB /
  100k records), stat-guarded replay cache keyed on size+mtime+inode, and
  legacy per-parent `rlm-subagents.jsonl` seeding with the atomic
  no-clobber hard-link publish. The rename/delete record join falls back
  to a sole childId edge (the symlink-retarget case). Per-child display
  files (`rlm-subagent.json`) port `rlm-subagent-display.ts` including the
  deleted-tombstone refusal.
- `crates/pa-daemon/src/rlm_roster.rs` (port of
  `walkPassiveRlmSubagents` + `withPassiveRlmDescendantInfos`): the
  passive roster walk roots at every saved session file plus every resident
  session file; live ledger edges group children by canonical parent path;
  resident children contribute no row (their row comes from the live
  registry) but stay walk roots for their own children; every other live
  child's file is read for display data with the edge as the only topology
  authority (fork headers never trusted). Row shapes match the TS
  `buildSessionListWithPassiveRlmSubagents` enrichment (runtimeKind
  subagent, rlmChildId, parentSessionPath, parentActiveSessionId when the
  direct parent is resident, rlmParentNodeId fallback to the childId,
  spawnCode from display/legacy metadata) and `inactiveLifecycleForSession`.
- Supervisor admission moments mirror the TS daemon: `recordRlmSubagentState`
  (spawn edge + display entry at create, admission fails if the spawn record
  cannot be made durable), `recordRlmSubagentDeletion` (delete tombstones
  BEFORE teardown; the parent-side `delete_subagent` kill carries an
  `rlmLedgerDelete` marker so a plain `stop` never tombstones), and
  `appendRlmLedgerRenameForState` (rename by child path, offline-safe).
- `list --all` now composes TS `buildSessionList`: saved rows first with
  resident replacements in place, then passive children, then resident-only
  rows; a broken ledger fails the list command (TS propagates the walk
  error) while the saved-session catalog degrades to the saved rows
  (TS `withPassiveRlmDescendantInfos` catches).
- Mechanism difference: TS memoizes the passive walk with stat
  fingerprints (`passiveRlmSubagentMemo`); the Rust supervisor reads the
  ledger behind its own stat guard and re-walks per list. `family()` /
  `siblings()` ledger reads are not needed by any Rust surface yet and were
  not ported.
- Verifier: `crates/pa-daemon/tests/rlm_roster_walk_e2e.rs` — a synthetic
  1,000-child ledger plus persisted child files yields the full 1,001-row
  `list --all` roster against the real supervisor, performance-bounded
  (TS reference: 1,001 rows in 0.78s).

## Provider wire diff (roster-wire lane)

- Field-by-field request-body diff against the TS binary (same transcript
  replayed on both, mock provider capturing bodies; see the PR body table):
  message serialization is byte-size identical for user/assistant rows,
  tool calls, tool results, and cross-model thinking-as-text replay; the
  only systematic differences are the layered system prompt (by design)
  and the single-text-block user row shape (TS content array vs Rust plain
  string, -28 bytes/row, same text).
- One real omission found and fixed: Rust compared harness-digest
  staleness against ALL session entries
  (`latest_digest_from_entries(get_all_entries())`), so a digest stored
  before a refinement/compaction context boundary suppressed re-delivery
  even though it was no longer in the loop context. TS
  `_latestContextHarnessDigest` scans `agent.state.messages` only, by
  timestamp. Ported as `latest_context_digest` (frame-scoped, timestamp
  recency); a refinement-boundary resume now re-injects the
  `[harness-digest]` user row exactly like TS (verified end-to-end against
  the TS binary).
# pa-tui porting notes (from TS prime-agent)

## Verifier
- tmux 80x24: run `pa-tui --resume <session.jsonl> --cwd <dir>`, capture pane, compare structure vs TS `prime-agent --offline --resume <same> --cwd <same>`.
- GT captures in ~/gt-captures (ts_collapsed/ts_expanded/ts_scrolled .txt/.ansi).

## Layout (fullscreen, default on)
- pin (top): TopBar = centered chat name (session name ?? basename(cwd)), `  $<cost.toFixed(2)>` dim after. name: text color; strip ctrl chars, collapse ws.
- scroll: headerContainer(splash), mainViewContainer[chat, shortcutGuide, pendingMessages, status], widgetAbove, [queued, sideQuestion], widgetBelow
- dock: promptDock[recapContainer, editorContainer, subagentSummaryLine, footerSlot]; FULLSCREEN_MIN_TRANSCRIPT_ROWS=3; dock height clipped to height-3.
- order in mainContainer: mainView, widgetsAbove, queued, sideQuestion, recap, editor, subagentSummary, widgetsBelow, footer.

## Editor
- promptPrefix "> ", paddingX 0; background surface (userMessageBg) => paddingX min(max(pad,2),maxPad).
- top border line: "─"*w or "─── ↑ N more " + "─"*(w-len) (borderMuted color); bottom similar ↓.
- cursor: reverse video (rendered by terminal; we output reverse style). maxVisibleLines = max(5, floor(rows*0.3)).
- history: unshift trimmed, dedupe if == history[0], cap 100. Up when empty OR browsing&firstVL -> navigateHistory(-1); down similar w/ last VL; at first VL (not browsing) -> moveToLineStart; at last VL -> moveToLineEnd.
- kill ring: push(prepend: backward deletes, accumulate if lastAction==kill); yank ctrl+y; yankPop alt+y rotates.
- undo: fish coalescing (word chars coalesce; space snapshots before itself); ctrl+- undo. submit clears stack.
- paste: bracketed paste; >10 lines or >1000 chars -> [paste #N +L lines]/[chars]; markers atomic; expanded on submit.
- word ops: whitespace/punct/word classes (PUNCTUATION_REGEX below).
- ctrl+u delete to start (kill ring prepend), ctrl+k to end (append), ctrl+w/alt+backspace word back, alt+d/alt+delete word fwd, ctrl+d delete fwd, shift+backspace=deleteCharBackward.
- enter: submit; shift+enter/\x1b\r/newline -> addNewLine; backslash+enter workaround (delete backslash, newline).
- tab: autocomplete; escape cancels; up/down navigate list; enter confirms (fall-through to submit if typed exact).
- bash mode: line starting `!` or `!!` -> promptPrefix "! "/"!! ", hidden prefix length (text hidden); border color bashMode.
- command token: leading /\s*/\/(\S+)/ with isArgumentCommand -> commandColor (accent).
- historyIndex -1 when any edit.

## Keybindings (defaults; see TS core/keybindings.ts + tui/keybindings.ts)
- editor: up/down/left/right, ctrl+b/ctrl+f left/right, alt+left|ctrl+left|alt+b wordLeft, alt+right|ctrl+right|alt+f wordRight, home|ctrl+a start, end|ctrl+e end, ctrl+] jump fwd, ctrl+alt+] jump back, pageUp/pageDown, backspace, delete|ctrl+d delFwd, ctrl+w|alt+backspace delWordBack, alt+d|alt+delete delWordFwd, ctrl+u delLineStart, ctrl+k delLineEnd, ctrl+y yank, alt+y yankPop, ctrl+- undo, shift+enter newline, enter submit, tab, ctrl+c copy(sel)
- app: ctrl+c app.clear(interrupt then exit on second), escape app.input.clear, ctrl+d app.exit(empty), ctrl+z suspend, ctrl+l model select, alt+m / shift+alt+m cycle, ctrl+o tools expand cycle(editor scope), alt+a subagents focus, ctrl+r heartbeats, ctrl+g external editor, ctrl+s stash, alt+enter followUp, alt+up/alt+down navigate older/newer, ctrl+alt+up/down move earlier/later, ctrl+v paste image, left agents.back, right agents.open/heartbeats.openSelected, space agents.reply, ? shortcuts.
- viewport: pageUp/pageDown scroll page, shift+alt+up top, ctrl+shift+down follow.
- select: up/down/pageUp/pageDown/enter confirm/escape,ctrl+c cancel.

## View behaviors
- ctrl+o cycles detail: overview -> details -> all -> overview. overview: hideThinking=true, toolExpanded=false, editDiffs=false; details: hideThinking=false, toolExpanded=false, diffs shown; all: expanded=true.
- status text right of recap line: "Collapsed mode (Ctrl+O to expand)" / "Details mode (...)" / "Expanded mode (Ctrl+O to collapse)"; from formatConversationDetailStatus + keyText (Ctrl+O).
- recap line (PromptContextLine): blank line then "Recap: <text>" dim, right side = status label; paddingX=1; recap collapses ws.
- tray info line (subagentSummaryLine.renderInfoLine): left = location label (agents hint/depth) or override (ctrl+c hint "Press Ctrl+C again to exit" / "Alt+Enter to queue message"), right = context label = goal + heartbeats + model joined " · ". muted color.
- goal label: "Pursuing goal (Xm Ys)"; heartbeat: "N heartbeats (Ctrl+R)"; model: modelId (provider prefix stripped):effortlower + " · N (P%)" context.
- subagents box: top "╭─ subagents ───...╮", body " ● N running   ◐ N idle   ○ N inactive" + right hint "↓ select" dim, "╰...╯" border color=accent; counts colors: success/warning/dim.
- working loader: line above status? in statusContainer (scroll area, after chat): "" + "<frame> <msg>" spinner accent, msg muted; frames ⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ @80ms; message "Thinking · Xs · ↓ Nk tokens" etc.
- queued messages: spacer + "» ..." TruncatedText.
- escape: 1st: interrupt or clear input (arm repeat window 500ms?); 2nd within window: "tree" (if nothing to interrupt) or "clear" input. ESCAPE_REPEAT_WINDOW_MS in TS (check). Ctrl+C: first show hint "Press Ctrl+C again to exit" (override label), second exits. Ctrl+D exits when empty.
- pageUp/PageDown scroll transcript by pageSize=window-1; wheel=3; following pauses on scroll-up; ctrl+shift+down re-follows; shift+alt+up top. Not-following hint appears at bottom-right? (check renderFullscreen pin area) — the capture shows "ctrl+shift+down to follow" hint inside transcript bottom line.

## Conversation rendering
- user msg: Box(paddingX=2,paddingY=1,bg userMessageBg) with markdown (color userMessageText) inside.
- assistant: Container: Spacer(1) if visible content; per content block Markdown(paddingX=1, color mdBody, thinking dim); spacer between thinking and next; abort/error Text error color px=1; trailing Spacer(1) if toolCalls && (visible || aborted || !precededByToolActivity).
- tool call: ToolPanel: header " <label> · <status>" on toolPanelBg (px=2); status: "done" success / "error" error / "◇ running" bashMode / "queued" muted; then blank panel line + children (renderers) each on panel bg. ipython uses IPythonCellComponent (self render, no panel).
- ipython collapsed line: " <marker> python · <preview> · ↑in ↓out lines · <dur> · <err>" marker ✓ success/✗ error/◇ queued/running pulse bashMode; language muted, preview dim.
- expand (all): full code with gutter "╰─ " dim + highlighted source, output " › " prefix then "   " lines; "no output" muted when empty.
- file diffs: "    ╰─ <path> +N -M" (path muted, counts toolDiffAdded/Removed); expanded rich diff rows.
- agent messages (child): spacer? then " ◆ Agent message received · <participant>" accent ◆, muted label, dim participant; expanded body " ╰─ ..." lines customMessageText.
- custom_message injected (goal_context/heartbeat/rlm_child_terminal_notice/harness_digest?): InjectedPromptMessageComponent — header muted "Goal continuation · <objective truncated>" + hint "(Ctrl+O to expand)"; expanded shows markdown body customMessageText px=1.
- async_bash_completion: ShellCompletionComponent: " ✓ Background shell command finished" muted / "✗ ... failed · exit N" error; expanded: header with pid+time + Text body px=1.
- refinement outcome: Spacer + " ◆ Harness refined" refinementHeader + EventSummary " <summary>" refinementSummary color px=1 (collapse ws, max 2 lines w/ " …").
- harness_digest custom_message w/ display:true → InjectedPromptMessage? customType not in list → falls to createDisplayedCustomMessageComponent (custom-message box). CHECK actual capture: harness_digest shows as "◆ Harness refined ..."? No — refinement_outcome renders that. harness_digest custom_message: check what component renders it (createDisplayedCustomMessageComponent switch).
- agent_status custom: no component (not displayed?) — check.
- spacing: createConversationSpacing: leading spacer for user/custom/etc when previous is not compact neighbor (agentmsg/toolexec/ipython/bash/shellcompletion); assistant handles own spacing.

## Theme (prime)
- fg via truecolor when supported else 256; fg reset \x1b[39m; bg reset \x1b[49m; bold 1/22, italic 3/23, underline 4/24, strike 9/29.
- key colors from prime.json (see file). empty "" = default terminal.
- mdBody #d8d8dc, mdHeading primarySoft #8d7fc0, mdLink info #38bdf8, mdCode #c8c8cd, mdCodeBlock stringMint #8ba888, mdQuote muted, mdQuoteBorder grid #52525b, listBullet muted, hr grid, borderMuted grid, accent primary #7c6faf, userMsgBg #1a1a1f, customMsgBg #151518, toolPanelBg surface #0d0d10, toolSuccessBg #0e1510, toolErrorBg #1a0d12, selectedBg #222226, thinkingText #8b8b94.

## Markdown theme mapping
- heading: fg(mdHeading); l1 adds bold+underline; l4 bold+italic; l5+ italic. paragraph: default color mdBody. code block: indent "  " + mdCodeBlock (or highlight). list: "- " bullet mdListBullet muted; nested "  ". blockquote: "│ " quoteBorder + italic quote color. hr: "─"*min(w,80). table: box drawing ┌─┬─┐ bold header. space token: "".
- strict strikethrough ~~x~~. inline math $..$ pandoc rules; block $$..$$ or \[..\].

## Select list (autocomplete)
- items prefix "› "/"  ", selected accent; metadata columns (slash cmds): primary width clamp(12..32 by widest+2), argumentHint mdCode, sourceTag dim, gap 2; scroll info "  ↑ N more  ↓ N more" muted; selected description wrapped below.
- maxVisible=5 (3..20).

## Session JSONL types (from ~/.prime/agent/sessions)
- session, model_change, thinking_level_change, service_tier_change, custom(thread_goal_state|prime-agent.refinement...), custom_message(harness_digest|goal_context|async_bash_completion|refinement_outcome|refinement_notice|rlm_child_terminal_notice|..., with display flag), message{message:{role:user|assistant|toolResult, content[], usage, stopReason,...}}, session_state, agent_status{status:{summary,taskState,basedOnMessageCount}}, git_state, child_usage_attributed.
- content block types: text, thinking(+thinkingSignature), toolCall{id,name,arguments}, image.
- toolResult: {role:"toolResult", toolCallId, toolName, content[], details{durationMs,status,stdout,stderr,...}, isError}.

## pa-tui lane notes

- `editor.rs` ports `packages/tui/src/components/editor.ts` faithfully: grapheme
  segmentation (unicode-segmentation), atomic `[paste #N]`/`[image #N]` markers,
  word wrap with wrap opportunities, sticky vertical column decision table,
  atomic-segment cursor snapping, kill ring (ctrl+k/u/w, alt+d, yank/yank-pop),
  fish-style undo coalescing, prompt history (up/down), jump mode (ctrl+]),
  backslash-enter newline, large-paste markers (>10 lines / >1000 chars).
- `keybindings.rs` carries the TS DEFAULT_* tables (tui + coding-agent app
  bindings, incl. `app.tools.expand` = ctrl+o); user overrides load from
  `~/.prime/agent/keybindings.json`.
- `markdown.rs` implements the markdown subset used in sessions (headings,
  paragraphs, fenced code, lists, quotes, hr, inline bold/italic/code/links,
  wrapping). Not yet ported: tables, math (latex), syntax highlighting.
- `session.rs` drives the same view from pa-types `FileEntry`s: `SessionStream`
  is the seam between JSONL replay and a future live daemon feed.
- `view.rs`/`app.rs` render the interactive layout: user-message background
  blocks, assistant markdown, tool panels (`⏺ name` header, toolPanelBg),
  `─` separator (dynamic-border parity), `> ` prompt, footer model label.

### Verifier
`crates/pa-tui/tests/tmux_replay.sh` renders a real captured session at 80x24,
captures the pane, and asserts the structural contract + editor keys
(type/backspace/arrows/ctrl+o/escape/ctrl+c). All checks pass.

### Ambiguities
- The TS `prime-agent resume <id>` flow opens the agents view and spawns live
  activity rather than statically replaying history, and `-r <path>` fails to
  spawn a session worker inside this sandbox (EACCES). The tmux differential
  therefore asserts the pa-tui structure against TS UI captures (separator,
  prompt, footer, collapsed-mode line) instead of a same-session side-by-side.
- Thinking blocks are excluded from replay transcripts (TS renders them
  collapsed); `--show-thinking` placeholder flag retained for a future port.


## pa-ai / real-provider lane notes

- `PRIME_INFERENCE_BASE_URL` now mirrors the TS reference exactly: the TS
  binary's provider config (`packages/coding-agent/src/core/prime-inference-model-catalog.ts`)
  declares `https://api.pinference.ai/api/v1`; the old Rust value
  (`https://inference.primeintellect.ai/v1`) does not resolve on this box. The
  generated catalog (`pa-ai models.generated.json`) already carried the pinference
  URL; only the private-model / live-catalog constant and the catalog-refresh
  fetch diverged. A differential test reads the TS source as the golden.
- `pa_agent::types::UserPart` was `serde(untagged)`, but the TS wire format
  (`packages/ai/src/types.ts` `UserMessage.content`) is `type`-tagged parts
  (`{"type":"text", ...}`). The untagged form failed the pa-agent -> pa-ai
  JSON round-trip in `real_stream_fn`, so every prompt admitted through
  `AgentPromptInput::Text` was silently dropped before reaching the provider.
  `session_engine::provider_adapter` has a regression test for the boundary.
- `AgentSession::prompt` eagerly appended the user message to the session;
  the TS reference persists user prompts only from the agent `message_end`
  event (`_processAgentEvent`). The eager append double-persisted once the
  wire shape was fixed; it was removed (in-memory persistence parity verified
  by the existing `prompt_persists_user_and_assistant` test).

## package-manager lane notes

- Source parsing (`crates/pa-core/src/packages/source.rs`) ports
  `core/utils/git.ts` `parseGitUrl` + `utils/paths.ts` `isLocalPath`. The
  hosted-git-info dependency is covered by a subset (shortcut prefixes
  `github:`/`gitlab:`/`bitbucket:`/`gist:`, known domains, `git+` schemes,
  `#committish`); the generic fallback already covers every other host, so
  the observable behavior matches the TS on all documented forms.
- Product quirks preserved deliberately (all verified against the TS binary):
  - `git://host/path` parses as a LOCAL path (the `git:` prefix is stripped
    before the protocol check), so `package install git://...` reports
    "Path does not exist".
  - `github.com/user/repo` shorthand is local without the `git:` prefix and
    git with it.
  - Local settings entries store paths relative to their settings base
    (agent dir / project config dir), so `package remove` only matches by
    equivalent resolved identity - cwd-relative and settings-relative forms
    of the same stored entry do NOT match each other.
  - Update order: npm version probes run first, then one batched npm install
    per scope (`install -g pkg@latest`), then git fetch/reset/clean. Update
    is sequential in this port; the TS runs probes at concurrency 4, which
    only affects wall time, not output.
- Settings writes use a field-scoped read-merge-write under the settings
  lock (`persist_scope_field`), matching the TS `persistScopedSettings`
  (fields added to the file after this manager loaded survive); a scope
  whose settings file failed to parse is never written (the TS load-error
  guard).
- CLI output parity is plain-text: pa-cli carries no color layer (chalk
  levels are dropped when piped anyway), so transcript comparisons with the
  TS binary use piped output, which strips chalk colors.
- Self-update (`prime-agent update`) is a typed boundary: native release
  manifests + daemon update-restart coordination are a separate lane.
  `package update` (extensions-only) completes fully.
- Differential verifier: `crates/pa-cli/tests/package_e2e.rs` uses an
  embedded-path npm shim via the `npmCommand` setting and an ssh shim
  (GIT_SSH_COMMAND) mapping `ssh://localhost/...` onto a local bare repo -
  the only git transports the source parser accepts are https/ssh/git, so a
  local fixture needs the ssh shim (git daemon `git://` URLs are not
  parseable sources).

## Side questions / provider-retry lane notes

- Provider retry policy (TS `core/provider-retry.ts`) lives in
  `pa-core::session_engine::provider_retry`: pure decision functions
  (`provider_retry_delay`, `is_permanent_provider_failure_kind`, lifecycle /
  faux-queue checks) plus the retryable one-shot driver
  `complete_with_provider_retry` with an injectable wait future. The wait is
  injectable so the scripted engine can drive it under a plain `futures`
  executor while the real engine races it against an `AbortSignal` on tokio.
- `start_side_question` / `abort_side_question` (TS daemon-mode L4730) map onto
  the worker `SessionEngine` trait as one extra engine call
  (`run_side_question`): the worker owns the run registry (one live run per
  client per session, TS error strings), the abort controller, and the
  `side_question_event` frames; the engine owns the turn behavior.
- The side-thread clone (TS `core/side-question.ts`) is
  `pa-core::session_engine::side_question::run_side_question`: it re-clones the
  live conversation per turn (same system prompt, model, thinking level, and
  tool declarations so the provider KV-cacheable prefix is preserved), replays
  `previousTurns` after the clone, blocks tool execution via `before_tool_call`,
  caps the run at 3 turns, and streams partial answers to a caller sink. The
  retry loop is inlined there because streaming must interleave with attempts
  (the generic driver stays available for one-shot consumers).
- Statuses follow the TS `SideQuestionStatus` vocabulary:
  `running` / `complete` / `cancelled` / `error` (not "completed"/"aborted").
- Event delivery parity: the live TS supervisor (0.9.5) fans every worker
  outbound - including `side_question_event` - out to the clients *attached*
  to the session (`daemon-supervisor.ts` `handleWorkerFrame` skips clients
  without the session in `attachedActiveSessionIds`). Verified live: an
  unattached client gets the `start_side_question` success response but no
  events; after attach the events arrive. The Rust supervisor reproduces this
  (side-question frames ride the same attached-session routing as
  session events).
- Known deviations: (1) the real engine (`AgentSessionEngine`) uses the
  default retry policy (settings `retry.*` wiring is a follow-up); (2) a
  model-resolution failure surfaces as an `error` side-question event rather
  than the TS synchronous "Select a model before asking a side question"
  command failure (the TS worker returns the start response before the engine
  call in the Rust redesign); (3) the worker keys runs by the client id the
  supervisor injects into the routed command (the TS single-process daemon
  compares socket objects; under the supervisor split its observable behavior
  matches for the single-client flow).
origin/main

## Chunked snapshot streaming lane notes

- Chunked attach (`chunked_snapshot` capability) is
  `pa-daemon/src/snapshot_stream.rs`: the supervisor rewrites the worker's
  attach result the way TS `createStreamedAttachResult` does - the snapshot
  keeps an empty `messages` array, the top-level `messages` copy is dropped
  (slim results never had one), and a `snapshotStream` descriptor
  (`{id, messageCount, targetChunkBytes}`) is added - then emits
  `session_snapshot_begin` / `session_snapshot_chunk` /
  `session_snapshot_end` records after the response.
- Byte budget is TS `SNAPSHOT_TARGET_CHUNK_BYTES` (512 KiB, not the ~256 KiB
  the task sketch guessed): each chunk record's `messages` array is
  serialized compact and flushed before exceeding the budget; a single
  oversized message travels alone and is never split.
- Snapshot id parity: the live TS daemon names it
  `<activeSessionId>-<generation>-<sequence>` from the event cursor
  (`daemon-mode.ts` `snapshotTransferId`), e.g. `d3ad819c5e92-427baf366601-567`.
  The supervisor-side sha256 revision in `daemon-supervisor.ts`
  `getOrCreateTranscriptCache` is the fallback for workers that do not
  stream; since the Rust worker returns full snapshots, the cursor format is
  the one clients actually see on the wire and is what the Rust port uses.
- `purpose` on `session_snapshot_begin` is `attach` for attach and
  `replacement` for reattach; the TS catch-up purpose value is `resync` on
  the wire, so `pa_types::daemon::SnapshotPurpose::Catchup` now serializes
  as `resync` (was a latent `catchup` wire divergence).
- Design deviation: the Rust supervisor materializes the whole snapshot
  before writing the streamed response (TS streams chunks asynchronously
  after the response with abort controllers and transcript reservations).
  The synchronous dispatch keeps the same client-visible record order and
  makes mid-command aborts impossible; a malformed worker transcript
  surfaces as `session_snapshot_failed` after the response (TS: transcript
  cache failure mid-stream), and a snapshotless worker payload fails the
  attach itself before any record exists (TS: `attachClient` throws).
- `session_snapshot_failed` for genuine mid-stream aborts (TS aborts the
  transfer on `session_closed`) does not occur: the supervisor's write loop
  breaks the connection on socket errors exactly when TS destroys it.
- The attach result now echoes the client's own (normalized) capability
  set, matching the live TS golden; a capability-less client sees
  `["attach_snapshot","event_sequence"]`.

HEAD
## Thin-supervisor stage 2 (direct-attach transport) lane notes

- Shared wire mechanics moved to pa-types because pa-tui (pa-types only) must
  speak the worker socket as a direct-attach client: the private-frame codec
  (`daemon::framing`, served from pa-daemon as a re-export), the command-plane
  table (`daemon::plane`, TS `DAEMON_COMMAND_PLANE` verbatim - the worker gates
  peer links with it and the routed client picks the socket with it), and the
  platform socket-identity stat (`platform::identity`). The TS peer-grant and
  ticket wire shapes (`DaemonWorkerPeerGrant`, `DaemonPeerCommand`,
  `DaemonPeerTransportTicket`) already lived there.
- Ticket issuance (`peer_tickets.rs`, TS `issuePeerTransport`): the supervisor
  resolves a registered session, requires a ready/connected/peer-capable worker
  (capability captured from the worker's `worker_auth` response,
  TS `workerAuthAdvertisesPeerTransport`), pins the exact worker instance id
  and socket-filesystem identity (dev+ino), mints a single-use grant with the
  TS `PEER_TRANSPORT_GRANT_TTL_MS` = 10s TTL, pushes it into the worker
  (`worker_register_peer_transport`, 3s round trip), and returns the ticket.
  Deviations, documented in code: (1) the process-identity check is live-pid
  liveness rather than TS's `processStartId` pin (the Rust supervisor never
  populated `process_start_id`); (2) the client-owned-worker refusal has no
  Rust equivalent (every spawned/adopted worker is a resident session, and
  `owner_client_id` today records the creating client for all workers - a TS
  semantic that predates this lane); (3) the grant token is one v4 UUID's hex
  (122 bits, single-use + 10s TTL) instead of TS's 32 random bytes in
  base64url; comparison is sha256-then-constant-time like TS
  `timingSafeEqual`.
- Worker grant store (`peer.rs`, TS `peerGrants` + `peer_auth` +
  `worker_register_peer_transport`): grants live in worker memory only, burn
  on first use BEFORE the token is checked (a failed presentation also burns),
  expire at their TTL (registration rejects grants expiring more than 30s out,
  `PEER_GRANT_TTL_LIMIT_MS`), and are capped at 1024 after an expired sweep.
  Registration additionally validates the grant's `issuerGeneration` against
  the authenticated supervisor connection's generation (TS compares against
  the `boundClaim`).
- Worker connection roles: `worker_auth` promotes a connection to
  `Supervisor` (full command set, always streams events);
  `peer_auth` promotes to `SessionClient` (session-plane commands for the
  grant's session only - the TS `peerClaims` gate with the exact TS failure
  string "Command is not allowed on this direct peer transport"). Event
  fan-out is now role-gated: unauthenticated connections never receive the
  session stream, and a session client streams only while it holds an attach
  (its `attach` succeeded, `detach` stops the stream) - TS streams to
  `state.clients`, not to every socket.
- Client routed transport (`pa-tui/src/direct_transport.rs` + `DaemonClient`,
  TS `daemon-routed-client.ts`): `upgrade_direct` is TS
  `createDaemonSessionTransport` - require the `direct_peer_transport`
  capability, request `get_direct_worker_transport` (5s, no recovery), validate
  the ticket (shape, target session, freshness, socket identity re-stat),
  connect (1s), read hello, `peer_auth` (3s). Any failure silently keeps the
  supervisor-routed path (transition-period fallback). Session-plane commands
  for the link's session then ride the worker socket; control stays on the
  supervisor. A dead link discovered before the frame is queued falls back to
  the supervisor; a sent request that times out surfaces the error and is
  never retried (no double execution of prompts, TS comment parity). A failed
  direct attach retries once over the supervisor (TS
  `DaemonAgentConnection.attach`), and switching sessions drops the link (a
  grant is bound to one session).
- Direct attach commands are stamped with the client id and the same
  slim-snapshot capability set the supervisor's routed attach injects
  (`["attach_snapshot","event_sequence","slim_attach"]`), so both paths return
  identical attach results. The TS routed client sends its own raw
  capabilities (including `chunked_snapshot`); the Rust worker returns full
  snapshots on the direct path, so the slim set is the honest contract for now.
- e2e (`tests/direct_attach_e2e.rs`): ticket field/ttl assertions, grant
  single-use (replay rejected with the TS string), control-plane denial on a
  peer link, mid-stream kill -9 of the supervisor with the direct stream
  continuing, supervisor restart + roster rebuild + fresh ticket + reattach +
  second scripted turn, and live grant expiry after the 10s TTL.


## Thin-supervisor stage 3 (peer messaging) lane notes

- TS has no worker-to-worker peer transport for agent messages: the TS worker
  routes kernel sends supervisor-mediated (`sendRemoteAgentSessionMessage`
  -> supervisor `send_message` -> `worker_deliver_message`). Stage 3 extends
  the stage-2 ticket machinery with `worker`-purpose single-use grants so the
  delivery bypasses the supervisor's route plane; TS parity holds for every
  user-visible shape (sender identity, rendered prompt, receipt).
- Kernel `agent_message.send` contract ported from TS
  `createAgentMessageHostHandlers` (core/agent-messages.ts): the runtime
  skill sends `{message, receiver_role, receiver_name}` (or `target:"all"`),
  never a positional target - the old Rust handler expected `{target}` and
  always failed against the installed runtime. Role/name resolution goes
  through the family roster; the thin supervisor's family is the supervisor
  `list` roster (every other resident session is a sibling). In-worker
  parents/children and the TS family catalog (`selectAgentFamily`,
  `awaitPendingChildPublication`) are deferred with the catalog itself.
- Direct delivery semantics: the peer ticket burns on first use, so a send
  is never retried once the `worker_deliver_message` command went out - a
  failed/refused/lost delivery surfaces as the error; only pre-delivery
  failures (no ticket, connect failure, failed grant burn) fall back to the
  supervisor-routed `send_message` (the TS path, kept verbatim).
- Sender identity: TS renders the sender from the sending session's live
  summary; the worker pushes its summary to the engine at create/rename and
  the direct path builds `{activeSessionId, sessionId, sessionName?,
  runtimeKind, clientId: "agent"}` (the TS agent-origin sender shape when no
  client id is in play). The supervisor-routed fallback keeps the supervisor
  building the sender from the source worker's `get_state` (TS behavior).
- Supervisor-link restart window: TS tears the link down through its
  DaemonClient close listener, so the next request reconnects. The Rust link
  discovers death lazily, so a write-phase failure (the command never reached
  the supervisor) reconnects and retries exactly once; read-phase failures
  never retry (the command may have been processed). Verified by the
  supervisor kill -9 e2e in tests/peer_messaging_e2e.rs.

## Daemon model selection + status line (cli-flags-parity lane)

- TS `main.ts` `runtimeConfigFromArgs` builds `AgentSessionRuntimeConfig`
  (cwd, provider, model, apiKey, ...) and every daemon `create` carries it
  (`daemon-protocol.ts` `create.config`). Ported: `pa-tui` forwards
  `ModelSelection` (provider/model/apiKey) in the create config; the
  supervisor persists it into the durable create command so respawned workers
  resolve the same model; the worker binds it onto its engine
  (`SessionEngine::configure_model`), which treats explicit flags as
  authoritative over the process env fallback
  (`PRIME_AGENT_MODEL_PROVIDER`/`PRIME_AGENT_MODEL` remain the fallback when a
  create carries no flags).
- TS API-key precedence for worker request auth (`main.ts`
  `setRuntimeApiKey` for `--api-key`, `model-registry.ts`
  `getApiKeyAndHeaders`): create-config key, then auth storage, then the
  models.json provider `apiKey`; the pa-ai provider env-key map stays the
  last resort inside the provider. Ported in `agent_engine.rs`
  (`resolve_request_api_key`); custom provider names (no env mapping) now
  authenticate from models.json.
- TS daemon-session-summarizer.ts (status line): after each completed turn
  (`turn_end`/`compaction_end` broadcast, 2s debounce) and every 25s sweep
  for working sessions, the daemon asks a small model
  (prime-inference/qwen/qwen3-30b-a3b-instruct-2507) for a dashboard recap:
  fixed system prompt, `<agent-state>` + trailing-8-message conversation
  body, max_tokens 400; result broadcast as `session_status` with the recap
  text. Ported in `pa-daemon/src/status_line.rs`, including the settled
  idle verdict persistence to the session journal (`appendAgentStatus`):
  real model classifications and transcript error verdicts
  (`terminalTurnError` -> `taskState: "error"`) persist as `agent_status`
  entries, the needs_input fallback never grows the journal, and a
  respawned worker seeds its in-memory verdict from the latest persisted
  entry (`DaemonSessionSummarizer.seed`).
- TS print-mode `-c`/`-r` active-session guard (`session-lease.ts`
  `SessionAlreadyActiveError` -> supervisor `assertWorkerCreateOwner` on the
  daemon create): a headless continue/resume refuses when the target session
  file is live in the daemon ("Session is already active in <id>: <path>").
  Ported in the Rust print path as a daemon-list probe before opening the
  file (the Rust print path runs in-process, not over the daemon, so the
  guard probes the supervisor's live roster first). The supervisor-level
  reuse-vs-guard semantics of TS `createOrReuseWorker` (same owner reuses,
  different owner refuses) and interactive resume's pre-resolution to attach
  are not ported yet — a separate lane item.origin/main

## Worker robustness + session entry parity (worker-robustness lane, B-3/B-8)

- AF_UNIX over-limit paths: the installed TS product survives deep-TMPDIR
  worker sockets because its runtime (Bun) transparently re-anchors long unix
  socket paths through an O_PATH directory fd — an strace of the live worker
  shows `bind(13, {sa_family=AF_UNIX,
  sun_path="/proc/self/fd/12/worker-....sock"}, 110) = 0`, and the socket
  file lands at the original deep path. No TS source references this; it is
  purely runtime behavior (Node fails the same bind with `EINVAL`). The
  Rust port puts the same rewrite in `pa_types::platform::transport`
  (`UnixSocketAddress`): paths within the 107-byte `sun_path` limit pass
  through; longer ones (Linux) open the parent directory with `O_PATH` and
  bind/connect via `/proc/self/fd/<fd>/<file name>`, keeping the fd open for
  the address lifetime. Non-Linux Unix platforms surface the natural
  path-length error. Filesystem cleanup (unlink/chmod) always uses the
  original path (no 108-byte limit there).
- Worker connect budget: TS `WORKER_CONNECT_TIMEOUT_MS` is 30s on Unix
  (90s Windows) and covers probes (500ms), connect, and the auth handshake
  under one deadline (`connectWorker` + `handshakeBudgetMs`); a worker that
  never comes up throws `DaemonWorkerProbeTimeoutError` and the failed
  launch stops the child. The Rust supervisor uses the same 30s shared
  deadline (`worker_connect_deadline`), probes every 25ms (TS backoff
  min=max=25ms), kills the spawned child when the deadline trips, and
  bounds the auth request to the remaining budget.
- `service_tier_change` entries (TS `sdk.ts` `createAgentSession`): fresh
  sessions append `model_change` + `thinking_level_change` +
  `service_tier_change` (settings default, TS `getDefaultServiceTier`
  fallback `"default"`); resumed sessions append thinking and service tier
  only when no earlier entry set them. Ported in the pa-core engine
  creation path (engine-owned session files) and mirrored onto the daemon
  worker's own session file at create.
- Queue snapshots: the Rust worker used to persist steering/follow-up lanes
  as `custom` session-file entries (`prime-agent-rs.queue_snapshot`) —
  a Rust-only entry type that broke the B-8 shape diff. TS keeps session
  files free of daemon bookkeeping and journals worker-private state
  separately, so the snapshot moved into the worker recovery journal
  (`WorkerRecoveryJournal::record_queue_snapshot`, latest-wins per session,
  survives journal compaction).
- `agent_status` entries (TS `daemon-session-summarizer.ts`
  `appendAgentStatus`): settled idle verdicts persist as `agent_status`
  session entries — real model classifications and transcript error
  verdicts (`terminalTurnError` → `taskState: "error"`, recap
  "Model request failed: …") — never the needs_input fallback, sweeps, or
  working refreshes (no verdict), and only when the verdict differs from
  the latest persisted one. Respawned workers seed the in-memory status from
  the latest persisted entry (`seed`) so a restart does not re-ask the
  model. Persisted shapes match the live-session goldens
  (`{"type":"agent_status",…,"status":{"summary","taskState",
  "basedOnMessageCount"}}`).

- Slash-command dispatch (PR: registry + dispatch + autocomplete): the
  builtin command table is pure data in `pa-types::slash_commands` (the TUI
  cannot import pa-core; pa-core re-exports it for the session engine, and
  pa-daemon consumes it through pa-core). The TUI client dispatch runs
  local handlers first (`/help` `/list` `/switch` `/exit` are Rust-client
  dev commands, not TS builtins), then the registry: `SlashCommandExecution`
  splits client commands (locally implemented subset: `/new` incl. the
  no-argument `/clear` alias, `/quit` → detach-and-exit; everything else
  reports `/{name} is not available in this client yet` — there is no exact
  TS string for an unimplemented client command, so the repo's
  `not available in this build yet` convention is used) from session
  commands (`/compact` `/refine` `/goal` `/autonomous` forward verbatim as
  prompts; the worker parses them before admission and never runs a model
  turn). Unknown names reproduce the TS suggestion error exactly
  (`Unknown command: /x. Did you mean /y?` via the shared
  `find_slash_command_suggestion`); names over 64 chars and names without a
  suggestion pass through as prompts, like the TS product.
- Slash autocomplete (port of `packages/tui/src/autocomplete.ts` +
  `select-list.ts` + `fuzzy.ts`): the editor installs a
  `CombinedAutocompleteProvider` (registry commands via the fuzzy filter,
  plus the readdir-based file/path completion with TS `extractPathPrefix`
  trigger rules) in `Editor::new`. The TS product's `@`-attachment
  completion is fd-backed; this build has no fd dependency, so `@` tokens
  yield no suggestions — the same behavior as the TS product without fd on
  PATH. `getSlashCommandContext` is a full port (prompt-start command vs
  argument position, mid-line `/token` contexts). The dropdown renders
  above the editor on the toolPanel popup background with the TS slash
  layout (primary column clamped 12–32, argument-hint column, directional
  scroll info, selected-item description below the list).
- Slash echo/result rows (port of `slash-command-message.ts` +
  `slash-command-result-message.ts`): the durable `session_slash_command` /
  `session_slash_command_result` custom rows render as user-message blocks
  (`Box(2,1)` on `userMessageBg`), the echo with the `/name` token in
  `accent` and `@path`/`--flag` argument tokens highlighted (`success` /
  `mdLink`), a leading spacer when the chat is non-empty; non-display rows
  (the `/refine` result) and unknown custom types render nothing; a
  session-command custom row with an invalid payload renders the
  `[Malformed session command message]` notice. The OSC 133 prompt markers
  the TS components emit are not ported (the Rust TUI draws no OSC zone
  markers yet).
- Verification seam: a plain `{"responses": [...]}` script uses the echo
  `ScriptedEngine`, whose `run_prompt` does not parse session commands —
  use the `{"engine": "faux", "responses": [...]}` script form to drive the
  real agent engine (with session-command admission) in TUI e2e tests.


## Eval/verifiers composition (eval-composition lane)

- TS inventory: the coding-agent has no `eval` command. Verifier flows ride
  the autonomous gate mechanism (TS `core/autonomous.ts`, ported in #98) plus
  headless modes: print (`-p` / non-TTY) and `--mode json` stream
  `session_event` frames on stdout; ACP carries `_meta.autonomous` per
  completion. `prime eval ...` on PATH is the Prime Intellect platform CLI
  (a separate product, hosted evals over verifiers environments) — outside the
  coding-agent parity surface; recorded in docs/completion-matrix.md family 20.
- Headless autonomous loop (pa-cli `headless_autonomous.rs`, port of
  `modes/print-mode.ts` + the gate half of `headless-completion.ts`): CLI
  autonomous flags build `AutonomousRuntimeState` (TS
  `runtimeAutonomousConfigFromArgs`: any autonomous flag enables the run);
  a subscription forwards every settled assistant message to the driver
  (per-message accounting); after each settled prompt the driver decides —
  gate-failure continuation injected as the next turn (a durable user row),
  or stop, persisting the `autonomous_status` stop row into the session and
  emitting it as `message_start`+`message_end` events (the daemon worker's
  custom-row wire shape).
- Exit-code contract (print-mode.ts): a configured gate still failing after
  its retry window (with or without a limit) -> stderr
  `Autonomous quality gate still failing after attempt N/M: <exit text>[;
  autonomous limit reached: ...]` + exit 1; an autonomous run without gates
  stopped by a limit -> stderr `Autonomous run stopped before terminal
  evidence; <limit reached (used/cap)>` + exit 1; gate pass -> exit 0. The
  contract applies to both text and json output modes; the stop row never
  prints to stdout in text mode.
- Scope cut vs TS: `waitForHeadlessCompletion`'s host-side gate-failure
  re-injection after an errored model turn (TS retries failing gates even
  when the turn ended `error`/`aborted`) is not ported yet; the per-turn
  driver loop covers the gate-failed and limit paths. Same cut as the ACP
  transport.
- Verifier: `crates/pa-cli/tests/eval_composition_e2e.rs` drives the built
  binary with fixture verifier scripts (counter-file verifier + always-fail
  verifier) over the faux provider in isolated HOMEs — no network, no daemon
  sockets.


## Passive-RLM roster + ledger (roster-wire lane)

- `crates/pa-daemon/src/rlm_ledger.rs` (port of
  `modes/daemon/rlm-ledger.ts`): the daemon-owned RLM spawn ledger, one
  append-only JSONL per sessions dir (`<agent-dir>/rlm-ledger/<sha256-16-of-canonical-dir>.jsonl`),
  same record grammar (v:1 meta/spawn/rename/delete, unknown-op forward
  compat, version violations fail closed), same read bounds (32MiB /
  100k records), stat-guarded replay cache keyed on size+mtime+inode, and
  legacy per-parent `rlm-subagents.jsonl` seeding with the atomic
  no-clobber hard-link publish. The rename/delete record join falls back
  to a sole childId edge (the symlink-retarget case). Per-child display
  files (`rlm-subagent.json`) port `rlm-subagent-display.ts` including the
  deleted-tombstone refusal.
- `crates/pa-daemon/src/rlm_roster.rs` (port of
  `walkPassiveRlmSubagents` + `withPassiveRlmDescendantInfos`): the
  passive roster walk roots at every saved session file plus every resident
  session file; live ledger edges group children by canonical parent path;
  resident children contribute no row (their row comes from the live
  registry) but stay walk roots for their own children; every other live
  child's file is read for display data with the edge as the only topology
  authority (fork headers never trusted). Row shapes match the TS
  `buildSessionListWithPassiveRlmSubagents` enrichment (runtimeKind
  subagent, rlmChildId, parentSessionPath, parentActiveSessionId when the
  direct parent is resident, rlmParentNodeId fallback to the childId,
  spawnCode from display/legacy metadata) and `inactiveLifecycleForSession`.
- Supervisor admission moments mirror the TS daemon: `recordRlmSubagentState`
  (spawn edge + display entry at create, admission fails if the spawn record
  cannot be made durable), `recordRlmSubagentDeletion` (delete tombstones
  BEFORE teardown; the parent-side `delete_subagent` kill carries an
  `rlmLedgerDelete` marker so a plain `stop` never tombstones), and
  `appendRlmLedgerRenameForState` (rename by child path, offline-safe).
- `list --all` now composes TS `buildSessionList`: saved rows first with
  resident replacements in place, then passive children, then resident-only
  rows; a broken ledger fails the list command (TS propagates the walk
  error) while the saved-session catalog degrades to the saved rows
  (TS `withPassiveRlmDescendantInfos` catches).
- Mechanism difference: TS memoizes the passive walk with stat
  fingerprints (`passiveRlmSubagentMemo`); the Rust supervisor reads the
  ledger behind its own stat guard and re-walks per list. `family()` /
  `siblings()` ledger reads are not needed by any Rust surface yet and were
  not ported.
- Verifier: `crates/pa-daemon/tests/rlm_roster_walk_e2e.rs` — a synthetic
  1,000-child ledger plus persisted child files yields the full 1,001-row
  `list --all` roster against the real supervisor, performance-bounded
  (TS reference: 1,001 rows in 0.78s).

## Provider wire diff (roster-wire lane)

- Field-by-field request-body diff against the TS binary (same transcript
  replayed on both, mock provider capturing bodies; see the PR body table):
  message serialization is byte-size identical for user/assistant rows,
  tool calls, tool results, and cross-model thinking-as-text replay; the
  only systematic differences are the layered system prompt (by design)
  and the single-text-block user row shape (TS content array vs Rust plain
  string, -28 bytes/row, same text).
- One real omission found and fixed: Rust compared harness-digest
  staleness against ALL session entries
  (`latest_digest_from_entries(get_all_entries())`), so a digest stored
  before a refinement/compaction context boundary suppressed re-delivery
  even though it was no longer in the loop context. TS
  `_latestContextHarnessDigest` scans `agent.state.messages` only, by
  timestamp. Ported as `latest_context_digest` (frame-scoped, timestamp
  recency); a refinement-boundary resume now re-injects the
  `[harness-digest]` user row exactly like TS (verified end-to-end against
  the TS binary).

## Transcript scrollback + double-Ctrl+C exit (tui-interaction lane)

- `crates/pa-tui/src/view.rs` ports the TS `FullscreenViewport` scroll model:
  `scroll_top`/`following` with `last_max_scroll`/`window_rows` recorded at
  frame composition. `scroll_by` pages from the tail while following and
  clamps to `[0, last_max_scroll]`; reaching the bottom resumes following
  (`scrollToTop`/`scrollToBottom`/`pageSize`/`ScrollInfo` all follow the TS
  shapes). The bindings `tui.viewport.pageUp/pageDown/top/follow` (already
  present in `keybindings.rs` with TS defaults) were dead in the interactive
  loop and the replay app; both dispatch them before the editor, matching
  the TS `tui.ts` fullscreen consume order. The visual indicator is the TS
  follow hint: ` ctrl+shift+down to follow ` reversed, centered, composited
  over the last transcript window row. It keeps leading OSC-133 zone
  markers at the row head (`osc133::split_leading_markers`), so a marked row
  under the hint stays flagged and the row-diff marker plan re-emits it —
  the TS `compositeLineAt` behavior, which also keeps row-head sequences.
  Mouse wheel scrolling (TS `WHEEL_SCROLL_LINES`, mouse-reporting mode) is
  not ported: the Rust TUI never enables mouse reporting.
- `crates/pa-tui/src/session_ui.rs` ports the TS Ctrl+C ladder
  (`handleCtrlC`/`showCtrlCExitHint`): the first press interrupts (aborting
  an active turn) and arms a 2s exit hint (`Press Ctrl+C again to exit`,
  the TS tray override label replacing the location label); a second press
  inside the window shuts down unconditionally. Previously the first press
  awaited the abort request inline (a wedged worker socket blocked the UI
  loop for the full 30s request timeout, and the second press re-entered the
  same await — the hang). Now the abort is fire-and-forget on a cloned
  `DaemonClient` (TS `void abort()`), failures surface as transcript notes
  through a background-note channel, and the exit path detaches with a
  600ms cap: exit lands well under the 1s contract even with a
  `kill -9`-wedged worker. All key-path daemon requests (prompt submit,
  stats refresh, list/switch/attach, detach) are bounded at 10s so the UI
  loop can never stop reacting to keys. Adoption telemetry follows the
  AGENTS.md convention: schema-v1 events `tui scroll used` (first scroll
  action per run: `action`, `resumed_following`) and `tui exit`
  (`exit_reason`: `ctrl_c_twice`/`ctrl_d`/`session_request`/`daemon_closed`,
  `turn_active`), emitted through the `InteractionTelemetry` seam in
  pa-tui's interactive loop and implemented by pa-cli against the
  one-shot telemetry client (pa-tui stays pa-types-only). The scroll event
  is fire-and-forget; the exit event is bounded at 500ms so the flush can
  never hold the exit-within-1s contract.
- Exit resume hint (TS `formatResumeHint`, `resume-hint.ts`): the exit
  path fetches `get_session_stats` while the connection is alive
  (bounded at 500ms, best-effort like the detach) and pa-cli prints the
  dim `Resume this session with: prime-agent --resume <id>` line after
  the terminal is restored — only for a flushed, resumable session
  (`sessionFile` exists on disk, `userMessages > 0`); agents-view
  returns suppress it (TS `returnToAgentsView` has no shutdown print).
  The renderer teardown now leaves the alt screen with a visible cursor
  (`?25h` after `?1049l`). KNOWN tmux 3.2a quirk, documented here for the
  follow-up: tmux's input processing drops text printed after an
  alt-screen leave when the escape sequences coalesce into one read
  chunk (reproduced with plain `printf` byte replays; timing-dependent,
  unaffected by pre/post-print sleeps). The TS product never hits it
  because its TUI renders on the MAIN screen (no alternate screen): the
  dead pane keeps the final frame plus the hint. The fix for tmux parity
  is rendering on the main screen like TS — a pa-tui renderer decision
  tracked for the TUI-polish lane, not this one. In real terminal
  emulators the hint prints correctly (verified bytewise via strace:
  the 93-byte write lands on fd 1 after `?1049l`).
- Verifiers: battery flows `f12_scroll` (pane frames show the content
  offset: early prompts paged into view, follow hint visible, paging back
  to the tail resumes following — both TS and Rust sides) and
  `f13_ctrlc_exit` (C-c C-c exits within 1s with exit code 0 — healthy
  long-turn case on both sides; wedged-worker `kill -9` case on the Rust
  side), plus `pa-tui` unit tests for the scroll model, the hint, and
  marker preservation under the hint. The TS pane in f13 dies as a
  reaped-late zombie, so tmux never reports its exit code; its verdict
  accepts a prompt exit plus the shutdown resume hint instead.

## Saved-session wake for `send_message` (session-wake lane)

- TS ground truth: the `send_message` block in
  `modes/daemon/daemon-supervisor.ts` — when the target lookup fails with
  `Unknown active session:`, the supervisor resolves the selector against
  the saved-session catalog (`daemon-catalog-process.ts` `resolve`:
  cwd-scoped list first, then the whole catalog; a match is a session-id
  prefix or an exact name; ambiguity is `Ambiguous session selector
  "<selector>"` and outranks the miss), then `createOrReuseWorker` spawns or
  reuses a worker over the file and routes the delivery. A miss keeps the
  unknown-session error; the stopped worker's 12-hex active id is not
  durable (catalog keys are the session uuid and the name), so it stays
  unknown in both products.
- Rust port: `crates/pa-daemon/src/session_catalog.rs` (the catalog
  resolve over `session_store::list_sessions`), the wake block in
  `crates/pa-daemon/src/messaging.rs` (`registry.find_by_session_file`
  reuse first, then `launch_worker` over the file with the session's own
  cwd from the header), and the CLI wake observed as the TS golden
  `Sent to <name>` (`pa-cli` `run_send` was already the TS port; the
  supervisor arm used to answer unknown). `session_catalog` resolves
  locally by raw cwd equality (`info.cwd == cwd`), matching the existing
  Rust saved-session list filter rather than TS `normalizeCwd`
  canonicalization — on this surface both products agree for absolute
  paths.
- Scope cut vs TS: the wake reuses a resident worker by session file but
  does not consult the RLM ledger or the opening-worker idempotency map
  (`openingWorkers`/`catalogOpeningWorkers`); supervisor-backed creates are
  serialized by the registry's per-worker adoption gates instead. The
  family-reach assertion for agent-origin sends stays deferred (needs the
  family catalog).
- Verifier: `crates/pa-daemon/tests/saved_session_wake_e2e.rs` (kill a
  session, send by name -> worker spawns, turn completes against a local
  mock provider, second send reuses the resident worker, unknown and
  stale-active-id selectors keep the TS errors); `pa-cli` `daemon_commands_e2e.rs`
  extends the Rust end-to-end with the CLI `Sent to renamed` golden and
  the TS-differential test now exercises the TS daemon's own wake with
  both CLIs rendering the delivered receipt.


## Resident RLM child roster identity (session-wake lane, family-id keys)

- TS ground truth: `modes/daemon/agent-roster.ts` `rosterAgentIdForSummary`
  keys every subagent roster row `parentSessionPath#rlmChildId` (the live
  parent active id stands in when the parent has no session path); worker
  summaries carry the identity fields, `daemon-mode.ts`
  `rosterAgentIdForState` derives the same id from the create
  `runtimeMetadata`, and child deletion pushes the roster removal under
  that family key. The agents view computes the alias client-side from the
  summary fields (`agents-view-state.ts`), never from a server-written
  `rosterAgentId` field.
- Rust port: `pa-daemon/src/worker.rs` parses the create command's
  `runtimeMetadata` (kind=subagent; `rlm_children::launch_child` already
  sends it on main) into `SessionCore` and emits `rlmChildId`,
  `parentActiveSessionId`, and `parentSessionId` on every summary
  (`types.rs` serde plumbing, skip-when-none). The roster store already
  keys via `pa_types::daemon::agent_roster::roster_agent_id_for_summary`,
  so `roster_subscribe` snapshots, `worker_roster_delta` pushes, and
  `remove_roster_worker` removals all re-key resident children for free.
  `supervisor.rs` persists `runtimeMetadata` in the durable create
  command so a respawned child keeps its family key.
- TUI: `agents_view_state.rs` `daemon_aliases` computes the
  `agent:<parentPath#childId>` alias via the shared pa-types formula; the
  old read of a `rosterAgentId` summary field was dead (no Rust writer
  ever set it).
- Ownership note for the roster-wire lane: roster-wire persists a
  top-level `rlmChildId` rest key for its ledger tombstone path; this
  change persists the whole `runtimeMetadata` block (a superset for
  worker respawn parsing). Keep both inserts when the lanes meet.
- Verifier: `agents_view_roster_e2e.rs`
  `rlm_children_key_the_roster_by_parent_path_and_child_id` — a scripted
  child spawned through `SupervisorChildSessions` joins the roster
  snapshot keyed `parentSessionFile#childId` with runtimeKind/rlmChildId/
  parentSessionPath/parentActiveSessionId/rlmDepth/sessionName, its live
  delta arrives under the same key, and `delete_subagent` pushes the
  removal under the family id.


 HEAD
## RLM child terminal notices + subagent TUI surface (subagent-surface lane)

- TS ground truth: `agent-session.ts` tracks every `rlm.spawn` run and, when
  a child finishes without an agent message to the parent
  (`child._parentReplyCount` unchanged), appends the
  `rlm_child_terminal_notice` custom row (`[child-exited: no-reply ...]` with
  the last assistant text preview) as a queued `followUp` turn action whose
  delivery record carries the custom message; the row renders in the parent
  transcript ("RLM child status") and the turn runs the model on the notice
  content. Deletion of a still-running child emits the `cancelled` notice
  ("Deleted by parent orchestrator"); failures emit `rlm_child_failure`. The
  TUI half: `SubagentSummaryLine` (the `subagents` counts box under the
  editor, `alt+a` to focus, confirm/open to drill in) feeds from the daemon
  roster (`countRosterSubagentStatuses` over `collectSubagentDescendantSummaries`),
  and `returnToAgentsView("scoped_agents_view")` opens the agents view
  scoped to the session's subtree (`scopeToSessionSubtree`, root excluded,
  `depth rlmDepth+1` metadata, `left` returns to the parent).
- Rust port (mechanism notes):
  - `pa-core/session_engine/rlm_notices.rs` owns the wire vocabulary and the
    two row constructors (content + details byte-parity with
    `messages.ts`).
  - The daemon (not the parent engine) owns child runs, so the settle
    watcher lives in `SupervisorChildSessions`: on spawn admission a
    detached task slices `wait_for_idle` against the child worker, settles
    through the existing `refresh_record` path, and delivers the no-reply
    notice when the child never replied. Reply tracking: the parent
    worker's `worker_deliver_message` handler marks the child on the
    registry (`SessionEngine::mark_child_reply`, TS `_parentReplyCount`).
  - Delivery rides the supervisor `follow_up` route with the wire's
    `customMessage` input (already in `PromptInput`): the parent worker
    queues the item, and `run_prompt` emits the custom row instead of the
    user row while the model turn still runs on the notice content — the
    same turn shape TS's injected notice action produces.
  - `delete_subagent` of a still-running child delivers the `cancelled`
    notice; the record's `notice_delivered` claim collapses the race with a
    natural settle.
  - Not ported yet: `rlm_child_failure` emission (a crashed child worker
    respawns under the recovery redesign, so a run-level error verdict is
    the worker-recovery lane's surface); the queue-visibility suppression of
    notice turns (TS `queueVisible: false`); the TS in-process
    `get_rlm_children` snapshot fallback (the Rust summary counts read the
    public roster, which every daemon session is on).
- TUI: `chrome.rs` renders the counts box (TS `SubagentSummaryLine.render`
  geometry: success/warning/dim counts, gap, focused `Enter/→ open` vs
  `↓ select` hint, selected background); `session_ui.rs` subscribes the
  session client to the roster (`roster_subscribe` + `roster_update`
  pushes, TS `subscribeAgentRoster`), counts descendants via
  `subagents.rs` (`collectSubagentDescendantSummaries` key math:
  active/session/file parent keys), and hands the terminal to the scoped
  agents view (`InteractiveOutcome.agents_view_scope`, pa-cli keeps the
  scope across the view/session loop). The scoped view lists the scope
  root's descendants excluding the root (`scope_to_descendants`), renders
  the `← back · <title> › subagents` label, the `depth rlmDepth+1` splash
  metadata row, the `← parent` hint, and `left`/escape reopens the scope
  root's session (TS `finish({type:"open", summary: backSession})`).
- Adoption telemetry: `tui subagents open` (`children_total`) rides the
  same `InteractionTelemetry` seam as the scroll/exit events.
- Verifiers: `pa-daemon/src/worker.rs`
  `an_injected_custom_turn_replaces_the_user_row` (the injected turn's wire
  shape); `rlm_children.rs` `watch_tests` (no-reply notice delivered to the
  parent, replied child suppressed) over a scripted supervisor;
  `pa-tui/src/subagents.rs` descendant-count tests; the f20 battery rerun.


## Provider failover (lane `provider-failover`)

- The TS quick-retry policy (`core/provider-retry.ts`) was already ported in
  `pa-core/session_engine/provider_retry.rs`; the failover lane keeps it
  byte-identical for the no-candidate path (a model served by exactly one
  configured provider keeps today's retry-exhausted surfacing).
- TS ground truth for the switch mechanism: `agent-session.ts`
  `_handleBackupModelRetry` / `_resolveBackupModel` /
  `_restorePrimaryModelAfterBackup` (a user-configured backup model; TS
  switches IMMEDIATELY on any transient failure). The Rust lane generalizes
  it to catalog failover (next configured provider serving the SAME model
  id, after the current provider exhausts its per-provider retry budget)
  and reuses the TS wire vocabulary: `auto_retry_start` with
  `reason: "backup"` + `backupModel`, `auto_retry_end` with
  `restoredModel`.
- New behavior (no TS counterpart): `pa-core/session_engine/provider_failover.rs`
  walks the candidate chain in catalog order; per-provider schedule from
  settings `retry.failover` (defaults: enabled, 5 retries per provider, 1s
  doubling backoff capped at 30s). The existing `retry.maxRetries` /
  `baseDelayMs` (TS defaults 3 / 2000) still govern the single-provider
  path.
- The daemon session's stream is request-dynamic through the provider-target
  slot (`provider_adapter::switchable_stream_fn`, the mechanism the
  model-picker lane landed for `set_model`): the failover switch/restore
  closures swap the slot to the switched-to provider (key resolved through
  the engine's request-key resolution) alongside the agent model re-bind,
  so the retried request hits the switched provider without a session
  rebuild; the no-switch path stays byte-identical (the build-time target).
- Candidate resolution: `pa-core/models/resolver.rs::failover_candidates`
  (same model id, other providers, auth-configured catalog, rotated to
  start after the current provider).
- Verifiers: unit tests on the backoff schedule, the switch order, and the
  candidate rotation; `pa-daemon/tests/provider_failover_e2e.rs` (primary
  500s -> switch -> backup answers -> primary restored; all-fail chain);
  battery flow `f22_provider_failover` (f11 stays unregressed for the
  single-provider case and the flow records the intentional TS divergence
  — the TS product has no failover).

## /model catalog + confirm row + /effort picker (model-picker-2 lane, f17)

- TS ground truth: the interactive `/model` picker sources its rows from the
  model registry's available catalog (models.json entries included, custom
  providers authenticate through their own `apiKey`), sorted by the
  `ModelSelectorComponent.sortModels` chain (configured providers first,
  prime-inference pinned, the current model first, the recent-use rank, the
  provider name, `featured`, numeric id compare); picking rides
  `connection.setModel` -> the daemon `set_model` command -> `session.setModel`
  (agent model swap + `appendModelChange` + `setDefaultModelAndProvider`),
  and the client records the `Model: <id>` status row after the state
  refresh. `/effort` reads the connection state's `availableThinkingLevels`
  (an `off`-only list is no thinking surface), opens the thinking selector
  or applies directly, and shows `Thinking level: <level>`; failures use
  `showError` (`⚠ Error: ...`).
- Rust port: `pa-core::models::order_for_picker` implements the sort chain
  (the composition root applies it to the catalog snapshot; the picker
  moves the live current model to the front), `ModelPicker` rows now label
  by model name with the id as a filter field, and the worker gained the
  `set_model`/`set_thinking_level` arms (`pa-daemon/src/model_switch.rs`):
  registry resolution with the TS `Model not found: ...` message, the
  durable `model_change`/`thinking_level_change` rows (the level row only
  on a real change, TS `isChanging`), the settings defaults, and the
  settings-default-level persistence gate. The engine switch
  (`AgentSessionEngine::switch_model`/`switch_thinking_level`) updates the
  selection, the live provider-target slot
  (`provider_adapter::switchable_stream_fn` — the built session's stream
  reads the slot per call, so a switch never rebuilds the session), and the
  built session's agent through the `AgentSession` seams. The worker's
  connection state now reports the resolved model's supported thinking
  levels (was a `["default"]` placeholder).
- `SessionUi::note` now implements the TS `showStatus` back-to-back rule: a
  status emitted with nothing after the previous one rewrites the previous
  status row in place (the f17 effort frame's `Model: mock-1` row is
  replaced by `Current model does not support thinking`, exactly like TS).
- Telemetry: builtin client commands (`/model`, `/effort`) emit
  `agent command used` from the interactive client (TS
  `captureAgentCommandUsed`); session commands keep emitting through the
  worker's session telemetry, so no submission is double-reported.
- Verifiers: `scripts/battery/runs/20260919T040417Z` — the f17 flow's three
  rust-side rows plus the `model-selected` and `effort-picker` frame diffs
  pass (normalized byte-identical); the remaining `model-selector` frame
  row needs the TS inline menu-panel rendering (search input, effort-square
  rows, price detail) plus live prime-inference catalog parity (the TS
  daemon fetches 1282 live models; the rust catalog is the 110 bundled
  entries + models.json), which is follow-up lane work.

## /model inline menu-panel + live catalog (model-selector lane, f17 close-out)

- TS ground truth: the interactive `/model` picker is the inline
  `ModelSelectorComponent` mounted by `showConfigurationMenu("models")` —
  it replaces the editor in the prompt dock (the prompt context / detail
  hint stays above it, the transcript stays mounted behind it), never a
  full-pane overlay. The panel is the TS `menu-panel.ts` inline shape: a
  full-width border rule, the `> `-prompt search field with the caret cell
  and dim placeholder, `>`-marker rows whose primary cell is the model name
  plus the centered effort cluster (`< arrows, effort squares padded to the
  row's slot count, level label padded to the longest level name), a
  right-aligned trailing cluster (`current - require sign in - provider`,
  shrunk from the front and truncated when narrow), the `  (i/n)` scroll
  indicator, the selected model's price detail block (`Input / Cached input
  / Output` columns with the `$ / 1M tokens` unit, narrow panes fall back
  to `Label: $x` rows), and the
  `↑/↓ model · ←/→ effort · Enter select · Esc close` hint (select/close
  only below 70 columns). Keys are the TS `handleInput` chain: up/down wrap,
  page moves step the visible window, left/right adjust the selected row's
  effort once the filter is empty or the list was navigated into, Enter
  confirms (the effort rides the apply only when the user edited it), Esc /
  Ctrl+C / left-at-column-0 cancel, and everything else edits the search
  field — a full TS `Input` port (undo stack, kill ring with accumulate and
  yank-pop, word motion, bracketed paste).
- Rust port: `pa-tui/src/menu_panel.rs` (the inline primitives: search
  field, `>`-rows with the soft-selection band, trailing budgeting) +
  `pa-tui/src/search_input.rs` (the single-line input) +
  `pa-tui/src/model_picker/` (the selector state: the TS `sortModels` chain
  — configured providers first, signed-in Prime Inference pinned, the
  current model, recent rank, provider, featured, numeric id — the
  `scoreModelSearch` quality ladder over `fuzzy_match`, the effort layout
  ladder, and the frame render). The picker mounts through the shared dock
  compose in `view.rs` (prompt context above, transcript behind), so the
  frame keeps the same scroll state as the chat behind it. The effort seed
  is the session's live level for a reasoning current model, else the
  settings default (`medium`).
- Live catalog: TS `get_model_catalog` (daemon-mode) ->
  `refreshModelCatalog` -> `refreshAvailableModels`, which fetches
  `https://api.pinference.ai/api/v1/models`, builds the model list over the
  bundled templates (minimum coverage 50%, failures fall back to the disk
  cache, then the bundled catalog), and writes the raw payload to
  `<agent-dir>/prime-inference-models-cache.json`; fresh registries read
  that cache at load (`PI_OFFLINE` stays on cached/bundled and never
  writes). The Rust daemon now serves `get_model_catalog` with the same
  contract (registry refresh, then the full catalog minus unauthorized
  private models plus `configuredProviders`), and the worker create path
  fires the same refresh at startup (the session-boot
  `refreshAvailableModels` trigger). The client fetches the catalog at
  attach and on `/model` open past the 60s TTL (TS
  `MODEL_CATALOG_REFRESH_TTL_MS`, forced when a search argument rides the
  command), folds the landing into the session and any open picker (TS
  `updateModels` keeps the selection on the surviving model), and the
  composition-root snapshot (bundled + models.json) serves until then —
  the offline fallback.
- Layering notes: the picker needs thinking-level semantics, so
  `get_supported_thinking_levels`/`clamp_thinking_level`/`models_are_equal`
  moved to `pa_types::ai::thinking_levels` (pa-tui is pa-types-only;
  `pa_ai::models` re-exports them). The picker owns the TS sort chain, so
  pa-core's `order_for_picker` moved into the picker module.
- Verifiers: `runs/20260919T144043Z` — the f17 flow passes 9/9 including
  the model-selector frame diff (normalized byte-identical: bordered field,
  `>` rows with effort squares at the TS columns, `current ·
  prime-inference` trailing, `(1/<n>)` indicator, price detail, key hint).
  Unit tests: the picker frame test pins the exact TS capture rows, the
  sort/filter/effort/key behavior, and the search input's kill ring;
  pa-core registry tests pin the live-cache merge (repriced entries replace
  bundled templates, live-only entries land, models.json custom models
  survive, corrupt cache falls back); `pa-daemon/tests/model_catalog_e2e.rs`
  is the offline-fallback e2e (PI_OFFLINE supervisor, no auth:
  `get_model_catalog` serves the bundled catalog + the models.json model,
  `configuredProviders` is exactly the models.json provider, private models
  stay out, the cache file is never written).

## Session archiving (session-archiving lane, roadmap item 4)

- TS ground truth: archiving in the TS product is a session-state marker, not
  a disk mechanism — `daemon-mode.ts`'s `archiveSession` appends
  `session_state { status: "archived" }` (ctrl+x in the session list),
  `inactiveLifecycleForSession` classifies archived/crash records out of the
  agents view, and they stay reachable only via `--resume <selector>`
  (`daemon-session-list.ts`). Idle retirement for RESIDENT workers is
  `idleEvictionMinutes` (default 90, `"off"` disables, malformed falls back
  to the default; `settings-manager.ts`), swept at boot plus every 1–5 min
  (`daemon-supervisor.ts`), with `hasRegisteredCronJob`/`attachedClients`
  guards (`session-action-store.ts` `canEvictWorker`). There is no TS disk
  archive; the sessions directory keeps growing — the roadmap item is a new
  mechanism informed by those rules, not a port.
- New behavior (no TS counterpart): `pa-daemon/src/session_archive.rs` moves
  retired sessions (never deletes) into `<agent-dir>/sessions-archive`
  (same per-`<uuid>.jsonl` layout, one level under the agent dir; moves fall
  back to copy+delete across filesystems). Two independent settings rules
  (pa-core `get_session_archive_policy`, the `idleEvictionMinutes` grammar —
  `number | "off" | "none"`, malformed falls back to the default):
  `sessionArchiveMaxAgeDays` (default 30; file mtime, inclusive boundary)
  and `sessionArchiveMaxSessions` (default 200; keep the newest by mtime,
  ties by path). Resident workers' session files and sessions with ACTIVE
  scheduled jobs (the `scheduled-jobs.json` artifacts scan) are counted
  toward the cap but never archived.
- Sweep seam: supervisor boot sweep plus a periodic re-sweep at the TS max
  sweep interval (5 min), housekeeping-only (failures log and retry; the
  sweep never gates serving) — the boot-sweep precedent is update-flow's
  `boot_sweep`.
- Catalog integration: archived files are physically out of the sessions
  dir, so every existing listing path excludes them by construction (the
  TS archived lifecycle stays a marker for ctrl+x'd sessions; the agents
  view / saved-session catalog never showed archived rows by default).
  Restore is the resume path: `resolve_saved_session` falls back to the
  archive (cwd-scoped first, then global, same prefix/exact-name matching
  and the same `Ambiguous session selector` error), RESTORES the match into
  the sessions dir, and returns the live path — the wake spawns over it,
  so `--resume <selector>` reaches archived sessions exactly like TS
  archived sessions stay resume-reachable.
- Telemetry: the sweep emits the `daemon event` kind `sessions_archived`
  with a `count` property (added to `docs/telemetry-events.md`).
- Verifiers: policy unit tests (inclusive age boundary, count-cap ranking,
  protected sparing, rule union), sweep/restore unit tests
  (`session_archive.rs`), catalog fallback tests (`session_catalog.rs`),
  and `pa-daemon/tests/session_archive_e2e.rs` (fixture session dir: aged +
  fresh + job-pinned sessions; boot sweep moves the right ones, the
  catalog excludes them; and the full restore round-trip: kill → age →
  restart archives the session, `send_message` by name restores it and the
  woken worker answers against the mock provider).

## Transcript prompt highlight: user rows + session-command echo (prompt-token-mask lane)

Extends #195's prompt-highlight engine over the two transcript surfaces it
deliberately left out (TS `prompt-highlight.ts`):

- User-message rows (TS `UserMessageComponent` + `HighlightedMarkdown` +
  `PromptTokenMask`): the prompt-highlight applies to EVERY transcript user
  row. The accent command segment masks only when the row's text opens with
  a `/name` naming a recognized command (TS `isRecognizedSlashCommand` —
  builtins plus daemon-registered connection commands; the Rust
  recognizes builtins, the same reduction the queued-strip preview makes);
  the argument tokens (`@path` `success`, `--flag` `mdLink`, a bare `--`
  only for argument-taking commands) mask on every row. Masking is
  layout-exact: tabs expand to three spaces, each token grapheme is
  replaced by a private-use base char (U+E000 + index) padded with U+FF9E
  per extra column so the placeholder measures the grapheme's width; the
  markdown renders the masked text (so markdown cannot wrap inside, eat,
  or emphasize token text — e.g. an `@path` full of asterisks renders
  verbatim), then `restoreLine` swaps the placeholders back in the token
  colors inside the `userMessageText` body. Sources holding literal
  U+E000..U+F8FF/U+FF9E chars, or more masked graphemes than the 6400-char
  placeholder alphabet, mask nothing and render plain (TS `MASK_LITERAL`
  and capacity bail). TS `restoreText` (selection-copy restore) has no
  Rust surface yet — mouse text selection is unported.
- Width mirror: TS `graphemeWidth` counts U+FF9E/U+FF9F (halfwidth
  katakana sound marks, EastAsianWidth H) as one column each; the Rust
  `char_width` now does too (unicode-width counted them zero as
  Grapheme_Extend, which would under-measure the mask's placeholders).
- Session-command echo rows (TS `SlashCommandMessageComponent` +
  `styleSlashCommandText`): the accent covers the leading `/name` for ANY
  typed name (recognized or not — the echo styles the typed text, not the
  registry), and the WHOLE text when the row is not a slash command (TS
  `commandEnd = text.length` fallback); the rest is default foreground
  with the argument tokens colored. The styled line wraps (TS wraps the
  styled string), so a token split by a line break keeps its color on both
  halves — the old per-row rescan lost the continuation's color and
  missed the quoted/backslash token forms.
- Surfaces kept byte-stable: the queued strip, editor highlight, and the
  malformed-session-command notice paths are unchanged (`/hotkeys` echoes
  through the same user-row renderer with the builtin-recognition
  predicate).
- Verifiers: unit tests on the mask (placeholder text, width
  preservation, zero-width literals, tab expansion, literal/capacity
  bails, restore), the user-row renderers (token colors inside the
  `userMessageText` body, accent on recognized commands only, markdown
  shielding, plain fallbacks), and the echo renderer (TS span shape,
  quoted/bare-separator token forms, wrap color carry-over); frame
  evidence in `scripts/queue_parity.py`: a token-bearing user row, the
  `/compact` echo row (fresh-session skip warning flow), and the
  `/hotkeys` command-bearing user row, each byte-exact vs the TS binary
  at width 120 (the `/hotkeys` state runs 120x80 in its own tmux session
  — its guide overflows the 36-row viewport, and the row bytes are
  width-bound).

## Session export fidelity (PR: tools section + custom-tool pre-render)

- TS ground truth (export-html/index.ts): the live-session export
  (`exportSessionToHtml`, reached by the daemon worker's `export_html`
  command) embeds `state.tools` mapped to
  `{ name, description, parameters }` and pre-renders custom-tool
  calls/results through the session's tool renderers
  (`createToolHtmlRenderer` + `preRenderCustomTools`): tool-call blocks
  outside `{bash, edit}` ask the tool definition's `renderCall`/
  `renderResult` (pi-tui components, rendered at width 100), the ANSI
  lines convert to inline-styled HTML (`ansi-to-html.ts`), and the map
  is keyed by tool-call id (`{ callHtml, resultHtmlCollapsed,
  resultHtmlExpanded }`; an empty map serializes away entirely). The CLI
  `session export` path (`exportFromFile`) embeds neither section — both
  stay omitted there.
- Which layer runs the renderer at export time: the SESSION layer
  (agent-session's `exportToHtml` builds the renderer from
  `this.getToolDefinition` + theme + cwd and passes it to the exporter);
  the TUI-app layer's special cases (interactive-mode's ipython card) are
  NOT consulted — the TS `ipython` tool definition carries no
  `renderCall`/`renderResult`, so exported ipython calls render through
  the template's generic fallback in TS too. The Rust port mirrors this:
  pa-core owns the walk + ANSI->HTML at the export step, the daemon
  engine's `ExportToolRenderer` resolves tools against the session's live
  registry (TS `getToolDefinition`), and a stock session therefore omits
  `renderedTools` in BOTH products — an unregistered custom-tool call
  falls back to the template's generic card identically (pinned by
  `export_live_differential.rs`).
- Extension tools' renderers cannot cross the Rust sidecar boundary
  (components are process-local; docs/extensions-runner-design.md
  §2.3/R3 "line-oriented subset" is designed but not landed), so a Rust
  session with extension tools omits the pre-render for them — the same
  degradation the design doc specifies for the TUI side; the exporter
  seam (`ToolHtmlRenderer`) is where a future line-oriented renderer
  lands without re-architecting.
- Export data sections now build lazily when an export precedes the
  first turn (the daemon engine builds the core session at the export
  read — the TS state exists from create); mid-turn exports omit them
  (best-effort try-lock, the pre-existing `export_system_prompt` rule).
- Verifier: `differential_live_export_matches_ts_binary`
  (pa-cli/tests/export_live_differential.rs) resumes the same fixture
  session (custom-tool call + result) on the TS daemon and the Rust
  daemon, runs the `export_html` wire command on both, and compares the
  exported files' decoded session data: header, fixture entries, the
  tools section (structurally equal, `ipython` registered), the
  `renderedTools` omission, and `systemPrompt` presence (content
  superseded by the layered prompt).
## Suspend-to-background (`app.suspend`, default ctrl+z) — TS `handleCtrlZ`

- TS ground truth (interactive-mode.ts `handleCtrlZ`, tui.ts `stop`/
  `enterFullscreen`): Ctrl+Z installs a no-op `SIGINT` listener for the
  suspended window (Ctrl+C at the shell must not kill the backgrounded
  app), stops the TUI (`ui.stop()`: fullscreen flush to native
  scrollback, mouse tracking released through
  `syncFullscreenMouseTracking`, alt screen left), then
  `process.kill(0, "SIGTSTP")` stops the process group. The one-shot
  `SIGCONT` handler removes the SIGINT listener, restarts the TUI
  (`ui.start()`), re-enters fullscreen (`applyFullscreen(true)` — which
  re-applies SGR mouse tracking), and forces a full render. win32 shows
  the status "Suspend to background is not supported on Windows". No
  other state is restored (no focus bookkeeping).
- Port: `pa-tui/src/suspend.rs` owns the cycle
  (`suspend_cycle(signals, terminal)`): SIGINT shielded → terminal stop
  → `kill(0, SIGTSTP)` → (the process stops; SIGCONT resumes execution
  inside the same call) → shield restored → terminal resume. Execution
  resuming inside the call replaces TS's persistent SIGCONT listener
  (Rust has no event-loop keep-alive requirement). The terminal side is
  the renderer's existing suspend/resume pair (the same one
  `/mcp login` uses): mouse tracking off + flush + raw-mode-off, then
  raw mode + alt screen + mouse tracking (per the `terminal.fullscreenMouse`
  setting) + full repaint. The session key dispatch requests the cycle
  (`session_ui` sets the flag; the loop owns the renderer); failure
  surfaces an error row and re-attempts the resume instead of the TS
  throw (a broken terminal state is the worse outcome), tracked as
  `tui suspend used` outcome `failed`.
- SIGINT shield subtlety (real port bug found by the e2e): TS installs a
  no-op *handler*, not `SIG_IGN`. The kernel queues a signal sent to a
  *stopped* process and evaluates the disposition at delivery, so a
  `SIG_IGN` shield would let a pending SIGINT kill the process right
  after the resume restored the default disposition mid-resume. The
  pa-types platform wall (`platform::process::{ignore_sigint_for_suspend,
  restore_default_sigint}`) installs a no-op handler; unit-tested by
  querying the disposition through `sigaction` itself (the sandbox
  kernel — gVisor — does not surface /proc's signal-mask lines at all,
  which is also why /proc-based evidence from an earlier probe run was
  taken on the box).
- Heartbeat-manager note (owner of the overlay): TS's `suspendFullscreenMouse`
  overlay option disables mouse tracking while the overlay is visible and
  re-applies it through the same `syncFullscreenMouseTracking` seam; when
  the heartbeat manager overlay is ported, route it through
  `mouse_tracking::{enable,disable}` the same way (out of scope here).
- Verifiers:
  - pa-tui unit tests: the cycle's sequencing (SIGINT bracket, cleanup
    on failure) and the mouse-tracking release/re-apply through the real
    seam (a real SIGTSTP cannot stop the test process itself).
  - pa-types unit test: the shield dispositions are really installed and
    restored (handler while suspended, default after).
  - pa-cli `tests/suspend_signal_e2e.rs`: the real product path on a pty
    in a child process group — startup mouse enable, Ctrl+Z releasing
    tracking + flushing scrollback, a real SIGTSTP group stop
    (waitpid-verified), SIGCONT resuming the process and repainting, and
    the resumed editor rendering typed text.
  - strace evidence (recorded during the harness bring-up): the child's
    post-continue resume writes the alt-screen-enter + mouse-enable
    sequences byte-for-byte before its repaint.
  - Harness findings worth keeping (pa-cli test doc-comment): the child
    is a session leader in its own group with no controlling terminal,
    runs a current-thread runtime (a multithreaded runtime's parked
    workers can lose futex wakeups across a stop/continue), the pty is
    drained to silence before SIGCONT (the suspend's 2.5KB scrollback
    flush can fill the kernel-side pty buffer and the pty driver silently
    drops writes that find it full), and the post-continue assertion
    pins the repaint rather than the first re-apply bytes (this sandbox's
    pty still nondeterministically eats the first post-continue writes).
    A SIGINT sent to the *stopped* child queues until SIGCONT and is
    delivered while the app is mid-resume — that interaction destabilizes
    the terminal reader in this harness, and a real shell cannot produce
    it anyway (Ctrl+C goes to the shell, the foreground process), so the
    shield window is unit-covered instead of e2e-covered.
## Compaction abort lane (compaction-abort, the #207 cancelled follow-up)

- TS ground truth (agent-session.ts): `abortCompaction()` aborts BOTH the
  manual `_compactionAbortController` and `_autoCompactionAbortController`
  (threshold/overflow/requested runs). Reaches the session from the
  daemon wire command `abort_compaction`, and from `requestAbort()` (the
  daemon `abort` command; TS daemon-mode calls `session.requestAbort()`
  which calls `abortCompaction()` last). The TUI interrupt key (Ctrl+C)
  fires `abortCompaction()` when `isAgentCompacting()` — the agent is NOT
  streaming during a compaction, so no turn abort accompanies it.
- `_runAutoCompaction`'s catch maps an abort to
  `_endCompactionUnsuccessfully(reason, "cancelled",
  reason === "requested" ? "Requested compaction cancelled" : "Compaction
  cancelled", { aborted: true })`: the durable `compaction_outcome` row
  carries the message; the `compaction_end` event carries `aborted: true`
  with NO errorMessage/errorSeverity (aborts are user-initiated). The
  aborted arm precedes the skip and failure arms, and the pending request
  stays consumed (taken before the run).
- `_performCompaction` re-checks `signal.aborted` AFTER the summarizer
  resolves and BEFORE the ledger commit: a summarizer that finished while
  the abort raced never lands a committed compaction. Ported as the
  `abort: Option<&AbortSignal>` on `CompactOptions`
  (`throw_if_aborted` before the provider call + `is_aborted` between the
  summary and `append_compaction`); the in-flight cancellation drops the
  request through `race_with_abort` (the daemon layer), mirroring the TS
  provider-stream cancel.
- Rust seams: `AgentSessionEngine.auto_compaction_abort` (the
  `_autoCompactionAbortController` slot; each run registers, only its own
  controller clears), `SessionEngine::abort_auto_compaction()` trait
  method (default no-op for scripted engines), `CompactionManager::abort`
  (manual slot + engine slot), `handle_abort` now aborts the compaction
  like TS `requestAbort`, `run_turn_boundary`/`run_auto_compaction`
  register + race + record the cancelled row through the #207
  `record_compaction_outcome` seam (the `Cancelled` enum arm).
- Requested-run start event: TS `_runAutoCompaction("requested")` emits
  `compaction_start` with the pending instructions before the summarizer;
  the Rust boundary now emits it too (the
  `Agent requested compaction, compacting context... (Ctrl+C to cancel)`
  loader was previously unreachable on the requested path — added
  `TurnBoundaryRequests::scheduled_compaction()` to read the pending
  instructions without consuming).
- TUI: Ctrl+C with the compaction loader up sends `abort_compaction`
  (TS `isAgentCompacting()` -> `abortCompaction()`), not the turn abort —
  the compaction runs between turns, so the interrupt cancels the run
  only. The cancelled outcome row renders as an error status (TS
  CompactionOutcomeMessageComponent: only `skipped` warns).
- Known adjacent deviation (documented, not fixed here): during a session
  `abort` mid-auto-compaction, the worker's existing abort gate
  (`abort_requested` stops consuming engine events) suppresses the live
  row-pair + `compaction_end` broadcasts, while the durable row still
  records (TS broadcasts them; the row is visible on reattach/rebuild).
  The dedicated `abort_compaction` path (the TUI interrupt) has no gate
  and is fully at parity. Also `isCompacting` in the worker's state flag
  is not set during in-turn auto compactions (it is set for manual runs);
  the agents-view roster shows busy instead of compacting there.
- Verifiers: pa-core unit tests (pre-aborted signal never requests;
  late abort cancels before the commit), pa-daemon engine unit tests
  (threshold + requested mid-flight aborts: the cancelled row pair, the
  aborted end event shape, no committed entry, request consumed),
  pa-daemon `tests/compaction_abort_e2e.rs` (real supervisor + worker +
  HTTP mock holding the summarizer; `abort_compaction` over the wire),
  `scripts/compaction_abort_parity.py` (TS vs Rust daemon wire
  differential over the battery mock: compaction_start, the row pair,
  the aborted compaction_end, and the durable rows, normalized diff).

## Print pre-turn compaction arms (print-pre-turn lane, the #214 residue)

- TS `_runPreTurnCompaction` (agent-session.ts) is the FULL
  `_checkCompaction` pass over the last assistant message, called before
  every admitted prompt (`_prepareForCommit` policies: `beforeModelSelection`
  for queued/injected, `afterModelSelection` for direct prompts), not just
  the overflow Case 1 #211 ported. Parameters
  `skipAbortedCheck=false, queueAutonomousContinuation=false` give the
  pre-prompt pass two behaviors the settled boundary does not have:
  - abort arm first: an aborted trailing assistant drops
    `_pendingRequestedCompaction` AND `_pendingRequestedRefine` (the turn
    that would service them never ran; a stale request must not leak into
    the admitted turn), and the check CONTINUES to the later arms.
  - no autonomous-continuation queueing on the threshold arm.
- Arm order in the shared `_checkCompaction` body: Case 1 (overflow, with
  the sameModel / not-before-compaction / enabled-or-pending guards) — any
  Case 1 path returns, so the requested/threshold arms never run in the
  same pass (the overflow run itself consumes a pending model request);
  then `pendingRequestedCompaction !== undefined` ->
  `_runAutoCompaction("requested", false)` with NO enabled/settings guard;
  then the threshold arm (gated on `settings.enabled` and
  `assistantIsFromBeforeCompaction`, tokens via
  `_getThresholdContextTokens` — the full-session estimate with the
  stale-usage guard).
- Rust port (pa-cli `print_boundary.rs`): `run_pre_turn` now runs the
  complete pass — the abort arm (`TurnBoundaryRequests::clear_pending`;
  the TS serialized-refine-plan cancel is daemon-side machinery the print
  runtime does not host), the #211 overflow attempt, and — when Case 1
  stayed silent — the requested and threshold arms. The requested/threshold
  body is shared with the settled boundary
  (`requested_and_threshold_arms`), so both boundaries run the identical
  arms (TS reaches both through the same `_checkCompaction`). A pre-turn
  compaction never re-issues: the admitted prompt continues the loop on
  the compacted context (TS `resumeAfterFailure` / `_runPreTurnCompaction`
  never call `agent.continue()`).
- Resume is the observable surface: a session that ended above the reserve
  headroom (run one, compaction disabled) compacts BEFORE its first
  resumed prompt (run two, `--continue`, compaction enabled) — both
  boundaries settle a crossing turn inside one process, so only a resumed
  session can bring a crossing to a pre-turn check. Pending model requests
  are in-memory only on both sides (TS `_pendingRequestedCompaction`, Rust
  `TurnBoundaryRequests`), so the pre-turn requested arm is a fidelity arm:
  the product flow consumes a `compact.run` schedule at the scheduling
  turn's own settled boundary.
- Known adjacent gaps (documented, not fixed here; the first two surfaced
  by the resume parity scenario, the rest from the code read):
  (1) TS `_scheduleAutoRefineAfterCompaction`/`_scheduleAutoRefineAfterAgentEnd`
  run a harness-state review after every compaction and at turn intervals
  (`autoRefine` settings, default on) — the Rust print runtime hosts no
  auto-refine at all; the parity harness pins `autoRefine: {enabled:
  false}` in its sandbox settings (the product's own switch) so the
  comparison stays scoped to the compaction arms. (2) A split-turn
  compaction (cut inside a turn) makes TWO TS summarizer calls and
  concatenates `summary + "\n\n---\n\n**Turn Context (split turn):**\n\n" +
  prefix`; the Rust pa-core `execute_compaction` folds the prefix into the
  single call — pa-core compaction owns that gap. (3) the settled
  boundary does not run TS's abort-drop (`skipAbortedCheck=true` clears
  the pending requests and returns) — in the print runtime an aborted turn
  never reaches `run_at_settled_turn` in-process (no abort source), and a
  resumed aborted session hits the pre-turn abort arm first; (4) the
  requested/threshold compactions do not note adoption telemetry
  (`note_compaction`) on the print or daemon threshold/requested arms —
  only the overflow arm does; TS counts every `compaction_end` into the
  run's `compactionCount`.
- Verifiers: pa-cli unit tests (the full `_checkCompaction` pre-turn pass
  — the requested arm consuming before the prompt with the TS event pair,
  the threshold arm compacting a resumed session before its first prompt,
  the abort arm dropping pending compaction+refine then continuing; the
  existing settled-boundary tests moved to the faithful mid-turn-request
  shape), `scripts/print_json_parity.py` new `resume` scenario (two runs
  over one session store: run one ends above the headroom with compaction
  disabled, run two resumes `--continue` on its own daemon socket with
  compaction enabled — the pre-turn `threshold` compaction_start/end pair
  precedes the prompt's turn events on both sides, normalized full-stream
  diff), f7/f14 battery unregressed (musl build).


## ipython_state compaction notice (kernel persistence)

- TS ground truth (`AgentSession._syncKernelStateAfterCompaction`, called at the
  end of `_performCompaction` — so every compaction surface: `/compact`, the
  `compact` skill, threshold auto-compaction, overflow recovery): when the
  session's kernel is running (`_ipythonKernelProvisioner.hasRunningKernel`),
  the session (1) prunes variables above the per-variable snapshot limit
  (`pruneOversizedVariables`, `.catch(() => null)`), (2) lists the live
  user-defined names under a 5s abort (`KERNEL_STATE_LISTING_TIMEOUT_MS`,
  `listNamespaceNames`), (3) appends a hidden `ipython_state` custom message
  — `display: false`, no details — with the content
  `[python-state]\n\nYour Python kernel persisted through compaction; its
  remaining variables, imports, and helpers are still available.` + the prune
  sentence (` Variables above the per-variable snapshot limit were removed:
  <names>.`) + the names detail (` These names are still defined: <names>.`
  / ` You have not defined any names yet.`; a failed listing on a
  still-running kernel lands the notice with no detail arm, and a listing that
  failed because the kernel stopped mid-probe lands nothing). The row is
  model context (TS `convertToLlm` keeps it as a user turn — the model is
  told its kernel survived), never rendered (display false), and pushed onto
  `agent.state.messages` BEFORE a trailing error assistant turn (splice, not
  push). `sessionManager.appendCustomMessageEntry` makes it durable, and the
  session `_emit`s its `message_start`/`message_end` pair before
  `compaction_end`.
- The #227 residue: because the notice follows every compaction, the session
  branch never ends on the compaction row while a kernel runs — a
  back-to-back `/compact` with nothing in between prepares again (update
  mode, `previousSummary` merge over empty new history) instead of skipping
  "Already compacted". Without a running kernel the branch does end on the
  compaction row and the skip fires (both products).
- Rust port: `pa-core::session_engine::ipython_state` owns the notice — the
  `CompactionKernelProbe` view of the kernel provisioner (the concrete
  `IpythonKernelProvisioner` implements it; `Arc<dyn>` so tests inject a
  scripted probe), the TS-exact content builder, and the sync (durable row +
  live-context insert-before-error + the row returned on `CompactRun`).
  `AgentSession::compact` runs it after the rebuild; every surface
  broadcasts the pair: the daemon wire `compact` (`CompactionManager`:
  store persist + `session_event` frames), the daemon `/compact` session
  command, the turn-boundary requested arm, threshold auto-compaction, the
  overflow arm (all `EngineEvent::CustomMessage`, which the worker persists
  + frames), and pa-cli print json mode (`message_start`/`message_end`
  before `compaction_end`).
- Kernel prewarm (the adjacent gap, closed by the kernel-prewarm lane):
  TS `createDefaultRuntimeFactory` sets `prewarmIpythonKernel: true`, and
  the session gates it with `rlmDepth === 0` + an active `ipython` tool
  (`agent-session.ts` `_buildRuntime`: `(prewarmIpythonKernel || hasSnapshot)
  && activeToolNames.includes("ipython")` -> `provisioner.prewarm()`, a
  fire-and-forget `void ensure().catch(() => {})`). The Rust port rides
  `SessionEngineConfig::prewarm_ipython_kernel`: the daemon worker and the
  headless/print product path pass `Some(true)` (the TS factory's callers),
  the engine applies the depth-0 + active-ipython gate and fires the
  provisioner prewarm at create; boot failures stay swallowed (the next
  `ensure()` surfaces a fresh attempt — the lazy first-call start intact),
  subagent sessions (rlmDepth > 0) stay lazy, and Rust-only verification
  harnesses (faux print) pass `None`. Two TS arms remain inert by
  construction: the `hasSnapshot` resume prewarm (the Rust session path
  wires `snapshot_dir: None` — no session can arrive with a snapshot to
  revive until kernel namespace snapshots land) and the /reload rebuild's
  dispose+prewarm (no Rust /reload surface yet). The daemon side needed a
  second fix the prewarm verifier caught: the Rust worker built its core
  session lazily on the first demand seam, so even with the flag the
  prewarm fired at the first turn, not at create (a TS daemon session has
  its kernel from creation on; the battery's no-tool-use scenario landed
  the notice on TS only). The wire `create` handler now starts the same
  build in the background at create — TS builds its AgentSession eagerly
  inside the create handler — behind a one-build gate
  (`AgentSessionEngine::session_build`) so the eager build and every
  demand seam meet at one session, while the create response stays
  model-independent (a build failure still surfaces on the first demand
  seam).
- Verifiers: pa-core unit tests (content shape arms, row wire shape,
  capture guards, the append point after the compaction entry + live
  context, the back-to-back second compaction RUNNING again with a running
  kernel and skipping "Already compacted" without one), the provisioner
  prewarm unit test (failure swallowed, `ensure()` retries fresh), pa-core
  live-kernel integration tests (`tests/kernel_prewarm.rs`: a prewarmed
  main session boots its kernel at create — observed through the `kernel
  bootstrap` telemetry event — and a compaction with NO ipython tool use
  lands the notice row with the durable entry; a depth-1 session with the
  flag still set stays lazy and keeps the lazy ipython tool), and the
  battery f7 kernel-notice differential in two scenarios (daemon + mock
  provider, two back-to-back wire `compact` commands each: the #227
  residue scenario — one scripted ipython tool call boots the kernel — and
  the #230 prewarm sibling — no tool use at all, a 15s post-create settle
  window on both sides (a warm sandbox boot measured ~7s), then two text
  turns; both sides must land the notice rows after each compaction
  entry, succeed on the second compact, and send identical update-mode
  summarizer requests).

## Post-compact queued-input suspension (compact-suspension lane, the #227/#233 ruling)

RULING (designed surface, not an artifact): the TS suspension is an
intentional admission gate with a defined, never-timeout lifecycle —
parity requires the same gate and rejection error in Rust; the Rust
worker's previous answer-immediately behavior was a parity bug (#227's
battery already encoded the TS shape with its steer workaround, so the
surface was already half-known).

Ground truth (`agent-session.ts`):

- The flag `_sessionInputPumpSuspended` is set by `requestAbort()` (the
  public `abort`, `abort_and_clear_queue`, and manual `compact()` — which
  aborts first: `await this.abort()`) and by `abortForUpdateRestart()`
  (with a separate `_sessionInputSuspendedForUpdateRestart` marker that
  keeps trigger-turn messages queued during the restart prep; a Rust
  worker restart is a fresh process, so no marker is needed).
- There is no timeout and no next-turn auto-clear. The suspension ends
  only at a resume site: `_resumeSessionInputAdmission()` runs from
  `resumeQueuedWork()` (the daemon `resume_queue` command — which clears
  the suspension BEFORE answering "No queued work to resume" — and every
  applied `mutate_queued_message`), a prompt admitted with
  `resumeIfIdle` (the daemon maps `prompt`/`prompt_and_wait`
  `resumeIfIdle` to `streamingBehavior !== undefined`, and the dedicated
  `steer`/`follow_up` commands pass `resumeIfIdle: true`), cron/heartbeat
  fires (`promptHeartbeat`/`promptUntilAccepted` carry `resumeIfIdle`), and
  the `compact()` success-with-active-goal branch (`resumeQueuedWork()` in
  the `didCompact` finally, `_goalState.status === "active"`).
- While suspended and not streaming, a plain prompt hits
  `_assertSessionActionAdmissionAvailable()` and is rejected with the
  byte-verbatim error `Cannot admit a session action while queued session
  input is suspended.` — the observed "100+ seconds of rejections" was the
  harness retrying plain `prompt_and_wait` with no resume site in between;
  a `steer`/`follow_up` (the TUI's submission path) resumes immediately.
- Agent-message delivery (`acceptAgentMessagePrompt`) runs with
  `resumeIfIdle: false`: on a suspended idle session it is rejected with
  the same error; only the busy carve-out queues it parked
  (`_isBusyForSessionInput`).
- Queued lanes park while suspended (the TS session-input pump refuses to
  schedule): items queued before the abort survive undelivered until a
  resume site fires.
- Auto-compaction (`_runAutoCompaction`: overflow/threshold/requested)
  never aborts, so it never suspends.

Rust port (pa-daemon worker, `SessionCore::queued_input_suspended`):
`abort`/`abort_and_clear_queue`/manual `compact` set it; the turn runner
drains nothing while set; plain `prompt`/`prompt_and_wait` on a not-busy
session is rejected with the TS error; prompts carrying `streamingBehavior`
resume; `steer`/`follow_up`, `resume_queue` (before the empty-queue
failure), applied `mutate_queued_message`, cron/heartbeat fires, and a
successful compact with an active goal resume; agent-message delivery
(`worker_deliver_message`) is rejected while suspended and idle, parked
while busy/compacting.

Verifiers: the f7 suspension-lifecycle differential (wire responses for
compact -> plain-rejected (error byte-equal) -> steer-admitted ->
plain-admitted -> abort -> plain-rejected -> resume_queue ("No queued work
to resume") -> plain-admitted), pa-daemon unit tests for the same surface,
and the existing f7 iterative rows (whose steer-after-compact step is the
TUI's real submission path and doubles as the no-regression check).


## Worker session-end kernel disposal (daemon-kernel-dispose lane, the #235 follow-up)

RULING (read from the shipped TS product): TS disposes a session's kernel
at every session end — the kernel is NOT a TS-designed long-lived per-worker
resource. The chain, end to end:

- `daemon-mode.ts` `closeSessionOnce` (every close reason — killed,
  shutdown, replaced, update, completed) awaits the session's abort
  (`waitForAbort` defaults true; `session.abort()` settles the in-flight
  turn, compaction, and branch-summary runs), then calls
  `state.runtime.dispose(disposal)`.
- `agent-session-runtime.ts` `disposeOnce`/`teardownCurrent` await
  `session.disposeAsync({ kernelSnapshot: options.kernelSnapshot ?? true })`
  (the replacement paths — resume/new/fork/switch — run the same dispose
  through `teardownForReplacement`).
- `agent-session.ts` `_disposeAsyncOnce` awaits
  `this._ipythonKernelProvisioner?.dispose({ snapshot: kernelSnapshot })`.
- `ipython.ts` `IpythonKernelProvisioner.dispose` resolves the pending
  boot, aborts in-flight startups, and `m.shutdown({ snapshot,
  drainHostRequests: true })` — the `python -m rlm.repl` process exits.
- The worker's supervisor-lost exit calls `this.shutdown(0)`, which closes
  every session first; a host hard-killed before its close pass leaves the
  kernel to the orphan journal (`core/orphan-process-journal.ts`, ported).

The `kernelSnapshot: false` policy appears only where the child's artifact
dir is deleted right after disposal (RLM subagent delete/close with
tombstone); the top-level kill/shutdown flushes a final snapshot.

Rust port (pa-daemon): the worker keeps the engine object past the session
end (the #235 engine-drop teardown cannot run there), so every end path
calls the explicit seam — `AgentSessionEngine::dispose_kernel()` ->
`SessionEngine::dispose_kernel()` -> `provisioner.dispose(None)` (default
snapshot policy, host-request drain, the TS `shutdown` semantics):

- `kill`: archive first (TS persist-before-abort), then the awaited abort
  (`abort_requested` + compaction/branch-summary aborts + idle wait for the
  in-flight run to settle), then the kernel dispose, then the
  `session_closed` broadcast.
- `shutdown`: the settle + dispose run in the handler, before the reply
  unlocks `std::process::exit(0)` — the exit runs no destructors, so an
  undisposed kernel would be orphaned with its worker gone.
- the supervisor-lost `exit_orphaned`: the monitor only reaches the exit
  with no session work in flight, so the dispose runs before the exit.

Documented divergence (left as-is, out of this lane's scope): the TS
replacement flows (new_session/switch_session/fork/tree navigation) dispose
the whole runtime — the new session boots a fresh kernel with a fresh
namespace. The Rust port rebuilds the live session's context in place on
the same engine and keeps the kernel (namespace and all) across the swap;
the fresh-namespace-per-replacement TS behavior is a separate parity
question for the tree/fork lanes.

Verifiers: `pa-daemon/tests/kernel_dispose_e2e.rs` — kill, daemon shutdown,
and the supervisor-SIGKILL orphan exit each must leave no live kernel
process (process-table diff against the pre-test baseline; every path ends
the worker process too, so a missing dispose leaves an orphaned kernel that
the diff catches). The seam itself is pinned by the pa-core
`kernel_teardown.rs` dispose tests from #235.


- lane `replacement-kernel`: replacement-flow kernel dispose (TS
  `teardownForReplacement` ruling per flow).
  TS ground truth (`packages/coding-agent/src/core/agent-session-runtime.ts`):
  every whole-runtime replacement — `newSession` ("new"), `switchSession`
  ("resume"), `fork` ("fork"), `importFromJsonl` ("resume") — runs
  `teardownForReplacement` -> `teardownCurrent` -> `session.disposeAsync()`
  before `buildAndApplyReplacement`: the old session's kernel disposes
  (final namespace snapshot flush, then the `python -m rlm.repl` process
  exits), and the fresh runtime built onto the replacement file prewarms a
  cold kernel (its namespace revives only from the moved-to session's own
  kernel-state snapshot, when one exists; a fresh `/new` or a branched fork
  has none — `createBranchedSession` copies no kernel state). The tree
  moves (`navigateTree`) are NOT replacements: TS rebuilds the branch
  context in place (`agent.state.messages = sessionContext.messages`) and
  the kernel stays warm. The prepare/teardown order matters: TS opens and
  validates the replacement session file BEFORE the teardown, so a failed
  prepare (missing switch target, bad fork entry) leaves the live session
  and its kernel untouched.
  Port: the ruling above resolves the divergence #236 had documented.
  The worker runs `teardown_for_replacement` between the prepare and the
  swap for new_session/switch_session/import_jsonl (session_navigation.rs)
  and fork (branch_navigation.rs): cancel the queued session actions
  (TS dispose rejects every queued action), abort the compaction and
  branch-summary runs, settle the turn, then retire the runtime —
  `AgentSessionEngine::retire_session_runtime` disposes the built
  session's kernel and drops the built session (with its mirrored goal
  handles and published goal state) under the build gate, so the next
  demand seam rebuilds a fresh session against the moved file and the
  background `prewarm_replacement_session` build fires the fresh kernel's
  prewarm at the replacement (TS `buildAndApplyReplacement` ->
  `createRuntime`). `navigate_tree` keeps the kernel warm — no teardown
  there, ever. The build funnel (`ensure_core_session_async` /
  `session_agent`) now shares one post-build adoption step
  (`adopt_built_session`: goal mirror, parked depth override, parked
  replacement branch), so a read-seam build cannot strand a parked
  replacement branch (pre-existing latent gap: only the turn-driven build
  consumed it before).
  TS `teardownCurrent` also disposes the session's hosted RLM subagent
  runtimes on replacement — that half is the `rlm-children-replacement`
  lane below (resolved there: the children close with the parent). TS
  `session.reload()`
  disposes the kernel provisioner (a previous provisioner's final snapshot
  flush gates the next read); the Rust `reload` arm is not yet implemented
  (no daemon `reload` command exists) — the ruling will apply when it lands.
  Verifiers: `pa-daemon/tests/replacement_kernel_e2e.rs` — per flow
  against a live kernel: `new_session` and `fork` must turn the kernel
  over (old pid gone, fresh pid alive from the replacement prewarm) and
  the fresh namespace must be COLD (a marker variable set before the
  replacement is gone); `switch_session` must turn over on a prepared
  target AND a missing target must keep the kernel alive untouched (the
  prepare precedes the teardown); `navigate_tree` must keep the SAME
  kernel pid alive with a WARM namespace. Unit: the engine retire rebuild
  (`agent_engine::replacement_teardown_retires_the_session_and_the_funnel_adopts_the_branch`)
  and the worker flow ruling
  (`session_navigation::tests::replacement_flows_retire_only_on_a_prepared_file`).

## Post-compaction goal continue (post-compact-continue lane, the #234 residue)

TS ground truth (agent-session.ts `compact()`'s `didCompact` finally arm):

- On a successful manual compact whose own abort signal is not aborted,
  with `_goalState.status === "active"`:
  `this._goalContinuationAwaitsRlmWork ||= !this.agent.hasQueuedMessages();`
  `this.resumeQueuedWork();`
  `if (this.agent.hasQueuedMessages()) this._schedulePostCompactionContinue();`
- `resumeQueuedWork()` clears the #227/#233 queued-input suspension first,
  then `_maybeResumeGoalContinuationAfterRlmWork()` mints the owed goal
  continuation when the flag is set and the goal is active with an
  objective: `continuationsUsed + 1`, the state change persists
  (`_setGoalState` -> `thread_goal_state`), `_emitGoalUpdate` fires, and
  the continuation context message (customType `goal_context`,
  "continuation") is admitted as a `followUp` prepared turn action with
  `resumeIfIdle: true`.
- `_schedulePostCompactionContinue()` is the scheduled continue: a runner
  that waits for agent idle / retry / refine quiescence / the
  queued-work-resume checkpoint, then drives `agent.continue()` over the
  queued follow-up — this is how the goal keeps driving across a compact.
- TS `agent.hasQueuedMessages()` spans BOTH the steering and follow-up
  queues; the `||=` sets the owed flag only when neither has items, so
  already-parked queued work owns the continue instead of a fresh mint.
- The compact-trigger auto-refine defers behind the continuation (TS
  `_scheduleAutoRefineAfterCompaction(willContinueAfterCompaction = true)`
  -> `_compactAutoRefinePending = true`): the review services at the
  continuation turn's quiescent boundary, not before it.

Rust port (pa-daemon worker `handle_compaction`): the compact success arm
checks `engine.goal_state_value().status == "active"`; with no queued work
parked (both lanes empty), the worker mints the continuation through the
new `SessionEngine::mint_post_compaction_goal_continuation` (the real
engine mirrors the goal runtime handles, runs the pa-core
`GoalDriver::next_continuation_message` mint on the engine runtime — the
pa-core mint now persists the state change like every other driver
mutation — and returns the follow-up turn request plus the mint's
`goal_update` payload, deduped against the engine's published baseline),
emits the `goal_update`, queues the item on the follow-up lane BEHIND the
still-set suspension, then resumes: `resume_queued_input()` clears the
gate and wakes the turn runner, which drains the continuation as the
scheduled continue's turn (the runner IS the scheduled continue — it
waits on the same idle/settle discipline). Parked queued work skips the
mint (`||=` mirror) and owns the resume. The compact-trigger auto-refine
defers when the goal branch ran (the trigger stays armed and services at
the continuation turn's boundary).

Scope rulings (parity boundaries, same class as #234's):

- The TS compact-with-active-goal branch also runs for the `/compact`
  session command and the ACP compaction arms; #234 scoped the
  suspension to the daemon worker's `compact` wire command (the TUI
  submits `/compact` as a steer with `resumeIfIdle`, so the interactive
  flow crosses the same resume site), and this lane mirrors that scope:
  the goal continue lives on the worker `compact` command. The
  engine-side `/compact` execution path has no worker queue surface; it
  stays with the goal-continuation-loop lane.
- The TS mint's `_hasUnsettledRlmQuiescenceWork()` gate (defer the
  continuation while child runs are unsettled) has no Rust equivalent
  yet: nothing in the Rust port sets the owed-continuation flag at turn
  end (the TS goal-continuation loop hook `_getGoalContinuationMessages`
  is not wired — its own lane). The compact branch is the only mint
  site today, and it sets the owed flag itself, so the gate cannot fire;
  wiring the deferral belongs with the goal-continuation loop.
- `tokensBefore`/usage stay out of the wire differential (per-side token
  estimates, the #182 compact_parity normalization).

Verifiers: pa-daemon worker unit tests (compact + active goal schedules
the continue and the suspension clears; queued work skips the mint; a
paused goal never continues), an AgentSessionEngine unit test for the
real mint (persistence, goal-context row, deduped `goal_update`, paused
mint refusal), and the f7 post-compact goal-continuation differential
(a goal session compacted mid-turn: the wire window from
`compaction_start` — compaction pair, minted `goal_update`,
`goal_context` row, continuation turn, `goal.complete()` freeze — plus the
mock's model requests byte-compared TS vs Rust).

Adjacent gaps surfaced by the f7 goal-continue differential (pre-existing,
NOT this lane's surface — reported for their owning lanes):

- Injected-custom turns double-represented in the engine branch — FIXED
  (the injected-turn representation lane): the daemon engine used to run
  an injected custom row's turn on the raw text as a plain user prompt
  (`run_turns(text)` -> `prompt_with_images` -> the loop's user message),
  so the ENGINE session branch carried BOTH the custom entry AND a user
  message with the same text; TS's followUp runs the turn ON the custom
  message and appends only the custom entry. The fix ports the TS seam:
  `AgentSession::prompt_injected_message` admits the custom row itself
  into the loop (TS `_promptInjectedMessage` ->
  `agent.prompt([customMessage])`), so the loop context and the
  compaction walk see the same turn structure as TS; the provider
  request carries the row's user-role view at the loop boundary
  (TS `convertToLlm`). `/goal` start/resume continuations moved onto the
  same seam (`continuation_message` instead of the early durable row +
  text prompt), the ACP surface follows, and the f7 goal-continue
  fixture is honest again (the split-prefix content-matched queue and
  the request exclusion are removed; the compact response compares the
  summary too). The pre-fix effect: the extra user row's ~256-token
  estimate shifted the keep-recent crossing and the TS-verbatim
  pull-back landed the cut mid-turn — a short-session compact split on
  Rust where TS cut whole (the f7 goal-continue fixture observed Rust
  making the split-turn prefix summarizer call, TS one history call).
- The daemon worker never mirrors the engine's `thread_goal_state`
  entries into its own session file (the engine session is in-memory;
  `set_session_file` only feeds the system prompt): a started goal — and
  this lane's minted continuation count — survive in the engine but not
  the durable file a rebuild would replay. TS has one store, so
  `/goal` state is durable there. (Goal durability across worker
  recovery: the goal-continuation-loop / worker-recovery lanes.)
- The TS session file carries a `harness_digest` custom row right after
  the session header; the Rust worker file does not (engine-side digest
  rows exist at cold-context boundaries only). Visible in the f7 session
  captures; no behavioral effect on this lane's window.
- The Rust worker's mid-turn compact abort is eager (lane
  `eager-abort`, the #238 adjacent gap 3 fix): TS `compact()` ->
  `abort()` -> `requestAbort()` cancels the in-flight provider fetch
  immediately through `this.agent.abort()`, while the Rust compaction's
  `wait_for_turn_end` originally only set `abort_requested` — the engine's
  provider request was cancelled at the next streamed event (the emit
  probe), so a compact landing mid-provider-wait (the battery's `delayMs`
  hold) let the response complete on Rust and the aborted turn's assistant
  usage reach the goal accounting (+30 tokens in the f7 goal-continue
  fixture) where TS recorded nothing. The fix is two-layered: the worker's
  abort surfaces (`abort`, `abort_and_clear_queue`, the compaction and
  branch-navigation interrupt-and-settle waits, shutdown, kill, and the
  `cancel_prompt_admission` cancel-owned arm) all funnel through
  `SessionEngine::abort_in_flight_turn()` (the agent mirror the engine
  sets at session build, because the core session's mutex stays held
  across a turn's admission), which aborts the agent's active-run
  controller — every loop await rejects and the fetch cancels; and the
  provider adapter passes a CancellationToken into pa-ai's stream
  options (`StreamOptions::signal`, the TS fetch AbortSignal), cancelled
  by `ModelStream::close` (the `closeIterator` abort callback) and the
  stream's drop, so the HTTP request dies at the transport instead of
  finishing detached behind the pump. The aborted turn settles on its
  aborted message with EMPTY_USAGE (TS `createAbortedAssistantMessage`
  with no partial) and the goal accounting's aborted guard skips it, so
  the f7 goal-continue projection now compares the continuation context's
  "tokens used" line verbatim (the normalization is removed). Verifiers:
  the pa-core stream-seam test (a held faux fetch cancels on close and
  settles on the aborted message) and the pa-daemon engine test
  (`abort_in_flight_turn` mid-provider-wait settles the turn inside the
  60s hold with zero usage).

- lane `replacement-rebinds`: the two #237 pre-existing replacement-surface
  residues — the switch cwd rebind and the fork schedule rebind (TS
  ground truth for both).
  Cwd: TS `AgentSessionRuntime.switchSession` / `importFromJsonl` rebuild
  the runtime with `createRuntime({ cwd: sessionManager.getCwd() })` — the
  TARGET session's recorded working directory (TS `SessionManager.open`:
  `cwdOverride ?? header.cwd ?? process.cwd()`), and
  `assertSessionCwdExists` gates a gone cwd at the prepare (before any
  teardown). `newSession` keeps the runtime's own cwd
  (`createRuntime({ cwd: this.cwd })`). The kernel itself is constructed
  with the session cwd (`new IpythonKernelProvisioner(this._cwd, ...)`),
  not the process cwd — the daemon worker happened to coincide (the
  supervisor spawns the worker process in the session cwd), which is why
  the residue was invisible until a switch crossed directories. Port:
  `PreparedReplacement` carries the target cwd out of the prepare;
  the worker rebinds `core.cwd` + the engine's live cwd slot between the
  teardown and the rebuild (the rebuilt session's kernel-resident tools,
  settings/MCP discovery, and shell-gate driver follow); pa-core threads
  the session cwd into `kernel_provisioner` explicitly. The `cwdOverride`
  stays runtime-resident (TS never persists it into the target header).
  Schedule rebind: TS daemon-mode calls `rebindCronJobsToState(state)`
  after `runtime.fork` (and `refreshReplacedSessionState` after EVERY
  runtime replacement, which registers the artifact partition and rebinds
  again). `AgentCronJobStore.rebindSessionJobs` rebinds EVERY job whose
  `activeSessionId` matches the live session OR whose `sessionFile`
  resolves to the target file — no source filter (plain cron, heartbeat,
  and rlm_heartbeat all rebind), preserving status and source; the new
  binding is the (forked/switched-to) session's
  `{ activeSessionId, sessionId, sessionFile, cwd }`. On fork the jobs
  move to the forked session file so a future restore targets the fork,
  not the source branch; the cwd and the daemon-local active session id
  stay (TS `forkFrom(_, this.cwd)` keeps the runtime cwd). Port:
  `Worker::bind_scheduled_jobs` runs at create, after every navigation
  replacement swap, and after the fork swap;
  `Worker::refresh_replaced_session_state` ports the rest of
  `refreshReplacedSessionState` (depth re-seed from the moved-to file, RLM
  identity re-derive without a create-inherited max-depth — the TS
  replacement runtime carries no `runtimeMetadata`, so the persisted chat
  override -> global -> env -> default precedence applies — plus the wire
  summary and status-line re-seed).
  Verifiers: unit
  `session_navigation::tests::switch_session_rebinds_the_worker_cwd_and_new_session_keeps_it`
  (the get_state summary cwd follows the switch target; a gone stored cwd
  fails at the prepare with the TS `MissingSessionCwdError` text;
  `new_session` keeps the cwd) and
  `session_navigation::tests::fork_rebinds_the_scheduled_jobs_onto_the_forked_session`
  (the cron job's sessionFile/sessionId follow the fork); live-kernel e2e
  `replacement_kernel_e2e::switch_session_rebinds_the_kernel_cwd_onto_the_target_session`
  (a session created in `alpha/` switches onto a file recording `beta/`;
  the post-switch kernel's `os.getcwd()` receipt is `beta`).


## Durable thread_goal_state mirror + recovery rehydration (goal-state-persist lane, the #238 residue)

TS ground truth (agent-session.ts, goals.ts): the goal state lives in ONE
store with the transcript. `_setGoalState` -> `_persistGoalState` appends
the `thread_goal_state` custom entry to the session branch and
`flushNow()`s it (durable before the first assistant response), so a
started goal, its usage counters, and `continuationsUsed` survive any
rebuild; the constructor rehydrates with
`this._goalState = this._loadPersistedGoalState()` (branch scan
newest-first, `isPersistedGoalState` validation, `normalizeGoalState`),
silently — construction never emits. `_reloadGoalStateFromBranch({
monotonicTokens })` after a context rebuild never lets the same goal's
counters regress. The owed-continuation flag
(`_goalContinuationAwaitsRlmWork`) is in-memory only and does NOT
persist — a killed worker's parked continuation is restored by the queue
journal or dropped, like TS.

Rust port (the daemon split: the worker owns the session file; the
engine's session manager is in-memory):

- Mirror: every `goal_update` announcement persists the announced state
  as a `custom` row (`customType: thread_goal_state`, `data: <GoalState>`)
  in the worker session file — the turn emit closure in `worker.rs`
  (engine events flow through it under the core lock) and the compact mint
  site (the mint runs outside a turn, so its row rides the mint branch
  next to `emit_worker_event`). The announcement only fires on real state
  change (`goal_update_if_changed`), which matches TS `_setGoalState`
  (every persisted transition also emits). pa-core standalone consumers
  (print mode) keep the direct `SessionManager` persistence — no change.
- Rehydration: the engine's build adoption
  (`adopt_built_session`, every build path) seeds the fresh
  `GoalDriver` with `GoalDriver::restore_from_persisted` from the moved
  branch's entries (a pre-build tree navigation: faithful branch
  semantics, the TS `_reloadGoalStateFromBranch` plain move) or the
  session file's latest valid `thread_goal_state` entry
  (`goal_state_persist.rs`, branch scan + validation + normalize), and
  seeds the published baseline so the rehydrated state never announces
  itself. Wall-clock accounting restarts for an active goal (the TS
  constructor's `_goalAccountingStartedAt = Date.now()`).
- Not ported here (documented adjacent gaps, other lanes): the
  goal-continuation loop hook at natural turn end (nothing sets the
  owed-continuation flag yet — the compact branch is the only mint site),
  the recovered engine's empty transcript branch (a post-recovery compact
  can skip "Session is too short" even when the durable file is long),
  and compaction-boundary reload rules (`_reloadGoalStateFromBranch`
  monotonic tokens) — the daemon engine keeps its driver across an
  in-process compaction, so no reload is needed there.

Verifiers:

- pa-core unit: `GoalDriver::restore_persisted` +
  `restore_persisted_adopts_the_state_without_rewriting_it` (counts
  adopted verbatim, accounting anchor by status, next continuation
  continues the persisted count).
- pa-daemon unit: `goal_state_persist` reader (latest-valid-entry-wins,
  invalid data skipped, missing/invalid files -> None, branch-entry scan
  matches the store read); engine
  `recovery_rebuild_rehydrates_the_goal_from_the_session_file` (fresh
  engine over a prepared file rehydrates objective + counts, one
  usage-accounting announcement from the rehydrated base, mint continues
  the count); worker dispatch `goal_update_events_mirror_the_durable_goal_row`
  and `compact_mint_persists_the_goal_state_row` (the two mirror sites
  write the durable row; ScriptedEngine goal section gains
  `emitUpdateOnPrompt` and the session-pathed store fixture).
- e2e `goal_recovery_e2e::killed_mid_goal_worker_rehydrates_the_goal_with_counts`
  (the f21 pattern, faux-scripted real engine): `/goal` start persists the
  durable row; a compact mint bumps `continuationsUsed` to 1 durably and
  announces it; SIGKILL the worker pid; the respawned worker serves the
  recovery prompt and `get_connection_state` reports the goal active with
  the preserved objective and `continuationsUsed: 1`; the recovery's
  accounting announcements carry the rehydrated count (never reset); the
  durable rows keep growing with objective and count intact.


- lane `rlm-children-replacement`: RLM children lifecycle on a parent
  runtime replacement (the #237 flagged divergence; TS ruling).
  TS ground truth (`core/agent-session-runtime.ts` +
  `modes/daemon/daemon-mode.ts`): every whole-runtime replacement flow
  (`newSession` / `switchSession` / `fork` / `importFromJsonl`) runs
  `teardownForReplacement` -> `teardownCurrent`, which disposes the
  session's kernel first and then `disposeHostedSubagentRuntimes`: the
  daemon host's `disposeRlmSubagentRuntimes` runs
  `closeChildSessions(parentState, "replaced")`, closing every resident
  child session (`getChildActiveSessionStates`: `metadata.parentActiveSessionId
  === parent.activeSessionId`) recursively through grandchildren. So TS
  children do NOT survive a parent replacement: they are archived, aborted,
  disposed, removed from the session map, and the replacement session's
  roster starts empty (a fresh `AgentSession` has no `_activeRlmChildRuns`).
  The close is a plain stop — `closeSessionOnce("replaced")` archives the
  child session file and aborts its in-flight work, but only
  `recordRlmSubagentDeletion` (explicit delete/cancel) tombstones the
  ledger, so the spawn edge and the passive roster row survive the close.
  `rlm.create_session` depth-0 root sessions carry no
  `parentActiveSessionId` and SURVIVE the replacement. A child-close error
  rethrows out of `teardownCurrent`: the replacement command fails with
  the old runtime already disposed. The same `closeChildSessions` cascade
  runs at `kill` (`closeSession(state, "killed")` — default
  `cascadeChildren: true`) and at shutdown (`closeSessionOnce("shutdown")`
  still calls `runtime.dispose` -> `disposeHostedSubagentRuntimes`).
  Ruling: the Rust redesign hosts each child as its own supervisor-owned
  worker, so the close ports as a signal through the supervisor.
  Port: `SupervisorChildSessions::close_children` (rlm_children.rs) stops
  every tracked child through the supervisor link — a `kill` with NO
  `rlmLedgerDelete` marker (a stop, not a delete: the ledger edge and the
  passive roster row survive, mirroring TS), suppresses the terminal
  notices (no notice is owed to a session being torn down), ends the
  settle watchers (`closed_by_parent`), and treats a child whose session
  is already gone as a completed no-op (the TS `sessions.has` early
  return); every other close failure propagates after the walk, exactly
  like TS `closeChildSessions` collecting the first error. The worker
  runs the close at `teardown_for_replacement` (after the kernel retire —
  TS `disposeAsync` precedes `disposeHostedSubagentRuntimes`; a failure
  fails the replacement command), at `handle_kill` (best-effort, like the
  TS daemon kill handler's `.catch(() => undefined)`), and at
  `handle_shutdown` (best-effort; TS close at shutdown runs the
  hosted-subagent disposal through `runtime.dispose`). Because the close
  reaches a child through the child worker's own `kill` handler, each
  child closes its own children first: the cascade to grandchildren is
  the kill route's recursion, matching the TS `closeSessionOnce` cascade.
  The harness's `childScript` create-config key (the TS analog: the child
  runtime inherits the parent's `sessionConfig`) lets a scripted parent
  spawn scripted children through the real worker path, and the key rides
  the replacement identity rebind (the replacement session's children
  stay scripted). Documented adjacent gaps (out of this lane's scope): a
  parent worker killed with SIGKILL cannot close its children (TS
  children die with the in-process parent); the supervisor's parent-death
  cleanup is the follow-up seam.
  Verifiers: `pa-daemon/tests/rlm_children_replacement_e2e.rs` — a real
  supervisor, a real parent worker whose kernel cell spawns the child
  through the product `rlm.spawn` surface, and a scripted child held
  mid-run: `new_session` must close the child (supervisor roster drops
  it, the child session file archives, `get_rlm_children` reads empty,
  and the replacement session's kernel `rlm.list_subagents()` returns
  `[]`), while an `rlm.create_session` root session must SURVIVE the
  same replacement. Mutation-checked: disabling the replacement
  close fails the e2e. Unit: `rlm_children::watch_tests` — the close
  carries no delete marker, an already-gone child is a no-op, a real
  close failure keeps the child tracked, and a closed child delivers no
  terminal notice.


- lane `parent-death-cleanup`: supervisor parent-death child cleanup
  (the #246 documented adjacent gap; TS parity ruling). TS ground truth
  (`modes/daemon/daemon-mode.ts`): RLM children are hosted in the parent
  session's process (`createRlmSubagentRuntime`), so they die WITH the
  parent — a SIGKILLed parent takes its children down, and the durable
  spawn ledger keeps each closed child as a passive roster row. The #246
  Rust close (`SupervisorChildSessions::close_children`) runs inside the
  parent worker's teardown paths (replacement, `kill`, `shutdown`), all
  of which SIGKILL bypasses, so a hard-killed parent would leave its
  supervisor-owned child workers running as orphans. Ruling: the
  supervisor's worker-death monitoring is the only component that
  observes the death, so the close ports there — on an unexpected exit
  (`watch_worker`'s crash arm, before the restart backoff), every
  resident worker whose durable create names the dead worker as its
  parent stops with it. The join is the durable create's
  `runtimeMetadata.parentActiveSessionId` against the dead worker's
  `rootActiveSessionId` (the exact TS `getChildActiveSessionStates`
  predicate; a depth-0 `rlm.create_session` root carries no parent link
  and never matches, and a resumed/forked copy of the parent file under
  another worker must not adopt another worker's children). The close
  per child is the same wire action the #246 `close_children` issues:
  a supervisor `kill` route with NO `rlmLedgerDelete` marker (a plain
  stop — the ledger edge and the passive roster row survive, mirroring
  TS `closeSessionOnce`'s no-tombstone close) plus the supervisor-side
  kill completion (`stop_worker`: registry/roster removal + ledger
  reseed, so the child passivates). Routing through the child worker's
  own kill handler keeps the grandchild cascade (the kill route's
  recursion, the TS `closeSessionOnce` cascade). Best-effort like the
  daemon kill handler's swallowed close error: a failed close logs and
  leaves the child resident (its durable parent link stays joinable for
  a later pass or explicit kill); the walk never blocks the crash
  recovery. The respawned parent replays its durable create and starts
  with a fresh in-process registry (the #246 replacement ruling: the
  roster starts empty; the closed children surface as passive ledger
  rows). Telemetry: `daemon event` kind `worker_children_closed` with
  the close count (schema additive).
  Verifiers: `pa-daemon/tests/rlm_children_parent_death_e2e.rs` — a real
  supervisor, a real parent worker whose kernel cell spawns the child
  through the product `rlm.spawn` surface, a scripted child held
  mid-run, then SIGKILL of the parent worker process (pid read from the
  durable descriptor, environ-checked): the child's resident roster row
  drops, its session file archives, the respawned parent's
  `get_rlm_children` reads empty, `list --all` shows the child as a
  passive ledger row (spawn edge intact, no delete record), while an
  `rlm.create_session` root session SURVIVES the same hard kill. The
  #246 replacement e2e stays green (the death close is the supervisor
  arm of the same close semantics). Mutation-checked: disabling the
  death close fails the e2e.


## Direct-ACP goal continuation (acp-continuation lane, the #244 ambiguity)

TS ruling (the direct, non-daemon ACP embedding): the TS ACP mode DOES
host the goal continuation loop — not as a mode-level construct, but
inside the session's own turn run. `AgentSession` installs
`agent.getContinuationMessages` (`_installAgentContinuationHook`); the
agent loop consults the hook at its natural turn end (`runLoop`); the
in-process `promptAndWait` runs that loop directly, so the direct ACP
drives every continuation INSIDE the one `session/prompt` request — the
continuation turns surface as events of the same prompt turn, and the
response settles only after the goal run ends (complete / paused /
budget_limited / error, or nothing more to do). The daemon-attached
transport rides the worker's queue instead (the #244 lane); the TS ACP
mode itself never re-prompts or owns a queue — evidence:
in-process-agent-connection.ts `promptAndWait` -> `session.promptAndWait`,
agent-loop.ts lines 429-443, agent-session.ts 1898-1899 / 4044-4085.

Rust port (`crates/pa-daemon/src/acp/goal_continuation.rs` + the prompt
turn's settle loop + the arms): the direct ACP's settle loop consults
the same TS arms per settled boundary, with the goal taking exclusive
priority over the autonomous arm:

- Budget-limit wrap-up steer: the usage listener arms the crossing
  (`record_assistant_usage` -> `BudgetReached`; error/aborted turns are
  excluded like TS `_accountGoalUsageForAssistantMessage`), the boundary
  mints the budget-limit context and runs it as the prompt's next model
  segment (TS `_shouldStopAfterTurn`'s budget arm, the steer with
  `resumeIfIdle`).
- Natural continuation mint (TS `_getGoalContinuationMessages`): an
  active goal mints one goal-context turn per settled boundary and runs
  it as the next turn of the same prompt via the injected-row admission
  (the #240 single representation). The mint's `continuationsUsed` bump
  publishes `_meta.goal` before the turn starts (TS `_setGoalState` ->
  `_emitGoalUpdate`).
- Terminal error: a failed turn fails the goal (TS
  `_finishGoalForTerminalAssistantMessage` at `agent_end`; an abort
  keeps the goal).
- Threshold-arm goal queue (TS
  `_queueGoalContinuationForThresholdCompaction`): a crossing turn with
  an active goal mints BEFORE the compaction runs (the mint's goal
  frame precedes the compaction frames, the TS event order), the held
  turn runs as the post-compaction turn (TS
  `_schedulePostCompactionContinue`), a cancelled compaction withdraws
  the mint with a slot rollback (TS
  `_clearQueuedGoalContinuationAfterCancelledThresholdCompaction`), and
  skip/failure keep it (TS `resumeAfterFailure`). The pre-turn check
  never queues (TS `_runPreTurnCompaction` passes
  `queueAutonomousContinuation = false`). A held continuation defers the
  compact-trigger review to the continuation turn's own boundary (TS
  `_scheduleAutoRefineAfterCompaction(willContinue)`).
- Compact-with-active-goal continue (TS `compact()`'s `didCompact`
  finally arm, the #238 residue scoped to this lane): a successful
  `/compact` session command with an active goal mints the continuation
  (the `||= !hasQueuedMessages()` arm — the direct ACP never queues work
  behind a session command) and runs it as the command turn's model
  segment.

Two TS gates hold trivially on this surface and therefore have no port:
the RLM quiescence deferral (`_hasUnsettledRlmQuiescenceWork` — the
in-process engine runs with `NoRlmChildren`, so no descendant work can
exist) and the queued-input deferral/arrival-epoch rollback
(`queuedActionCount > 0` — the ACP connection admits one prompt turn at
a time, so no session input can queue behind the running turn).

Adjacent gap, NOT this lane's surface: the direct ACP rejects a second
`session/prompt` while a turn is live where TS queues it behind the
injected work with follow-up semantics (acp-mode.ts lines 881-884); the
one-prompt-turn admission is the pre-existing #229 surface. Also out of
scope: the print mode's goal surface (no continuation loop there
either; the #241 goal-state persistence works, the loop needs the same
settle-hook pattern in the print runner — its own lane).

Verifiers: `pa-daemon` unit `acp::goal_continuation::tests` — the
natural loop (one mint per settled turn, every continuation inside the
one prompt request, the failed turn fails the goal), the budget steer
(the steer runs as the second segment, `budget_limited` settles
`end_turn`, no slot consumed), the threshold queue (the mint frame
precedes the ran compaction, the held turn runs after it), the compact
continue (the minted segment runs after a ran `/compact`, the
compact-trigger review services at its boundary), and the cancelled
arm (the mint rolls back). `pa-cli` e2e
`acp_goal_command_publishes_goal_meta_and_runs_the_continuation`
pinned to the budget-bounded loop. Differential: the
`acp_compaction_parity.py` "goal" scenario byte-compares the TS binary
vs the Rust direct ACP for the `/goal --budget 5` prompt — the
goal-start continuation segment, the `budget_limited` flip, the
wrap-up steer segment, and the `end_turn` response (8 frames identical
after the #182 token-estimate normalization). The threshold/overflow
scenarios stay green (the arm surface is unregressed).

## Print-mode goal continuation (print-continuation lane, the #252 residue)

TS ruling (probed against the installed TS binary over the shared
faux-provider harness, `prime-agent --mode json --goal <objective>
[--goal-token-budget <n>] -p <prompt>`): the print path DOES run goal
continuations inside the one print invocation. `--goal` seeds the goal at
session construction (main.ts passes `initialGoal` only for depth-0; the
TS constructor seeds only a branch with bootstrap entries and no persisted
goal) and queues the goal-context continuation row into
`_pendingNextTurnMessages`, so it rides the FIRST turn ahead of the user
row. The session's message-end handler records usage
(`_accountGoalUsageForAssistantMessage`) and publishes `goal_update`; the
budget crossing queues the `[goal: budget-limit]` wrap-up steer as session
input (`session_action_update` with the queued steering preview) between
the crossing turn's `message_end` and `turn_end`; the agent loop's
`getContinuationMessages` hook (installed by
`_installAgentContinuationHook`) mints one continuation context per natural
turn end and runs it INSIDE the same agent run (`turn_end -> goal_update ->
turn_start`, no run boundary); a requested compaction or a threshold
crossing stops the loop at `_shouldStopForThresholdCompaction`, the
threshold arm minting its continuation BEFORE the compaction (the bump
precedes the compaction frames); a failed terminal assistant message fails
the goal AFTER `_checkCompaction` (the error `goal_update` follows the
run's `agent_end`). Evidence: the two probe captures
(`/tmp/ts-goal-probe.jsonl`, the budget-bounded run; the unbounded run
loops natural mints to the queue-exhaustion error).

Rust port:
- pa-agent: the continuation hook is settable after construction
  (`Agent::set_continuation_hook`, the TS `agent.getContinuationMessages`
  seam) - the embedding that owns the goal arms wires it once its state
  exists.
- pa-core (`session_engine::goal_boundary.rs`): the engine owns the arms -
  `seed_initial_goal` (the TS constructor seed + the next-turn row queue,
  `AgentSession::queue_next_turn_row` = `_pendingNextTurnMessages`),
  `record_goal_usage`, `goal_budget_limit_steer`, `mint_goal_continuation`,
  `rollback_goal_continuation_mint`, `fail_goal_for_terminal_error`. The
  transports drive the one goal driver through these methods.
- pa-cli (`print_goal.rs`): the print surface wires an in-loop hook on the
  agent - the natural mint runs continuations in-run (full TS framing);
  queued input (the armed steer), a pending requested compaction, or a
  threshold crossing defer to the turn boundary (the threshold arm mints
  its held continuation ahead of the boundary's compaction); the driver's
  boundary drain runs the steer and the held continuation as this
  invocation's follow-up turns with the TS `session_action_update` phase
  frames (queued preview, preparing/committing/running, drain), and a
  terminal error fails the goal after the arms. The goal owns the boundary
  exclusively (the autonomous arm is never consulted while a goal is
  active, TS `_getContinuationMessages`). Text mode runs the same loop
  silently.
- `--goal`/`--goal-token-budget`: parsed since the CLI lane; the print
  runtime now consumes them (the seed runs before the stream wires, so the
  construction-time state never announces itself on the json stream).

Verifiers: `pa-cli` unit `print_goal::tests` - the seed riding the first
turn (slot zero, ahead of the user row, silent), the branch-seed gate, the
natural loop (one agent run, one mint per settled turn, terminal-error
fail), the budget steer (budget_limited, the queued preview + phase frames,
a second run, no slot consumed), and the threshold hold (a resumed session
with an active goal: the mint's slot bump immediately precedes the
compaction, the held context row follows it, queue frames, post-compaction
context back under the headroom). Differential:
`scripts/print_json_parity.py` gained two scenarios - `goal-budget`
(33 events byte-identical after the volatile scrubs) and `goal-natural`
(36 events) - and the existing scenarios stay green (stream, threshold,
resume, compact-refine, compact-refine-decline). The normalizations are
the #182 class only: `tokensUsed`/`timeUsedSeconds`/`createdAt`/`updatedAt`
plus the goal-context text lines (each side estimates its own context).

Adjacent gaps, NOT this lane: print-mode session-command execution
(`/goal ...`, `/compact ...` as prompts are parsed but dropped in the
Rust print path where TS executes them through the session's command
queue); the compact-with-active-goal continue arm (TS `compact()`'s
didCompact finally arm) rides that missing session-command surface; the
autonomous continuation loop keeps its separate-run framing (pre-existing,
unverified against TS).


## agent_end wire frame with the run messages payload (agent-end-messages lane, the #250 residue)

TS ruling (read from the TS source plus probed against the installed TS
binary, `scripts/battery/agent_end_messages_probe.py`): `agent_end`
carries the run's whole message set - the per-run accumulation of the
packages/agent agent-loop `newMessages` (prompt rows with the harness
digest riding as a custom row, assistant rows, tool results, in-run
steering/follow-up/continuation rows). TS emits ONE `agent_start` +
`agent_end` pair per agent run: retried runs (provider retry) and
continued runs (compact-and-continue, `agent.continue()`) each restart
with their own pair, and the continuation run's `agent_end` payload
carries ONLY that run's messages (the failed row left the loop context
first). This resolves the #250-flagged per-runner-item divergence BY the
accumulation: the Rust worker's trailing synthesized frame is a fallback
for runs that ended without a model turn (session commands, pre-model
failures), and a run whose `agent_end` the abort gate swallowed stays
silent exactly like TS (the compact path's detached run emits none at
all). The wire `agent_end` is `{type, messages}`.

A second TS ruling on the same surface: `promptAndWait` settles the
agent-message completion AFTER the whole turn settle
(agent-session.ts settles `completion` inside the input-pump arm, after
`_startPreparedTurnActions` fully unwinds). The Rust worker resolved the
`prompt_and_wait` waiter at the engine `Done` event, inside the turn
task - a window where the session is still mid-unwind (`core.busy`
still set, queue projection pending). A client whose follow-up request
landed in that window hit the suspension gate with busy set, so the gate
queued it behind the (indefinite) suspension instead of rejecting it
with the TS admission error - the f7 suspension sequence's post-abort
prompt hung exactly there. The turn task now parks the settled outcome
and `run_turn` resolves the waiter after the idle flip, roster delta,
boundary frames, queue projection, and admission bookkeeping.

Rust port:
- `EngineEvent::AgentStart/AgentEnd`: the engine forwards the loop's
  run-boundary events in the session wire shapes (`session_wire_value`
  converts custom rows too - the harness digest rides the payload).
- The worker's run-opening `agent_start`/`turn_start` forward only once
  a boundary frame already passed in the item, so the worker's own
  frames stay the first run's and retried/continued runs re-open with
  their own frames.
- ACP turn settlement resolves on the turn's LAST `agent_end` (a
  retried turn restarts its runs; an early marker resolution would let
  the trailing retry frames trail the settlement).
- run_turn_once: a turn whose admission already settled must not re-poll
  the completed `pin!` future (pin! futures panic when resumed after
  completion; the compact-abort drain surfaced it deterministically).
  Gate the abort wait on `!settled`.

Verifiers: engine tests pin the settled payload ([digest, user,
assistant]), the retried per-run frames/message sets, and the aborted
run's `agent_end`; worker tests pin the wire frames (per-run
agent_start/turn_start/agent_end, the Done-only fallback pair, the
compact swallow) and the post-settle waiter resolution (the waiter fires
only after the idle flip); the healing-retry e2e pins three `agent_end`
frames with per-run messages; the probe
(`scripts/battery/runs/agent-end-messages-probe-20260921T1331Z/`)
byte-compares the `agent_end` frames plus run-boundary fingerprints
(normalized timestamps + JS stack traces) on settled, retried, and
continued runs against the TS binary - all MATCH. Known follow-up (out
of lane): the retried boundary sequence diverges on `auto_retry_*`
placement (TS resets the retry counter at the successful `message_end`
hook; the Rust retry driver closes `auto_retry_end` after the attempt
boundary).


## pa-ai: Responses input-item ids never empty (dogfood P0, TS ruling)

TS ground truth (`packages/ai/src/providers/openai-responses-shared.ts`,
`convertResponsesMessages`, verified by driving the TS package directly with
`npx tsx` on degenerate ids; goldens mirrored in
`openai_responses_shared.rs::tests`):

- `function_call` item: `toolCall.id.split("|")` with no `|` leaves the
  item id `undefined`, so the `id` key is OMITTED from the wire JSON (never
  `""`); `call_id` is the whole id. A different-model message with an `fc_`
  item id also omits `id` (the Rust port previously sent `null` there, and
  `""` for the absent case - the live dogfood failure: follow-up turns
  carried `input[N].id: ""` and the API rejected the turn with
  `[ApiParam][invalid_id]`).
- `message` item: without a usable text-signature id the TS synthesizes
  `msg_${msgIndex}` where `msgIndex` counts every converted message
  (user/assistant/toolResult). The Rust port previously sent `""`.
- `function_call_output.call_id`: the TS sends `split("|")[0]` verbatim,
  including `""` for an empty tool result id; no fallback exists upstream.
  The Rust port matches that exactly - an empty `call_id` is only reachable
  when the tool call id itself was empty, which the agent layer never
  produces (tool results inherit the tool call id verbatim).

Deliberate narrowing (documented, not invention): a `|`-terminated id with
an EMPTY item segment (`"call_x|"`) folds into the omitted-`id` path. The
TS emits `id: ""` there, but that shape is unreachable in TS (its stream
template interpolates `undefined` for missing ids); it IS reachable in Rust
(the stream uses `unwrap_or("")`), and the API rejects `id: ""`, so the
guard treats an empty segment like an absent one.

Ambiguity found (out of lane, reported): `response.output_item.done` in
`openai_responses_stream.rs` removes the slot BEFORE the
`current_slot(output_index)` lookup, so the `done` finalize paths are dead
(reasoning signatures are never recorded from `done` items; arguments are
already set by the `function_call_arguments.delta/done` events, which is
why tool calls still work). Effect: reasoning items are not replayed on
follow-up turns (token waste, no API error). The TS uses `currentBlock`
(set at `added`, not slot-lookup) and does record the signature. Needs its
own verified lane; not changed here.
