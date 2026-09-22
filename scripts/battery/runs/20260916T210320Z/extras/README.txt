Extra evidence captured during harness bring-up; referenced by docs/parity-battery.md.

- ts-statusline-request.json: the TS daemon session issues a second provider
  request after each completed turn, to the dashboard status-line model
  (qwen/qwen3-30b-a3b-instruct-2507, provider prime-inference; system prompt
  "You generate a status line for an AI coding agent dashboard...").
  Captured by pointing the prime-inference provider base URL at the battery
  mock via models.json. The Rust build issues no equivalent request.

- rust-interactive-model-flags-session.jsonl: session file from a Rust
  interactive launch invoked with `--provider battery --model mock-1` (before
  the battery's env workaround). The assistant turn ran with provider
  prime-inference / model z-ai/glm-5.3: the CLI model flags never reach the
  daemon session worker, which resolves its own fallback model.
