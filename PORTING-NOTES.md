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
