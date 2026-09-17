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
| f9_agents_view | interactive agents view | tmux frames (frame diffing: visual-parity lane) |

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

## Gap table (first full run 20260916T210320Z; the *fixed* marks come from reruns 20260916T221725Z and 20260916T224512Z)

Categories: visual / behavior / protocol / timing.

| id | flow | category | TS (ground truth) | Rust | evidence |
|---|---|---|---|---|---|
| B-1 | f1 | protocol | FIXED in 20260916T221725Z: `--provider`/`--model` reach the daemon session over the wire config; the interactive request carries the flagged model | FIXED: flags ride the TUI create config -> durable create -> worker engine (env fallback only when a create carries no flags); battery row "interactive model flags are authoritative" passes on both sides | `runs/20260916T221725Z/{ts,rust}/f1_launch/first-prompt-mock-requests.json` |
| B-2 | f1 | visual | splash ASCII art + first-run "Share agent traces with Prime Intellect?" notice (Share / Not now, `/traces` hint) | straight into the TUI; no splash, no notice | `ts/f1_launch/01-launch.txt`, `rust/f1_launch/01-launch.txt` |
| B-3 | f1 | timing | FIXED in 20260917T041145Z: worker connect budget 30s (`WORKER_CONNECT_TIMEOUT_MS`, probes + connect + auth share one deadline, stuck child killed on timeout); the AF_UNIX transport re-anchors over-limit socket paths through an O_PATH dir fd (`/proc/self/fd/<fd>/<name>`), the same mechanism the TS runtime applies transparently (strace: `bind(13, {sun_path="/proc/self/fd/12/worker-...sock"}) = 0`) | was: supervisor waited only 15s, and worker socket bind failed outright when the AF_UNIX path exceeded 107 chars -> "session worker <id> did not come up in time", TUI exits 1 | historical: `runs/20260916T203149Z/rust/`; fix evidence: `runs/20260917T041145Z/extras/b3-deep-tmpdir/` (worker socket bound at a 159-char path, full interactive turn over it) |
| B-4 | f2 | protocol | model tool surface: `ipython` only (bash/edit live in the kernel) | FIXED (run `20260916T224512Z`): `ipython` only, tool schemas byte-identical | `ts/f2_prompt/mock-requests.json` vs `rust/f2_prompt/mock-requests.json` |
| B-5 | f2 | protocol | sends a `[harness-digest]` user message before the prompt | FIXED (same run): digest delivered at cold context boundaries, byte-identical to TS | same evidence as B-4 |
| B-6 | f2 | protocol | system prompt carries conversation-log path, pre-installed packages, installed skill modules, available-skills inventory, refinement guidance (23119 chars in the capture) | FIXED (same run): normalized prompts identical; golden test `crates/pa-core/tests/golden/system_prompt.rs` | `runs/20260916T210320Z/protocol-request-diff.txt` |
| B-7 | f2/f5 | protocol | FIXED in 20260916T221725Z: after each completed turn the daemon session issues a status-line request to a small model (`qwen/qwen3-30b-a3b-instruct-2507`) | FIXED: the worker's StatusLineRunner issues the same request (same trigger, model, system prompt, max_tokens 400) and broadcasts `session_status`; battery row "post-turn status-line request issued by both sides" passes | `runs/20260916T221725Z/{ts,rust}/f5_side_questions/statusline-requests.json` |
| B-8 | f3/f8 | protocol | FIXED in 20260917T041145Z: session entry sets match on both sides (`f3`/`f8` rows "session entry type sets match"): `service_tier_change` emitted in the creation prefix (fresh + resume, settings default), `custom_message` + `compaction` landed earlier (#83/#85), queue snapshots moved out of session files into the worker recovery journal, and settled status verdicts persist as `agent_status` entries (model classifications + transcript error verdicts; needs_input fallback and sweeps never grow the journal; respawned workers seed from the persisted verdict) | was: `custom` entries (`prime-agent-rs.queue_snapshot`), no `service_tier_change`, no `agent_status` | `runs/20260917T041145Z/{f3_tool,f8_resume}-session-shapes.json`; `agent_status` differential: `runs/20260917T041145Z/extras/b8-agent-status/` |
| B-9 | f4 | visual | `/` opens the slash-command menu (settings, model, new, compact, ...) | `/` is typed into the composer; no command menu | `rust/f4_commands/01-slash-menu.txt` |
| B-10 | f7 | protocol | daemon `compact` compacts and returns `{summary, firstKeptEntryId, tokensBefore, details{readFiles, modifiedFiles}}` | `compact` is an unknown command (`{"command":"unknown"}`) | `ts/f7_compaction/compact-response.json`, `rust/f7_compaction/compact-response.json` |
| B-11 | f8 | behavior | FIXED in 20260916T221725Z: print `-c` refuses when the target session is active in the daemon: "Session is already active in <id>: <path>" | FIXED: the print path probes the daemon roster and refuses with the exact message; battery row "print '-c' refuses a session active in the daemon on both sides" passes | `runs/20260916T221725Z/{ts,rust}/f8_resume/continue-cmd.json` |

Fixed in the same lane (no battery row): the daemon worker resolves request
auth through the registry (auth storage, then the models.json provider
`apiKey`), so custom provider names with no env-key mapping now
authenticate; the battery's env-var model workaround is gone.


