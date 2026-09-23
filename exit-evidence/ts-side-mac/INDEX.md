# TS-side exit evidence (Mac, darwin-arm64 TS 0.9.5 release binary)

Run: scripts/tui_exit_restore_parity.py --sides ts --scenarios quit,quit-slash,agents-exit,poisoned
Result: EXIT-RESTORE TMUX E2E: GREEN (poisoned recorded as the TS divergence).

- quit: alt 1->0, stty cksum == baseline (3972113892 220), exit 0, literal echo line, prompt back.
- quit-slash: same.
- agents-exit: same.
- poisoned: exit 0, alt 0, prompt back, BUT tty cksum 3190655191 219 != baseline 3972113892 220
  (TS restores its captured wasRaw = the poisoned state) AND the first typed line lost its
  leading characters ("bash-3.2$ LITCHECK_OK_7f3a" - the "echo " prefix eaten) — Kevin's
  live symptom reproduced on the TS product. Recorded divergence, gated on rust only.

The raw-pty battery TS side (quit-normal / quit-poisoned) also ran GREEN on the Mac:
quit-normal stop set present (paste_off, kitty_pop, mouse_off, leave_alt, cursor_show;
modifyOtherKeys reset absent — TS only writes it when its fallback armed the mode),
termios cooked; quit-poisoned leaves icanon=0 echo=0 (the recorded TS divergence).

NOTE: pane captures here are from the smoke runs at /tmp/exitsmoke-ts6 (results.json holds
the facts). The pane files are the visible evidence: the flushed transcript above the
bash prompt (TS parity exit frame), the clean prompt, the poisoned typed line.
