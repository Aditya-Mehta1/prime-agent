# Parity battery run 20260921T080016Z

Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md).
- ts binary: prime-agent
- rust binary: /home/ubuntu/lane-worktrees/turn-end-frame/target/debug/prime-agent
- flows: f2_prompt, f5_side_questions, f6_attach

## Findings

1 gaps, 0 EXPECTED-FAIL (known gaps, owner lanes), 11 parity checks passed.

### f6_attach

- [protocol] attach event sequences differ: ts=12 rust=11; ts-only=['message_update:assistant'] rust-only=[] — evidence: f6_attach

## Passed checks

- [f2_prompt/behavior] print-mode stdout identical: 'battery hello from mock'
- [f2_prompt/protocol] system prompt: rust layered redesign (cached static layers + dynamic tail; TS-prompt parity superseded; raw prompts in protocol-request-diff.txt)
- [f5_side_questions/protocol] ts: start_side_question answered via mock with side_question_event stream
- [f5_side_questions/protocol] rust: start_side_question answered via mock with side_question_event stream
- [f5_side_questions/protocol] post-turn status-line request issued by both sides (ts=1, rust=1 requests, model qwen/qwen3-30b-a3b-instruct-2507)
- [f6_attach/protocol] ts: wire attach returned a snapshot (data keys: ['activeSessionId', 'client', 'lastEventCursor', 'lastEventSequence', 'protocol', 'replay', 'snapshot'])
- [f6_attach/protocol] ts: attached client received 14 events during the turn
- [f6_attach/behavior] ts: CLI 'attach' opened the session in tmux (frame captured)
- [f6_attach/protocol] rust: wire attach returned a snapshot (data keys: ['activeSessionId', 'client', 'lastEventCursor', 'lastEventSequence', 'protocol', 'replay', 'snapshot'])
- [f6_attach/protocol] rust: attached client received 11 events during the turn
- [f6_attach/behavior] rust: CLI 'attach' opened the session in tmux (frame captured)
