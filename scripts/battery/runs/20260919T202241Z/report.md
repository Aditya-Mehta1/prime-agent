# Parity battery run 20260919T202241Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /tmp/prime-agent-musl-wr
- flows: f21_worker_recovery

## Findings

0 gaps, 4 EXPECTED-FAIL (known gaps, owner lanes), 9 parity checks passed.

## Known gaps (EXPECTED-FAIL — evidence for the owning fix lanes)

- [f21_worker_recovery/behavior] EXPECTED-FAIL (lane: worker-recovery): ts ground truth (EXPECTED-FAIL, ruling in the flow docstring): the wire-created (unowned) session does not survive its worker's death — the daemon parks the worker failed and the follow-up submit fails with "Session worker is failed"; the owned-path respawn is the behavior Rust ports — evidence: ts/f21_worker_recovery/06-recovered-settled.txt
- [f21_worker_recovery/protocol] EXPECTED-FAIL (lane: worker-recovery): ts ground truth (EXPECTED-FAIL, ruling in the flow docstring): get_state fails with "Session worker is failed" — the unowned worker stays parked failed, no new ready worker in this daemon's lifetime — evidence: ts/f21_worker_recovery/07-post-recovery-state.json
- [f21_worker_recovery/visual] EXPECTED-FAIL (lane: worker-recovery): post-kill: frames differ TS vs Rust (see frame-diff-post-kill.txt) — evidence: ts/f21_worker_recovery/frame-diff-post-kill.txt
- [f21_worker_recovery/visual] EXPECTED-FAIL (lane: worker-recovery): recovered: frames differ TS vs Rust (see frame-diff-recovered.txt) — evidence: ts/f21_worker_recovery/frame-diff-recovered.txt


## Passed checks

- [f21_worker_recovery/protocol] ts: the session summary exposes the live worker pid (workerState: ready)
- [f21_worker_recovery/behavior] ts: the attached TUI keeps the transcript after the worker process is killed
- [f21_worker_recovery/behavior] ts: the worker death surfaces the daemon reconnection status row
- [f21_worker_recovery/behavior] ts ground truth: the failed follow-up surfaces the "⚠ Error: Session worker is failed" and "⚠ Error: Daemon reconnection failed: Session worker is failed" rows with the typed text preserved in the input
- [f21_worker_recovery/protocol] rust: the session summary exposes the live worker pid (workerState: ready)
- [f21_worker_recovery/behavior] rust: the attached TUI keeps the transcript after the worker process is killed
- [f21_worker_recovery/behavior] rust: the worker death surfaces the daemon reconnection status row
- [f21_worker_recovery/behavior] rust: after the worker is killed the session recovers — the next turn completes with the transcript intact
- [f21_worker_recovery/protocol] rust: recovery respawned the worker (pid 363953 -> 364004, workerState ready)
