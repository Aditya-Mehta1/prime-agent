# Parity battery (standing live A/B harness)

A rerunnable battery that drives the installed TS `prime-agent` binary (ground
truth) and the Rust build through the same real user flows side by side, with
both binaries pointed at one deterministic mock provider so model responses are
identical. It captures tmux frames, session transcripts, provider wire
requests, and daemon wire traffic, and prints a gap report.

One-command re-run (from the repo root, after `cargo build --release`):

    python3 scripts/battery/run_battery.py

Useful options: `--flows f2_prompt,f5_side_questions` (run a subset),
`--rust-bin PATH`, `--ts-bin PATH` (default `prime-agent` on PATH),
`--runs-root PATH`. Evidence lands in `scripts/battery/runs/<UTC stamp>/` with
per-side flow directories, `report.md` (human gap report), and
`findings.json` (machine-readable).

## Harness pieces

- `scripts/battery/mock_provider.py` - deterministic OpenAI-compatible SSE
  server (`/v1/chat/completions`, `/v1/models`) driven by a JSON script; every
  request body is logged for wire diffs. The script file is reloaded on
  change, so one long-lived mock instance serves per-flow scripts.
- `scripts/battery/batterylib.py` - isolated per-side environments (fresh agent
  dir, short TMPDIR, scrubbed `PRIME_AGENT_INTERNAL_*`/`RLM_*` markers),
  tmux frame capture at 120x36 (own `vbat*` sessions only), and a JSONL
  daemon-wire client (protocol 7).
- `scripts/battery/run_battery.py` - the flows and the report.

Both binaries point at the mock through a `models.json` provider named
`prime-inference` (base URL = mock, `apiKey` on the provider entry): both
sides resolve the provider/model from `models.json` plus the CLI flags/wire
create config, and the API key through the registry (auth storage, then the
models.json `apiKey`), with `PRIME_API_KEY` as the shared env fallback. The
Rust side no longer needs the env-var model workaround (gap B-1 fixed).

## Flows

| flow | what it drives | capture |
|---|---|---|
| f1_launch | fresh install state: splash, first-run notice, first prompt + reply | tmux frames, mock request log (B-1 row: flagged model reaches the request) |
| f2_prompt | headless print mode: one prompt, one model response | stdout/exit, mock request bodies, session files |
| f3_tool | a tool-call turn (`ipython` in both) | stdout, session files, entry-type diff |
| f4_commands | the `/` slash-command menu + one benign run (`/session`) | tmux frames |
| f5_side_questions | `start_side_question`/`abort_side_question` over the daemon socket | wire transcript, event stream, post-turn status-line requests (B-7 row) |
| f6_attach | wire-level attach (snapshot + event stream) and CLI `attach` in tmux | attach response shape, events, frames |
| f7_compaction | daemon `compact` on a grown session | compact response, session entries |
| f8_resume | headless session persisted, then print `-c` + interactive `--resume` | stdout/exit (B-11 row: both sides refuse an active session), session shape diff |
| f9_agents_view | the agents view over a scripted roster (running + idle live sessions, one saved-catalog session): section grouping, open-to-attach on the running row, and a normalized TS-vs-Rust frame diff at 120x36 and 220x50 | tmux frames + wire responses |
| f14_compact | `/compact` in the attached TUI (session A): the compaction loader and the durable `◆ Context compacted` summary row; the auto-compaction threshold crossing (session B): the `Auto-compacting...` loader and the compacted row when one mock-reported 126k-usage turn crosses the 500-token reserve headroom. Settings shape the fixture: `reserveTokens` 127500 + `keepRecentTokens` 10 keep the seeded turns compactable (default 20000 skips with "Session is too short to compact") | tmux frames (per key moment, normalized frame diff), mock request log |
| f15_a2a | sibling agent-to-agent messaging over the daemon: a sibling's delivered message (`◆ Agent message received` row) and the sender's `◆ Agent message sent/queued` row, both with participant labels | tmux frames (normalized frame diff), wire prompt logs |
| f16_refine | kernel-scheduled `refine.run()`: the `◆ Harness refined` outcome row and the `[harness-digest]` boundary message on the next turn | tmux frames (normalized frame diff) |
| f17_slash_model | `/model` and `/effort`: the selector overlay open, a selection through the picker (`Model: <id>` confirm row), and the thinking-level picker | tmux frames (normalized frame diff) |
| f18_goal_autonomous | `/goal` lifecycle (start context row, status, pause/resume, kernel `goal.complete()` completion row) and `/autonomous on|off` status rows | tmux frames (normalized frame diff) |
| f19_heartbeat | `/heartbeat` set status row, the fired `♥ Heartbeat prompt · every <schedule>` row, and the `/heartbeats` manager view | tmux frames (normalized frame diff) |
| f20_subagents | kernel `rlm.spawn()`: the subagent summary line above the editor (live counts), the child's `RLM child status` no-reply terminal notice, and the scoped agents view opened from the focused summary line (child listed by name) | tmux frames (normalized frame diff), roster/agents-view frames |
| f21_worker_recovery | worker crash recovery: SIGKILL the session's worker (pid from the daemon session summary), then keep using the session through the attached TUI — the next turn must complete with the transcript intact, and the session summary must show a new ready worker pid | tmux frames (post-kill + recovered, normalized frame diff), get_state wire snapshots |

Flows f10-f13 (perf, provider failure, scroll, ctrl+c exit) predate this table
entry; f14-f21 are the real-surface flows: each drives one product surface end
to end (daemon session + attached interactive TUI), captures the frame at the
key moment, and byte-diffs TS vs Rust after normalization. A flow whose diff
fails on a surface known to be missing in the Rust build records its finding
with the owning fix lane and the report lists it as EXPECTED-FAIL.

First full committed run: `scripts/battery/runs/20260916T210320Z/`
(11 gaps, 18 passed checks). Worker-timeout/socket-path evidence:
`scripts/battery/runs/20260916T203149Z/`. Extra hand-captured evidence lives
in `runs/20260916T210320Z/extras/`. Latest run:
`scripts/battery/runs/20260916T221725Z/` (10 gaps, 21 passed checks - the
B-1/B-7/B-11 rows flipped to passed).

Full battery after the model-surface lane (B-4/B-5/B-6 fixed):
`scripts/battery/runs/20260916T224512Z/` (7 gaps, 18 passed checks; the
system-prompt comparison now normalizes per-side paths, session UUIDs, skill
locations, and skill enumeration order - see `normalize_system_prompt` in
`run_battery.py`).

Full battery after the agents-view lane: `scripts/battery/runs/20260917T115203Z/`
(2 gaps, 37 passed checks - only the pre-existing f1 splash/notice rows remain;
the f3/f8 entry-type rows now match, f9 is fully green, and the f10/f11 rows
pass). The agents-view frame parity: normalized frames are byte-identical
between the products at 120x36 and 220x50 (paths, uuids, ages, versions, cost,
and the animated running icon normalize away); see the f9 evidence in that run.
The f9 flow resets each side's session universe (fresh daemon + wiped session
files) so the view frames hold only the scripted roster, and settles the mock
before swapping response scripts (a delayed response must only ever apply to
the busy session's request; a status-line request hitting the delayed entry
holds a settled session "running" for the delay).

## Gap table (first full run 20260916T210320Z; the *fixed* marks come from reruns 20260916T221725Z and 20260916T224512Z)

Categories: visual / behavior / protocol / timing.

| id | flow | category | TS (ground truth) | Rust | evidence |
|---|---|---|---|---|---|
| B-1 | f1 | protocol | FIXED in 20260916T221725Z: `--provider`/`--model` reach the daemon session over the wire config; the interactive request carries the flagged model | FIXED: flags ride the TUI create config -> durable create -> worker engine (env fallback only when a create carries no flags); battery row "interactive model flags are authoritative" passes on both sides | `runs/20260916T221725Z/{ts,rust}/f1_launch/first-prompt-mock-requests.json` |
| B-2 | f1 | visual | splash ASCII art + first-run "Share agent traces with Prime Intellect?" notice (Share / Not now, `/traces` hint) | straight into the TUI; no splash, no notice | `ts/f1_launch/01-launch.txt`, `rust/f1_launch/01-launch.txt` |
| B-3 | f1 | timing | FIXED in 20260917T041145Z: worker connect budget 30s (`WORKER_CONNECT_TIMEOUT_MS`, probes + connect + auth share one deadline, stuck child killed on timeout); the AF_UNIX transport re-anchors over-limit socket paths through an O_PATH dir fd (`/proc/self/fd/<fd>/<name>`), the same mechanism the TS runtime applies transparently (strace: `bind(13, {sun_path="/proc/self/fd/12/worker-...sock"}) = 0`) | was: supervisor waited only 15s, and worker socket bind failed outright when the AF_UNIX path exceeded 107 chars -> "session worker <id> did not come up in time", TUI exits 1 | historical: `runs/20260916T203149Z/rust/`; fix evidence: `runs/20260917T041145Z/extras/b3-deep-tmpdir/` (worker socket bound at a 159-char path, full interactive turn over it) |
| B-4 | f2 | protocol | model tool surface: `ipython` only (bash/edit live in the kernel) | FIXED (run `20260916T224512Z`): `ipython` only, tool schemas byte-identical | `ts/f2_prompt/mock-requests.json` vs `rust/f2_prompt/mock-requests.json` |
| B-5 | f2 | protocol | sends a `[harness-digest]` user message before the prompt | FIXED (same run): digest delivered at cold context boundaries, byte-identical to TS | same evidence as B-4 |
| B-6 | f2 | protocol | system prompt carries conversation-log path, pre-installed packages, installed skill modules, available-skills inventory, refinement guidance (23119 chars in the capture) | SUPERSEDED (roadmap item 3): the Rust product adopts the layered prompt redesign as its native prompt — TS-prompt parity no longer applies. The battery row checks the layered shape (static layers, then the dynamic tail); the Rust prompt itself is pinned by the self-snapshot golden `crates/pa-core/tests/golden/system_prompt.rs` and the guard tests `crates/pa-core/tests/prompt_guards.rs`. Raw prompts stay in `protocol-request-diff.txt` as reference evidence | `runs/20260916T210320Z/protocol-request-diff.txt` |
| B-7 | f2/f5 | protocol | FIXED in 20260916T221725Z: after each completed turn the daemon session issues a status-line request to a small model (`qwen/qwen3-30b-a3b-instruct-2507`) | FIXED: the worker's StatusLineRunner issues the same request (same trigger, model, system prompt, max_tokens 400) and broadcasts `session_status`; battery row "post-turn status-line request issued by both sides" passes | `runs/20260916T221725Z/{ts,rust}/f5_side_questions/statusline-requests.json` |
| B-8 | f3/f8 | protocol | FIXED in 20260917T041145Z: session entry sets match on both sides (`f3`/`f8` rows "session entry type sets match"): `service_tier_change` emitted in the creation prefix (fresh + resume, settings default), `custom_message` + `compaction` landed earlier (#83/#85), queue snapshots moved out of session files into the worker recovery journal, and settled status verdicts persist as `agent_status` entries (model classifications + transcript error verdicts; needs_input fallback and sweeps never grow the journal; respawned workers seed from the persisted verdict) | was: `custom` entries (`prime-agent-rs.queue_snapshot`), no `service_tier_change`, no `agent_status` | `runs/20260917T041145Z/{f3_tool,f8_resume}-session-shapes.json`; `agent_status` differential: `runs/20260917T041145Z/extras/b8-agent-status/` |
| B-9 | f4 | visual | `/` opens the slash-command menu (settings, model, new, compact, ...) | `/` is typed into the composer; no command menu | `rust/f4_commands/01-slash-menu.txt` |
| B-10 | f7 | protocol | daemon `compact` compacts and returns `{summary, firstKeptEntryId, tokensBefore, details{readFiles, modifiedFiles}}` | `compact` is an unknown command (`{"command":"unknown"}`) | `ts/f7_compaction/compact-response.json`, `rust/f7_compaction/compact-response.json` |
| B-11 | f8 | behavior | FIXED in 20260916T221725Z: print `-c` refuses when the target session is active in the daemon: "Session is already active in <id>: <path>" | FIXED: the print path probes the daemon roster and refuses with the exact message; battery row "print '-c' refuses a session active in the daemon on both sides" passes | `runs/20260916T221725Z/{ts,rust}/f8_resume/continue-cmd.json` |

Fixed in the same lane (no battery row): the daemon worker resolves request
auth through the registry (auth storage, then the models.json provider
`apiKey`), so custom provider names with no env-key mapping now
authenticate; the battery's env-var model workaround is gone.




## Battery-flows unit (f14-f21) — latest run 20260919T011717Z

Authoritative run for the f14-f21 unit: TS `prime-agent` (v0.9.5, ground
truth) vs the Rust release binary (sandbox build, rustc 1.98.1, merged base
through #150-#153), both against the deterministic mock provider.
`runs/20260919T011717Z/` holds the full evidence (report.md, findings.json,
per-side tmux frames, session transcripts, mock wire logs).

Result: 0 gaps, 26 parity checks passed, 44 EXPECTED-FAIL rows (known gaps,
each tagged with its owning fix lane in findings.json).

Harness fixes proven in this run (all in `scripts/battery/run_battery.py`):

- **f18 goal-loop taming.** With an active goal the TS daemon immediately
  drives `[goal: continuation]` turns and starves queued composer input; run
  20260918T231155Z measured ~2900 loop turns and a 1.65GB wire log. The flow
  now pauses right after the start row renders (the queue is still shallow),
  so the loop churns ~2 turns and the TS wire log is 1.3MB. All TS f18 rows
  pass; the completion turn is scripted with a 6s mock-settle window so the
  previous turn's status-line request cannot pop the queued tool call.
- **f16 refinement scripting.** The refinement-proposal JSON is queued as a
  proper tool call (mock round-robin had starved it); the TS "Harness
  refined" row passes.
- **Sandbox-built rust binary + kernel runtime.** A cargo/sandbox build
  bakes the BUILD machine's source-checkout path into runtime resolution
  (run 20260919T002527Z: every kernel ipython cell died with "kernel
  startup failed ... `uv pip install prime-agent-runtime ...` exit 1").
  The battery now sets `PI_PACKAGE_DIR` to the checkout for the rust side
  (the same layout the packaged product ships), so kernel flows (f15
  send, f16 refinement, f18 goal.complete, f20 rlm.spawn) execute their
  kernel cells on the rust side.

Per-lane EXPECTED-FAIL triage (44 rows, run 20260919T011717Z):

| lane | flow | rows | rust-side substance |
|---|---|---|---|
| compact-fb-2 | f14 | 4 | no `◆ Context compacted` row on `/compact`; no auto-compaction outcome on threshold crossing; manual+auto frame diffs (lane fix #501c5a4 not in this base) |
| decorations-3 | f15 | 4 | sibling message IS delivered (`custom_message` in the session) but no `◆ Agent message received` row renders; sender shows no `◆ Agent message sent` row |
| decorations-3 | f16 | 5 | the refinement tool call leaves no refinement entry in the rust session and no `◆ Harness refined` row; no `[harness-digest]` row on either side (TS digest expectation needs reconciliation with TS behavior) |
| model-picker-1 | f17 | 6 | `/model` selector does not list the configured mock model; no `Model: <id>` confirm row; `/effort` shows no thinking-level surface |
| goal-autonomous | f18 | 8 | `goal.complete()` executes (session shows `status: complete`) but no completion row renders; all 7 goal/autonomous frame diffs differ. Note: the completed goal's recorded objective is the literal `/goal resume` text — rust appears to re-create the goal on `/goal resume` instead of resuming it |
| heartbeat-tui | f19 | 6 | no `Heartbeat set` row on `/heartbeat`; fired heartbeat shows no `♥ Heartbeat prompt` row; `/heartbeats` opens no manager view |
| subagents-tui | f20 | 6 | `rlm.spawn` works (`RLMSpawnHandle` with `rlm_child_id` in the session) but no spawn summary line, no `RLM child status` terminal notice, and the scoped-agents view does not list the child |
| worker-recovery | f21 | 5 | both sides fail the post-recovery turn. TS: worker kill ends in `Session worker is failed`, no new ready worker (workerState: None). Rust: worker respawn PASSES (new pid, workerState ready) but the post-recovery turn does not complete. Reconcile which surface is ground truth |

Run history for this unit: 20260918T231155Z (first run, committed; found the
TS goal-loop runaway), 20260919T002527Z (found the sandbox-baked runtime
path; superseded, not committed), 20260919T011717Z (authoritative,
committed).
