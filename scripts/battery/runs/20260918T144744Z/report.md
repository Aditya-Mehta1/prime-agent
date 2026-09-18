# Parity battery run 20260918T144744Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/win-dirs/target/release/prime-agent
- flows: f1_launch, f2_prompt, f3_tool, f4_commands, f5_side_questions, f6_attach, f7_compaction, f8_resume, f9_agents_view, f10_perf, f11_provider_failure, f12_scroll, f13_ctrlc_exit

## Findings

5 gaps, 40 parity checks passed.

### f11_provider_failure

- [visual] failed-attempt error rows differ: ts=4 rust=2 — evidence: f11_provider_failure
### f12_scroll

- [behavior] ts: scrollback checks failed: paged shows early prompt — evidence: ts/f12_scroll
### f13_ctrlc_exit

- [behavior] ts: C-c C-c did not exit cleanly (case healthy): the held prompt never reached the mock — evidence: ts/f13_ctrlc_exit/ts-healthy.json
- [behavior] rust: C-c C-c did not exit cleanly (case healthy): the held prompt never reached the mock — evidence: rust/f13_ctrlc_exit/rust-healthy.json
- [behavior] rust: C-c C-c did not exit cleanly (case wedged): the held prompt never reached the mock — evidence: rust/f13_ctrlc_exit/rust-wedged.json

## Passed checks

- [f1_launch/behavior] ts: first interactive prompt answered by the mock provider
- [f1_launch/protocol] ts: interactive model flags are authoritative (request model: mock-1)
- [f1_launch/behavior] rust: first interactive prompt answered by the mock provider
- [f1_launch/protocol] rust: interactive model flags are authoritative (request model: mock-1)
- [f1_launch/visual] first-run splash + trace-sharing notice rendered and answerable on both sides (fresh install)
- [f2_prompt/behavior] print-mode stdout identical: 'battery hello from mock'
- [f2_prompt/protocol] system prompt: rust layered redesign (cached static layers + dynamic tail; TS-prompt parity superseded; raw prompts in protocol-request-diff.txt)
- [f3_tool/behavior] ts: ipython tool call executed and output captured
- [f3_tool/behavior] rust: ipython tool call executed and output captured
- [f3_tool/protocol] session entry type sets match (['custom_message', 'message', 'model_change', 'service_tier_change', 'session', 'session_state', 'thinking_level_change'])
- [f4_commands/visual] ts: '/' shows a slash-command menu
- [f4_commands/visual] rust: '/' shows a slash-command menu
- [f5_side_questions/protocol] ts: start_side_question answered via mock with side_question_event stream
- [f5_side_questions/protocol] rust: start_side_question answered via mock with side_question_event stream
- [f5_side_questions/protocol] post-turn status-line request issued by both sides (ts=4, rust=2 requests, model qwen/qwen3-30b-a3b-instruct-2507)
- [f6_attach/protocol] ts: wire attach returned a snapshot (data keys: ['activeSessionId', 'client', 'lastEventCursor', 'lastEventSequence', 'protocol', 'replay', 'snapshot'])
- [f6_attach/protocol] ts: attached client received 14 events during the turn
- [f6_attach/behavior] ts: CLI 'attach' opened the session in tmux (frame captured)
- [f6_attach/protocol] rust: wire attach returned a snapshot (data keys: ['activeSessionId', 'client', 'lastEventCursor', 'lastEventSequence', 'protocol', 'replay', 'snapshot'])
- [f6_attach/protocol] rust: attached client received 12 events during the turn
- [f6_attach/behavior] rust: CLI 'attach' opened the session in tmux (frame captured)
- [f6_attach/protocol] attach event sequences match (12 projected events, in order; harness-digest custom pairs and turn_end/agent_end payloads are out of scope here — documented model-surface diffs, see PORTING-NOTES)
- [f7_compaction/protocol] ts: daemon 'compact' succeeded: {"summary": "pre-compaction reply 6", "firstKeptEntryId": "c87c9ddd", "tokensBefore": 110, "details": {"readFiles": [], "modifiedFiles": []}}
- [f7_compaction/protocol] rust: daemon 'compact' succeeded: {"firstKeptEntryId": "bcd8f786", "summary": "pre-compaction reply 6", "tokensBefore": 110}
- [f8_resume/behavior] print '-c' refuses a session active in the daemon on both sides: 597cc1c9b111 (ts) vs 140215d30a11 (rust)
- [f8_resume/protocol] session entry type sets match (['compaction', 'custom_message', 'message', 'model_change', 'service_tier_change', 'session', 'session_info', 'session_state', 'thinking_level_change'])
- [f9_agents_view/behavior] ts: agents view groups the roster into Running/Idle/Inactive sections with the scripted sessions in place
- [f9_agents_view/behavior] ts: opening the Running row attached to the live session and its in-flight reply rendered
- [f9_agents_view/behavior] rust: agents view groups the roster into Running/Idle/Inactive sections with the scripted sessions in place
- [f9_agents_view/behavior] rust: opening the Running row attached to the live session and its in-flight reply rendered
- [f9_agents_view/visual] agents view frames identical at 120x36 (normalized: paths, ids, ages)
- [f9_agents_view/visual] agents view frames identical at 220x50 (normalized: paths, ids, ages)
- [f10_perf/perf] ts: cold startup to interactive-ready median 9.605s over 3 launches (first frame median 9.504s); typing latency median 86.6ms, p95 123.5ms over 75 keystrokes
- [f10_perf/perf] rust: cold startup to interactive-ready median 0.410s over 3 launches (first frame median 0.410s); typing latency median 87.1ms, p95 103.9ms over 75 keystrokes
- [f10_perf/perf] startup: rust 0.410s vs ts 9.605s cold-ready median (ratio 0.04, threshold 1.5)
- [f10_perf/perf] typing: rust p95 103.9ms vs ts p95 123.5ms keystroke-to-render (ratio 0.84, threshold 1.5)
- [f10_perf/perf] rust binary measured: /home/ubuntu/lane-worktrees/win-dirs/target/release/prime-agent (130.9MB, release posture)
- [f11_provider_failure/behavior] ts: provider failure surfaces (retry banner + 4 error row(s))
- [f11_provider_failure/behavior] rust: provider failure surfaces (retry banner + 2 error row(s))
- [f12_scroll/behavior] rust: PageUp pages history into view with the follow hint; paging back to the tail resumes following
