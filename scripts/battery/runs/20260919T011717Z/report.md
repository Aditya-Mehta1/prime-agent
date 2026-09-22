# Parity battery run 20260919T011717Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/battery-flows/target/release/prime-agent
- flows: f14_compact, f15_a2a, f16_refine, f17_slash_model, f18_goal_autonomous, f19_heartbeat, f20_subagents, f21_worker_recovery

## Findings

0 gaps, 44 EXPECTED-FAIL (known gaps, owner lanes), 26 parity checks passed.

## Known gaps (EXPECTED-FAIL — evidence for the owning fix lanes)

- [f14_compact/visual] EXPECTED-FAIL (lane: compact-fb-2): rust: /compact produced no visible '◆ Context compacted' summary row — evidence: rust/f14_compact/02-after-compact.txt
- [f14_compact/behavior] EXPECTED-FAIL (lane: compact-fb-2): rust: threshold crossing produced no visible auto-compaction outcome — evidence: rust/f14_compact/05-auto-after.txt
- [f14_compact/visual] EXPECTED-FAIL (lane: compact-fb-2): manual-compact: frames differ TS vs Rust (see frame-diff-manual-compact.txt) — evidence: ts/f14_compact/frame-diff-manual-compact.txt
- [f14_compact/visual] EXPECTED-FAIL (lane: compact-fb-2): auto-compact: frames differ TS vs Rust (see frame-diff-auto-compact.txt) — evidence: ts/f14_compact/frame-diff-auto-compact.txt
- [f15_a2a/visual] EXPECTED-FAIL (lane: decorations-3): rust: the delivered sibling message shows no 'Agent message received' row — evidence: rust/f15_a2a/02-received-settled.txt
- [f15_a2a/visual] EXPECTED-FAIL (lane: decorations-3): rust: the sender's ipython cell shows no '◆ Agent message sent/queued' row — evidence: rust/f15_a2a/04-sent-settled.txt
- [f15_a2a/visual] EXPECTED-FAIL (lane: decorations-3): received: frames differ TS vs Rust (see frame-diff-received.txt) — evidence: ts/f15_a2a/frame-diff-received.txt
- [f15_a2a/visual] EXPECTED-FAIL (lane: decorations-3): sent: frames differ TS vs Rust (see frame-diff-sent.txt) — evidence: ts/f15_a2a/frame-diff-sent.txt
- [f16_refine/visual] EXPECTED-FAIL (lane: decorations-3): ts: no [harness-digest] message row on the post-refinement boundary — evidence: ts/f16_refine/04-digest-settled.txt
- [f16_refine/visual] EXPECTED-FAIL (lane: decorations-3): rust: no '◆ Harness refined' outcome row after the kernel-scheduled refinement — evidence: rust/f16_refine/02-refine-settled.txt
- [f16_refine/visual] EXPECTED-FAIL (lane: decorations-3): rust: no [harness-digest] message row on the post-refinement boundary — evidence: rust/f16_refine/04-digest-settled.txt
- [f16_refine/visual] EXPECTED-FAIL (lane: decorations-3): refine: frames differ TS vs Rust (see frame-diff-refine.txt) — evidence: ts/f16_refine/frame-diff-refine.txt
- [f16_refine/visual] EXPECTED-FAIL (lane: decorations-3): digest: frames differ TS vs Rust (see frame-diff-digest.txt) — evidence: ts/f16_refine/frame-diff-digest.txt
- [f17_slash_model/visual] EXPECTED-FAIL (lane: model-picker-1): rust: /model selector did not list the configured mock model — evidence: rust/f17_slash_model/01-model-selector.txt
- [f17_slash_model/visual] EXPECTED-FAIL (lane: model-picker-1): rust: no 'Model: <id>' confirm row after picking in the selector — evidence: rust/f17_slash_model/03-model-selected-settled.txt
- [f17_slash_model/visual] EXPECTED-FAIL (lane: model-picker-1): rust: /effort showed no thinking-level surface — evidence: rust/f17_slash_model/04-effort-picker.txt
- [f17_slash_model/visual] EXPECTED-FAIL (lane: model-picker-1): model-selector: frames differ TS vs Rust (see frame-diff-model-selector.txt) — evidence: ts/f17_slash_model/frame-diff-model-selector.txt
- [f17_slash_model/visual] EXPECTED-FAIL (lane: model-picker-1): model-selected: frames differ TS vs Rust (see frame-diff-model-selected.txt) — evidence: ts/f17_slash_model/frame-diff-model-selected.txt
- [f17_slash_model/visual] EXPECTED-FAIL (lane: model-picker-1): effort-picker: frames differ TS vs Rust (see frame-diff-effort-picker.txt) — evidence: ts/f17_slash_model/frame-diff-effort-picker.txt
- [f18_goal_autonomous/visual] EXPECTED-FAIL (lane: goal-autonomous): rust: goal.complete() produced no visible completion row — evidence: rust/f18_goal_autonomous/05-goal-complete.txt
- [f18_goal_autonomous/visual] EXPECTED-FAIL (lane: goal-autonomous): goal-start: frames differ TS vs Rust (see frame-diff-goal-start.txt) — evidence: ts/f18_goal_autonomous/frame-diff-goal-start.txt
- [f18_goal_autonomous/visual] EXPECTED-FAIL (lane: goal-autonomous): goal-status: frames differ TS vs Rust (see frame-diff-goal-status.txt) — evidence: ts/f18_goal_autonomous/frame-diff-goal-status.txt
- [f18_goal_autonomous/visual] EXPECTED-FAIL (lane: goal-autonomous): goal-pause: frames differ TS vs Rust (see frame-diff-goal-pause.txt) — evidence: ts/f18_goal_autonomous/frame-diff-goal-pause.txt
- [f18_goal_autonomous/visual] EXPECTED-FAIL (lane: goal-autonomous): goal-resume: frames differ TS vs Rust (see frame-diff-goal-resume.txt) — evidence: ts/f18_goal_autonomous/frame-diff-goal-resume.txt
- [f18_goal_autonomous/visual] EXPECTED-FAIL (lane: goal-autonomous): goal-complete: frames differ TS vs Rust (see frame-diff-goal-complete.txt) — evidence: ts/f18_goal_autonomous/frame-diff-goal-complete.txt
- [f18_goal_autonomous/visual] EXPECTED-FAIL (lane: goal-autonomous): autonomous-on: frames differ TS vs Rust (see frame-diff-autonomous-on.txt) — evidence: ts/f18_goal_autonomous/frame-diff-autonomous-on.txt
- [f18_goal_autonomous/visual] EXPECTED-FAIL (lane: goal-autonomous): autonomous-off: frames differ TS vs Rust (see frame-diff-autonomous-off.txt) — evidence: ts/f18_goal_autonomous/frame-diff-autonomous-off.txt
- [f19_heartbeat/visual] EXPECTED-FAIL (lane: heartbeat-tui): rust: /heartbeat produced no 'Heartbeat set' status row — evidence: rust/f19_heartbeat/01-heartbeat-set.txt
- [f19_heartbeat/behavior] EXPECTED-FAIL (lane: heartbeat-tui): rust: the fired heartbeat produced no visible '♥ Heartbeat prompt' row — evidence: rust/f19_heartbeat/03-heartbeat-fired-settled.txt
- [f19_heartbeat/visual] EXPECTED-FAIL (lane: heartbeat-tui): rust: /heartbeats did not open a heartbeat manager view — evidence: rust/f19_heartbeat/04-heartbeats-manager.txt
- [f19_heartbeat/visual] EXPECTED-FAIL (lane: heartbeat-tui): heartbeat-set: frames differ TS vs Rust (see frame-diff-heartbeat-set.txt) — evidence: ts/f19_heartbeat/frame-diff-heartbeat-set.txt
- [f19_heartbeat/visual] EXPECTED-FAIL (lane: heartbeat-tui): heartbeat-fired: frames differ TS vs Rust (see frame-diff-heartbeat-fired.txt) — evidence: ts/f19_heartbeat/frame-diff-heartbeat-fired.txt
- [f19_heartbeat/visual] EXPECTED-FAIL (lane: heartbeat-tui): heartbeats-manager: frames differ TS vs Rust (see frame-diff-heartbeats-manager.txt) — evidence: ts/f19_heartbeat/frame-diff-heartbeats-manager.txt
- [f20_subagents/visual] EXPECTED-FAIL (lane: subagents-tui): rust: a kernel rlm.spawn produced no visible subagent summary line — evidence: rust/f20_subagents/02-spawn-settled.txt
- [f20_subagents/visual] EXPECTED-FAIL (lane: subagents-tui): rust: the completed child produced no 'RLM child status' terminal-notice row — evidence: rust/f20_subagents/04-child-status-settled.txt
- [f20_subagents/visual] EXPECTED-FAIL (lane: subagents-tui): rust: the scoped agents view did not list the spawned child — evidence: rust/f20_subagents/07-scoped-agents-settled.txt
- [f20_subagents/visual] EXPECTED-FAIL (lane: subagents-tui): spawn: frames differ TS vs Rust (see frame-diff-spawn.txt) — evidence: ts/f20_subagents/frame-diff-spawn.txt
- [f20_subagents/visual] EXPECTED-FAIL (lane: subagents-tui): child-status: frames differ TS vs Rust (see frame-diff-child-status.txt) — evidence: ts/f20_subagents/frame-diff-child-status.txt
- [f20_subagents/visual] EXPECTED-FAIL (lane: subagents-tui): scoped-agents: frames differ TS vs Rust (see frame-diff-scoped-agents.txt) — evidence: ts/f20_subagents/frame-diff-scoped-agents.txt
- [f21_worker_recovery/behavior] EXPECTED-FAIL (lane: worker-recovery): ts: the session did not survive its worker's death (no post-recovery turn) — evidence: ts/f21_worker_recovery/06-recovered-settled.txt
- [f21_worker_recovery/protocol] EXPECTED-FAIL (lane: worker-recovery): ts: recovery did not produce a new ready worker (workerState: None, workerPid: None) — evidence: ts/f21_worker_recovery/07-post-recovery-state.json
- [f21_worker_recovery/behavior] EXPECTED-FAIL (lane: worker-recovery): rust: the session did not survive its worker's death (no post-recovery turn) — evidence: rust/f21_worker_recovery/06-recovered-settled.txt
- [f21_worker_recovery/visual] EXPECTED-FAIL (lane: worker-recovery): post-kill: frames differ TS vs Rust (see frame-diff-post-kill.txt) — evidence: ts/f21_worker_recovery/frame-diff-post-kill.txt
- [f21_worker_recovery/visual] EXPECTED-FAIL (lane: worker-recovery): recovered: frames differ TS vs Rust (see frame-diff-recovered.txt) — evidence: ts/f21_worker_recovery/frame-diff-recovered.txt


## Passed checks

- [f14_compact/visual] ts: /compact shows the durable '◆ Context compacted' summary row
- [f14_compact/behavior] ts: crossing the compaction threshold auto-compacts and shows the summary row
- [f15_a2a/visual] ts: a sibling agent message renders the '◆ Agent message received' row with participant label
- [f15_a2a/visual] ts: the sender's ipython cell renders the '◆ Agent message sent/queued' summary row with the participant label
- [f16_refine/visual] ts: the kernel-scheduled refinement renders the '◆ Harness refined' outcome row
- [f17_slash_model/visual] ts: /model opens the selector with the configured model listed
- [f17_slash_model/visual] ts: picking a model in the selector shows the 'Model: <id>' confirm row
- [f17_slash_model/visual] ts: /effort shows the thinking-level picker or its unsupported-model row
- [f18_goal_autonomous/visual] ts: /goal start renders the goal context row / active goal label
- [f18_goal_autonomous/visual] ts: /goal pause renders the paused row
- [f18_goal_autonomous/visual] ts: goal.complete() renders the completion row
- [f18_goal_autonomous/visual] ts: /autonomous on renders the autonomous status row
- [f18_goal_autonomous/visual] rust: /goal start renders the goal context row / active goal label
- [f18_goal_autonomous/visual] rust: /goal pause renders the paused row
- [f18_goal_autonomous/visual] rust: /autonomous on renders the autonomous status row
- [f19_heartbeat/visual] ts: /heartbeat renders the 'Heartbeat set' status row
- [f19_heartbeat/behavior] ts: a fired heartbeat renders the '♥ Heartbeat prompt · every 10s' row
- [f19_heartbeat/visual] ts: /heartbeats opens the heartbeat manager view
- [f20_subagents/visual] ts: a kernel rlm.spawn renders the subagent summary line above the editor with the running/idle/inactive counts
- [f20_subagents/visual] ts: a child finishing without a reply renders the 'RLM child status' terminal-notice row in the parent transcript
- [f20_subagents/visual] ts: the scoped agents view lists the spawned child by name
- [f21_worker_recovery/protocol] ts: the session summary exposes the live worker pid (workerState: ready)
- [f21_worker_recovery/behavior] ts: the attached TUI keeps the transcript after the worker process is killed
- [f21_worker_recovery/protocol] rust: the session summary exposes the live worker pid (workerState: ready)
- [f21_worker_recovery/behavior] rust: the attached TUI keeps the transcript after the worker process is killed
- [f21_worker_recovery/protocol] rust: recovery respawned the worker (pid 161398 -> 161444, workerState ready)
