# B-8 agent_status differential evidence

Both products: fresh daemon install, interactive TUI session, mock provider
scripted with one turn reply plus one well-formed status-line answer
(`<recap>Answering the greeting from the mock</recap><status>COMPLETED</status>`).

After the turn settles (2s debounce), both sides persist one `agent_status`
session entry:

- ts-session.jsonl: {"type":"agent_status",...,"status":{"summary":"Answering the greeting from the mock","taskState":"completed","basedOnMessageCount":3}}
- rust-session.jsonl: {"type":"agent_status",...,"status":{"summary":"Answering the greeting from the mock","taskState":"completed","basedOnMessageCount":2}}

Identical wire shape (JSON key order differs only). `basedOnMessageCount`
differs by one because the TS daemon session transcript includes the
harness-digest custom message (Rust daemon workers keep the digest in the
in-session manager and do not mirror it into the worker-owned file yet).
