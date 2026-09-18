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
