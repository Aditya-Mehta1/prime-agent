# Parity battery run 20260921T202746Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/utility-commands/target/release/prime-agent
- flows: f1_launch, f4_commands, f5_side_questions, f12_scroll

## Findings

0 gaps, 0 EXPECTED-FAIL (known gaps, owner lanes), 12 parity checks passed.


## Passed checks

- [f1_launch/behavior] ts: first interactive prompt answered by the mock provider
- [f1_launch/protocol] ts: interactive model flags are authoritative (request model: mock-1)
- [f1_launch/behavior] rust: first interactive prompt answered by the mock provider
- [f1_launch/protocol] rust: interactive model flags are authoritative (request model: mock-1)
- [f1_launch/visual] first-run splash + trace-sharing notice rendered and answerable on both sides (fresh install)
- [f4_commands/visual] ts: '/' shows a slash-command menu
- [f4_commands/visual] rust: '/' shows a slash-command menu
- [f5_side_questions/protocol] ts: start_side_question answered via mock with side_question_event stream
- [f5_side_questions/protocol] rust: start_side_question answered via mock with side_question_event stream
- [f5_side_questions/protocol] post-turn status-line request issued by both sides (ts=3, rust=2 requests, model qwen/qwen3-30b-a3b-instruct-2507)
- [f12_scroll/behavior] ts: PageUp pages history into view with the follow hint; paging back to the tail resumes following
- [f12_scroll/behavior] rust: PageUp pages history into view with the follow hint; paging back to the tail resumes following
