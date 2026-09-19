# Evidence notes (trimmed)

The fixture install roots kept their layout minus the copied 130M binaries and
the candidate tarball (byte-identical copies of the release binary; see the
prior update_staged EVIDENCE-NOTES). All row data lives in
update_reattach-report.json, including the captured tmux frames:

- T1 mid_update_frame: the TUI pane DURING the update window - the §10.1/§11
  banner ("Prime Agent is updating — restarting the daemon (about 60s). <ids>
  will resume automatically.") plus the reconnect note.
- T1 post_update_frame: the pane AFTER the update - "Reconnected to Prime
  Agent (update <id>) — your session and queued work resumed." with the pane
  alive and the editor usable (reattach is the DEFAULT end state).
- T1 pre_update_frame: the ready state before the update (the visual-parity
  ready needle).
- The TUI ran inside tmux (120x36, `env -u TMUX` rules, the visual-parity
  conventions); the onboarding pane was skipped by pre-seeding the
  `onboardingShown` settings flag (the row tests the reconnect contract, not
  the trace question).

K1/K2: the coordinator was SIGSTOPPED inside the target window first (the
`Preparing` status window is ~10ms on this fixture - a bare race could not
land deterministically), then killed -9: K1 mid-Preparing (the RPC in flight;
the daemon finishes the transaction alone), K2 at Prepared (the frozen
coordinator's marker present). Both recover through the marker self-expiry
watchdog to Serving, the session survives, and the re-run update completes
(the dead-holder intent steal). `prepared_marker_at_kill` and `status_at_kill`
record which window the kill landed in.

The update_staged_rerun / update_prepare_rerun2 dirs (same timestamp) are the
slice-4/3 batteries against the slice-6 binary: all rows green (no regression
from the close-frame/reconnect/UX changes; F1/F2/G1/G2/G3 unchanged).
