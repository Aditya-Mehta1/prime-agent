# B-3 deep-TMPDIR verification (2026-09-17T04:11:41Z)

Rust binary: lane/gap-worker-2 (post-fix), target/release/prime-agent.
Reproduces the original B-3 failure setup (runs/20260916T203149Z): a TMPDIR
deep enough that the worker socket path exceeds the 107-byte AF_UNIX sun_path
limit.

- deep TMPDIR: /tmp/b3-rust-verify2/dddddddddddddddddddd/dddddddddddddddddddd/dddddddddddddddddddd/dddddddddddddddddddd (104 chars)
- worker socket bound at: /tmp/b3-rust-verify2/dddddddddddddddddddd/dddddddddddddddddddd/dddddddddddddddddddd/dddddddddddddddddddd/prime-agent-1000/worker-f8af0b58f7f4-d343d1b894cb.sock (159 chars > 107)
- daemon socket (shallow): /tmp/b3-rust-verify2/daemon.sock
- interactive TUI session came up over the deep-path worker socket; prompt
  'hi' answered 'battery hello from mock' (mock provider).
- daemon-hosted session file entry order: session, model_change,
  thinking_level_change, service_tier_change, session_state, message, message
  (TS parity prefix; queue snapshot no longer a session-file 'custom' entry).

Before the fix this setup failed with "path must be shorter than SUN_LEN"
(worker never came up -> "session worker <id> did not come up in time", TUI
exit 1). The bind/connect transport now re-anchors over-limit paths through
an O_PATH directory fd (/proc/self/fd/<fd>/<name>), the same mechanism the TS
runtime applies transparently (verified by strace: bind(13, {sa_family=AF_UNIX,
sun_path="/proc/self/fd/12/worker-....sock"}) = 0).
