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
`prime-inference` (base URL = mock): the TS side resolves the provider/model
from `models.json` plus CLI flags/wire config, the Rust daemon worker resolves
the API key by provider name (`PRIME_API_KEY`) and the model from the
`PRIME_AGENT_MODEL_PROVIDER`/`PRIME_AGENT_MODEL` env vars (see gap B-1).

## Flows

| flow | what it drives | capture |
|---|---|---|
| f1_launch | fresh install state: splash, first-run notice, first prompt + reply | tmux frames, mock request log |
| f2_prompt | headless print mode: one prompt, one model response | stdout/exit, mock request bodies, session files |
| f3_tool | a tool-call turn (`ipython` in both) | stdout, session files, entry-type diff |
| f4_commands | the `/` slash-command menu + one benign run (`/session`) | tmux frames |
| f5_side_questions | `start_side_question`/`abort_side_question` over the daemon socket | wire transcript, event stream |
| f6_attach | wire-level attach (snapshot + event stream) and CLI `attach` in tmux | attach response shape, events, frames |
| f7_compaction | daemon `compact` on a grown session | compact response, session entries |
| f8_resume | headless session persisted, then print `-c` + interactive `--resume` | stdout, frames, session shape diff |
| f9_agents_view | interactive agents view | tmux frames (frame diffing: visual-parity lane) |

First full committed run: `scripts/battery/runs/20260916T210320Z/`
(11 gaps, 18 passed checks). Worker-timeout/socket-path evidence:
`scripts/battery/runs/20260916T203149Z/`. Extra hand-captured evidence lives
in `runs/20260916T210320Z/extras/`.

## Gap table (run 20260916T210320Z unless noted)

Categories: visual / behavior / protocol / timing.

| id | flow | category | TS (ground truth) | Rust | evidence |
|---|---|---|---|---|---|
| B-1 | f1 | protocol | `--provider`/`--model` reach the daemon session over the wire config | CLI model flags never reach the daemon worker; model comes from `PRIME_AGENT_MODEL_PROVIDER`/`PRIME_AGENT_MODEL` env (or falls back to the real provider) | `runs/20260916T210320Z/extras/rust-interactive-model-flags-session.jsonl` |
| B-2 | f1 | visual | splash ASCII art + first-run "Share agent traces with Prime Intellect?" notice (Share / Not now, `/traces` hint) | straight into the TUI; no splash, no notice | `ts/f1_launch/01-launch.txt`, `rust/f1_launch/01-launch.txt` |
| B-3 | f1 | timing | worker connect timeout 30s (`WORKER_CONNECT_TIMEOUT_MS`); survives long TMPDIR socket paths | supervisor waits only 15s (`WORKER_SPAWN_CONNECT_TIMEOUT_MS`), and worker socket bind fails outright when the AF_UNIX path exceeds 107 chars -> "session worker <id> did not come up in time", TUI exits 1 | `runs/20260916T203149Z/rust/` (failed creates), `crates/pa-daemon/src/supervisor.rs` L48 |
| B-4 | f2 | protocol | model tool surface: `ipython` only (bash/edit live in the kernel) | exposes `bash`, `edit`, `ipython` as model tools | `ts/f2_prompt/mock-requests.json` vs `rust/f2_prompt/mock-requests.json` |
| B-5 | f2 | protocol | sends a `[harness-digest]` user message before the prompt | no harness-digest message | same evidence as B-4 |
| B-6 | f2 | protocol | system prompt carries conversation-log path, pre-installed packages, installed skill modules, available-skills inventory, refinement guidance (23119 chars in the capture) | system prompt omits those sections (13526 chars) | `runs/20260916T210320Z/protocol-request-diff.txt` |
| B-7 | f2/f5 | protocol | after each completed turn the daemon session issues a status-line request to a small model (`qwen/qwen3-30b-a3b-instruct-2507`) | no equivalent request | `runs/20260916T210320Z/extras/ts-statusline-request.json` |
| B-8 | f3/f8 | protocol | session entries: `custom_message` (harness_digest), `service_tier_change` per session, `compaction` entries | `custom` entries (`prime-agent-rs.queue_snapshot`), no `service_tier_change`, no `compaction` wiring | `f3_tool-session-shapes.json`, `f8_resume-session-shapes.json` |
| B-9 | f4 | visual | `/` opens the slash-command menu (settings, model, new, compact, ...) | `/` is typed into the composer; no command menu | `rust/f4_commands/01-slash-menu.txt` |
| B-10 | f7 | protocol | daemon `compact` compacts and returns `{summary, firstKeptEntryId, tokensBefore, details{readFiles, modifiedFiles}}` | `compact` is an unknown command (`{"command":"unknown"}`) | `ts/f7_compaction/compact-response.json`, `rust/f7_compaction/compact-response.json` |
| B-11 | f8 | behavior | print `-c` refuses when the target session is active in the daemon: "Session is already active in <id>: <path>" | `-c` silently reopens the same session file while the daemon session is live (no active-session guard in the print path) | `ts/f8_resume/continue-cmd.json` |

Parity checks that pass (per the committed run): identical print-mode stdout;
tool-call turns execute in both; side questions stream
`side_question_event` running->complete on both; wire `attach` returns the same
snapshot data keys on both; CLI `attach` opens in tmux on both; agents view
renders in both.

Notes:

- The Rust daemon `compact`/side-question/attach paths are exercised for real
  through the mock; earlier scripted-engine parity tests live in
  `crates/pa-daemon/tests/supervisor_e2e.rs`.
- TUI frame-level diffing belongs to the visual-parity lane; this battery
  captures frames and flags layout-level gaps (B-2, B-9) only.
- `f8`'s TS `-c` refusal is real TS behavior (the interactive session from f1
  stays resident in the daemon); the battery records it because the Rust side
  diverges (B-11).

Every gap above is also filed in `docs/parity-checklist.md` under the battery
section (no renumbering of existing items).
