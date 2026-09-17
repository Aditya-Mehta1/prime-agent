# ACP differential corpus (slice 1)

Captures of the installed TS binary (`prime-agent --mode acp --no-session
--provider prime-inference --model z-ai/glm-5.3-flash`) speaking ACP over
stdio, one file per scenario. Every file is line-delimited JSON:

    {"scenario": ..., "direction": "in"|"out", "frame": {jsonrpc frame}}

`out` lines are what the scenario client sent; `in` lines are what the
binary answered, in order. The last line of each file is a `meta` row with
the process exit code and stderr.

## Scenarios

- `happy_path`: initialize, session/new (matching cwd), one text prompt,
  session/close. Shows the full completion envelope: chunks, then the
  `responseBoundary` info update (`terminalQuiescenceExpected: true`,
  `outcome`), the quiescence event, the `terminalQuiescence` envelope, then
  the `{stopReason}` response.
- `cwd_mismatch`: session/new with cwd `/tmp` while the process runs in
  another directory. The result carries `_meta.cwd {requested, actual}`.
- `errors`: unknown-session prompt/close, a second session/new on a live
  connection, an unknown method (`-32601` with the observed message
  shape), close. Also demonstrates the one-`initialize`-per-connection
  behavior of the served surface.
- `second_initialize`: a second `initialize` on the same connection is
  served normally.
- `cancel`: `session/cancel` mid-turn. The prompt resolves
  `{stopReason: "cancelled"}` with no boundary frames after the streamed
  chunks.
- `tool_call`: a turn that calls the ipython tool (`6*7`). Shows the
  `tool_call` / `tool_call_update` shapes including the `content` wrapper
  and the ipython rich-output `_meta` on results that carry attachments.

## The Rust captures in this directory

The `rust-*.jsonl` files are the evidence run for this slice: the same
scenario client against the Rust build with the same provider/model
(`z-ai/glm-5.3-flash`) and the kernel sidecar resolved through
`PI_PACKAGE_DIR` (a cargo-built binary has no sidecar next to the exe).
All six scenarios matched structurally at commit time.

## Comparing a Rust run

`compare_differential.py` normalizes both captures (volatile ids, version,
model-generated text, sequence numbers, and the `mcpCapabilities` flag the
TS daemon path advertises but the in-process Rust slice does not serve
yet) and compares the frame shape sequences field by field:

    python3 compare_differential.py ts-happy_path.jsonl rust-happy_path.jsonl

Both captures must come from the same scenario client with the same
request order. `crates/pa-cli/tests/acp_mode_e2e.rs` locks the deterministic
scenarios offline against the scripted faux provider; the
network-dependent scenarios (tool_call, cancel mid-turn) are verified by
running the scenario client against both binaries on a networked box.
