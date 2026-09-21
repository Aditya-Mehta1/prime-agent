# agent_end messages probe (lane agent-end-messages)

Run 20260921T1331Z — TS `prime-agent` 0.9.5 (PATH) vs the lane branch head
(d81bc7bf, static musl build: `cargo build --workspace --target
x86_64-unknown-linux-musl` in the lane's rust:1 sandbox, then run on this
box). `ALL agent_end frames MATCH`.

`scripts/battery/agent_end_messages_probe.py` drives both binaries against
the shared mock provider and byte-compares the wire `agent_end` frames
(normalized: message `timestamp`s and the JS `stack` traces inside
`provider_stream_failure` diagnostics dropped — a bun binary-path trace the
Rust side structurally never has) plus the run-boundary frame sequence
(`agent_start`/`turn_start`/`turn_end`/`agent_end`/`compaction_*`).

| case | what drives it | cross-side verdict |
| --- | --- | --- |
| settled | a plain settling turn | 1 `agent_end`, payload roles `[custom(harness_digest), user, assistant]` — frames + sequence MATCH |
| retried | HTTP 500 then success (retry settings, 100ms base delay) | 2 `agent_end` frames — the failed run's `[digest, user, error-assistant]`, the retry run's `[assistant]` — plus the retry run's own `agent_start`/`turn_start`; frames + sequence MATCH |
| continued | context-overflow 400 then success (compact-and-retry, keepRecentTokens=10, a filler turn for compaction material) | 3 `agent_end` frames — the filler turn's, the overflow run's `[user, error-assistant]`, the continuation run's `[assistant]` — with `compaction_start`/`compaction_end` between the runs' frames; frames + sequence MATCH |

Notes:
- Unregression: the #250 `aborted_row_probe.py` re-run on the same build —
  `ALL turn_end frames MATCH` on settled/abort/compact/kill/
  abort_and_clear_queue; the compact path emits no `agent_end` at all on
  either side.
- The harness-digest custom row rides `agent_end.messages` on both sides;
  the digest's separate `message_start`/`message_end` pair remains the
  OTHER deferred model-surface diff (custom-message wire projection),
  unchanged.
- Environment caveat for reproduction: the Rust side's bundled-skills
  discovery uses the compile-time source-checkout path, so a sandbox-built
  binary needs `/work/repo` to resolve to the checkout for the `refine`
  skill to register (the digest's refine-call line flips otherwise); the
  box's own target/debug build finds `skills/` natively. The retry-counter
  `auto_retry_*` placement divergence noted in the 0939Z run remains a
  follow-up (the `agent_end` payloads and all run-boundary frames match).
