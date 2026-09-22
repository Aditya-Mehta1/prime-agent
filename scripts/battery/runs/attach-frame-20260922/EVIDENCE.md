# Attach-render evidence — PR "paint the startup chrome before the session attach"

## The reported bug (live dogfood)

On daemon-attach resume of an idle session the pane shows nothing usable until
`/reload` or a terminal resize. Pre-fix behavior on a real daemon (the live
mission socket, a 590-message idle session, `--resume <id>`):

- t=0..30s: the pane holds the previous surface (plain shell/notice text) —
  `run_interactive` awaited the whole attach before entering the alt screen.
- t=30.4s: `Renderer::setup` enters the alt screen and `terminal.clear()`
  wipes it — blank.
- t=30.6s: the first frame draws (the attach snapshot finally arrived).

Root cause: TS `init()` runs `ui.start()` BEFORE `rebindCurrentSession()` and
ends `renderInitialMessages()` in `requestRender()`; the Rust port held the
opposite order (attach first, surface later), so nothing repaints between
attach start and the snapshot's `rebuild_view` frame — no resize fires there.

## Post-fix (same live-daemon replay)

- t=0.6s: the startup chrome paints (banner, editor, tray) — TS `ui.start()`.
- t=32.4s: the transcript paints when the attach snapshot lands
  (`rebuild_view`'s dirty flag; no key, no resize, no `/reload`).

## Verifiers

- `scripts/attach_first_frame_parity.py` (in this PR) drives both binaries
  through: wire-created session with one settled turn -> owner wire closed ->
  TUI attach in tmux with NO keys and NO resize. Gates: first frame <= 10s,
  transcript <= 20s, and an idle-session `rename` (second wire client ->
  `session_info_changed`) repainting <= 10s. Result (evidence:
  `parity-captures/`): ts 1.21s/1.27s, rust 0.11s/0.17s, rename repaint 0.01s
  both sides; the settled attach frames are identical after normalizing the
  build-specific rows (version, model label, cwd).
- Battery reports (attached): f1_launch/f6_attach/f8_resume/f9_agents_view,
  f6_attach, f12_scroll/f13_ctrlc_exit/f21_worker_recovery (0 gaps),
  f10_perf (0 gaps; rust cold-ready 0.147s vs ts 1.100s, first frame median
  0.056s — faster than the pre-fix 0.13-0.33s because the chrome no longer
  waits for the attach).
- New e2e regression tests in `crates/pa-cli/tests/interactive_daemon_e2e.rs`
  pin the two behaviors: attach-to-idle-session renders with no input, and an
  idle-session daemon event repaints with no input.

## Known pre-existing reds on main (verified on `origin/main` in a clean
worktree, not this PR): `suspend_signal_e2e::ctrl_z_...` (reproducible),
`acp_daemon_attached_cancels_mid_turn` (whole-suite load flake; passes alone),
f6's 12-vs-11 streamed-update count (seen on 2026-09-21 in the turn-end-frame
lane), f8's `custom_message` entry diff (flaky, historical), f9's raw frame
diff (f6-leftover roster row, TS archives it while Rust keeps it idle).
