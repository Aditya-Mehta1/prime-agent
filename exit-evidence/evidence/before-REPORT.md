# BEFORE evidence — TUI exit-restoration lane (rust tip = the before-fix build)

- Sandbox: `s7txjw2pat282e1yp17ok8w9` (name exit-restore-lane, image rust:1, 8 cpu / 16 GB / 100 GB, 240-minute timeout, kept alive for the after-phase)
- Git sha built: `8edf2d269` (branch `rust` tip — "pa-tui: the subagent pane is the full-screen scoped agents view (TS parity) + panel clippy/short-viewport fixes (#2623)")
- Build: `cargo build --release --bin prime-agent` → exit 0 (`/opt/build.done` = 0); rustc/cargo 1.98.1
- Binary under test: `/opt/repo/repo/target/release/prime-agent`
- Harnesses: taken from `origin/fix/tui-exit-restore` commit `d6c7718d9` (the rust tip itself does not carry `tui_exit_restore_parity.py` — it is new on the fix branch; `left_arrow_pty_repro.py` is the fix-branch version with the quit scenarios). No repo source was modified; the two scripts were extracted into the VM checkout only.
- VM runtime env: tmux 3.5a, python3 3.13.5, uv 0.12.18 (`/usr/local/bin/uv`), `PI_PACKAGE_DIR` = the TS release's `prime-agent-runtime` sidecar (pure-Python package, platform-independent; release 0.9.5-beta.1948).
- Methodology note: both harnesses need the `prime-agent-runtime` sidecar. One earlier pty-battery run was discarded because the client never launched in that environment (the missing sidecar raised SystemExit inside the fork-child, surfacing as a bogus kill traceback + exit-code-1 FAILs everywhere) — those FAILs were environment noise, not product evidence. The logs below are the completed runs.
- Verdict note: with `--sides before`, before-side failures are recorded, not gated, so the tool's final line prints `EXIT-RESTORE TMUX E2E: GREEN` by design; the `RECORDED-BEFORE-FAIL` lines plus `results.json` are the damage inventory. 
- Environment artifact (important): the VM runs as root, so the pane shell prompt is `bash-5.2#`. The harness's prompt-back detector regex is `^bash-[\d.]+\$ ?$` (the non-root prompt), so every scenario records a spurious `prompt-not-back` (and the prompt-regex-based `error_exited`/`force_exited` facts read false). These lines are artifacts of the VM's root shell, NOT tip damage — the pane evidence files show the shell prompt returning. The regex-independent facts (exit code, `#{alternate_on}`, stty cksum, escape-leak scan) are unaffected and are the basis of the table below.

## Scenario table (before side, rust tip 8edf2d269)

| scenario | verdict on real invariants | what broke |
|---|---|---|
| quit | PASS | none (exit 0, alt off, tty sane `1376417782 88`, no leak); `prompt-not-back` = root-prompt artifact |
| quit-slash | PASS | none (same artifact only) |
| agents-exit | PASS | none (same artifact only) |
| error-exit | **FAIL — real damage** | mid-loop fatal error ("timed out after 10000ms waiting for the Prime Agent daemon response") exits the process **1** with NO restore: alt screen **still ON** after exit (`alt-on=1`), tty left **raw** (cksum `2997995933 87` ≠ baseline `1376417782 88`), escape bytes **leak** into the typed line — the full paper-cut family |
| wedge | PASS | exit 0, alt off, tty sane, no leak at this path on the tip; `force_exited=false` = root-prompt artifact (the pty battery confirms the force-quit watchdog does fire with a full stop set) |
| poisoned | **FAIL — real damage** | normal quit on a poisoned (`stty raw -echo`) start restores the saved raw state: tty mismatch (cksum `3298912900 85` ≠ baseline `1376417782 88`) + escape leak in the typed line — the missing cooked-tty verification/repair on the normal exit |
| panic | not run | the tip's `pa-tui-replay` has no `--panic-exit` (it arrives with the fix branch) |

## PTY battery scenario table (rust = tip build)

| scenario | verdict | detail |
|---|---|---|
| quit-normal | PASS | exit 0; full stop set (paste_off, kitty_pop, modify_reset, mouse_off, leave_alt, cursor_show all true); kitty push/pop balanced; termios cooked after (icanon/echo true) |
| quit-poisoned | **FAIL — the before-side point** | exit 0 with the full stop-set bytes present, but termios after = **icanon:false, echo:false** — the poisoned raw start was restored as-is; `rust:quit-poisoned:poisoned-start-not-repaired` |
| wedge-force-quit | PASS | force-quit watchdog fired (`shutdown_stalled` message present), exit 0, full stop set, termios cooked |
| left-instant | PASS | bare LEFT hands the pane to the agents view (`handoff_painted` true), client stays alive, no forced exit |

Final: `LEFT-ARROW PTY REPRO: RED` with exactly one real FAIL (`rust:quit-poisoned:poisoned-start-not-repaired`) — matches the expected before-side divergence (the same scenario where the TS side records its own wasRaw-restore divergence on the Mac).

## Full harness stdout

The complete stdout of both runs follows (verbatim from the VM log files; `results.json` and the per-scenario pane captures are in `before/`).

### scripts/tui_exit_restore_parity.py --sides before --scenarios quit,quit-slash,agents-exit,error-exit,wedge,poisoned --out /opt/evidence/before

run root: /tmp/exit-restore-ag5_9trj; evidence: /opt/evidence/before
[exit-restore] before quit: {"side": "before", "scenario": "quit", "stty_baseline": "1376417782 88", "settled": true, "alt_on_while_running": "1", "prompt_back": false, "exit_code": 0, "tty_cksum": "1376417782 88", "alternate_on_after": "0", "stty_matches_sane": true, "typed_visible": true, "leak_free": true, "evidence_pane": "/opt/evidence/before/before-quit-pane.txt", "resume_hint_visible": false}
[exit-restore] before quit-slash: {"side": "before", "scenario": "quit-slash", "stty_baseline": "1376417782 88", "settled": true, "alt_on_while_running": "1", "prompt_back": false, "exit_code": 0, "tty_cksum": "1376417782 88", "alternate_on_after": "0", "stty_matches_sane": true, "typed_visible": true, "leak_free": true, "evidence_pane": "/opt/evidence/before/before-quit-slash-pane.txt", "resume_hint_visible": false}
[exit-restore] before agents-exit: {"side": "before", "scenario": "agents-exit", "stty_baseline": "1376417782 88", "settled": true, "alt_on_while_running": "1", "prompt_back": false, "exit_code": 0, "tty_cksum": "1376417782 88", "alternate_on_after": "0", "stty_matches_sane": true, "typed_visible": true, "leak_free": true, "evidence_pane": "/opt/evidence/before/before-agents-exit-pane.txt", "resume_hint_visible": false}
[exit-restore] before error-exit: {"side": "before", "scenario": "error-exit", "stty_baseline": "1376417782 88", "settled": true, "alt_on_while_running": "1", "workers_found": 6, "error_exited": false, "prompt_back": false, "exit_code": 1, "tty_cksum": "2997995933 87", "alternate_on_after": "1", "stty_matches_sane": false, "typed_visible": true, "leak_free": false, "evidence_pane": "/opt/evidence/before/before-error-exit-pane.txt", "resume_hint_visible": false}
[exit-restore] before wedge: {"side": "before", "scenario": "wedge", "stty_baseline": "1376417782 88", "settled": true, "alt_on_while_running": "1", "workers_found": 8, "force_exited": false, "prompt_back": false, "exit_code": 0, "tty_cksum": "1376417782 88", "alternate_on_after": "0", "stty_matches_sane": true, "typed_visible": true, "leak_free": true, "evidence_pane": "/opt/evidence/before/before-wedge-pane.txt", "resume_hint_visible": false}
[exit-restore] before poisoned: {"side": "before", "scenario": "poisoned", "stty_baseline": "1376417782 88", "settled": true, "alt_on_while_running": "1", "prompt_back": false, "exit_code": 0, "tty_cksum": "3298912900 85", "alternate_on_after": "0", "stty_matches_sane": false, "typed_visible": true, "leak_free": false, "evidence_pane": "/opt/evidence/before/before-poisoned-pane.txt", "resume_hint_visible": false}
RECORDED-BEFORE-FAIL before:quit:prompt-not-back
RECORDED-BEFORE-FAIL before:quit-slash:prompt-not-back
RECORDED-BEFORE-FAIL before:agents-exit:prompt-not-back
RECORDED-BEFORE-FAIL before:error-exit:exit-code=1
RECORDED-BEFORE-FAIL before:error-exit:prompt-not-back
RECORDED-BEFORE-FAIL before:error-exit:alt-on=1
RECORDED-BEFORE-FAIL before:error-exit:tty-state=2997995933 87
RECORDED-BEFORE-FAIL before:error-exit:escape-leak
RECORDED-BEFORE-FAIL before:wedge:prompt-not-back
RECORDED-BEFORE-FAIL before:poisoned:prompt-not-back
RECORDED-BEFORE-FAIL before:poisoned:tty-state=3298912900 85
RECORDED-BEFORE-FAIL before:poisoned:escape-leak
EXIT-RESTORE TMUX E2E: GREEN

### PA_RUST_BINARY=... scripts/left_arrow_pty_repro.py --sides rust --scenarios quit-normal,quit-poisoned,wedge-force-quit,left-instant

sandbox root: /tmp/left-arrow-pty-zwk9af3r
[pty] rust quit-normal: {"side": "rust", "scenario": "quit-normal", "chat_settled": true, "reply_streamed": true, "alive": false, "exit_code": 0, "shutdown_stalled": false, "kitty_pushes": 1, "kitty_pops": 1, "handoff_painted": false, "stop_set": {"paste_off": true, "kitty_pop": true, "modify_reset": true, "mouse_off": true, "leave_alt": true, "cursor_show": true}, "push_pop_balanced": true, "termios_after": {"icanon": true, "echo": true}, "tail_text": "                                                                   \r\n\r                                                                                                                        \r\n\rPress Ctrl+C again to exit                                                                            faux-1 \u00b7 8.0k (6%)\r\nResume this session with: prime-agent --resume 01a0d04f-bdc4-7450-b6a2-7d0daa746a16\r\n"}
[pty] rust quit-poisoned: {"side": "rust", "scenario": "quit-poisoned", "chat_settled": true, "reply_streamed": true, "alive": false, "exit_code": 0, "shutdown_stalled": false, "kitty_pushes": 1, "kitty_pops": 1, "handoff_painted": false, "stop_set": {"paste_off": true, "kitty_pop": true, "modify_reset": true, "mouse_off": true, "leave_alt": true, "cursor_show": true}, "push_pop_balanced": true, "termios_after": {"icanon": false, "echo": false}, "tail_text": "                                                                   \r\n\r                                                                                                                        \r\n\rPress Ctrl+C again to exit                                                                            faux-1 \u00b7 8.0k (6%)\r\nResume this session with: prime-agent --resume 01a0d04f-caa9-704b-8c84-55006613bc95\r\n"}
[pty] rust wedge-force-quit: {"side": "rust", "scenario": "wedge-force-quit", "chat_settled": true, "reply_streamed": true, "workers_stopped": 7, "alive": false, "exit_code": 0, "shutdown_stalled": true, "kitty_pushes": 1, "kitty_pops": 1, "handoff_painted": false, "stop_set": {"paste_off": true, "kitty_pop": true, "modify_reset": true, "mouse_off": true, "leave_alt": true, "cursor_show": true}, "push_pop_balanced": true, "termios_after": {"icanon": true, "echo": true}, "tail_text": "                                                                                                                                                                                                        \u280bWaiting \u00b7 0s \u00b7 \u2191 6 tokensr\u2193\u2819\u2839\u2838left arrow repro rep       \u2838Writing \u00b7 0s \u00b7 \u2193 6 tokensly\u283c                          faux-1 \u00b7 8.0k6a g a i n \u001b[<u\u001b[>4;0m\u001b[>4;0m
[pty] rust left-instant: {"side": "rust", "scenario": "left-instant", "alive": true, "shutdown_stalled": false, "kitty_pushes": 2, "kitty_pops": 1, "handoff_painted": true, "stop_set": {"paste_off": true, "kitty_pop": true, "modify_reset": true, "mouse_off": true, "leave_alt": false, "cursor_show": true}, "push_pop_balanced": false}
FAIL rust:quit-poisoned:poisoned-start-not-repaired
LEFT-ARROW PTY REPRO: RED

