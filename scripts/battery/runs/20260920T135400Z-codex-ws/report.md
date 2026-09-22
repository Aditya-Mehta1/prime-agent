# Codex WS transport-error parity run

Probe: `scripts/battery/provider_error_probe.py` (extended with the WS-mode scenarios) — 
scripted WebSocket wire sequences against a raw-socket mock, plus a successful SSE fallback answer,
one custom provider per scenario (models.json, retry disabled). Sides: TS 0.9.5 (`prime-agent` on
PATH, mission box) and Rust (this branch, release build in the `build-codex-ws` rust:1 sandbox).
Evidence per side: `*/evidence.json` (`errorMessage`, stderr, exit code,
`provider_stream_failure` + `provider_transport_failure` diagnostics; ports are per-run and differ).

## WS transport scenarios (provider_transport_failure diagnostic)

| scenario | match | TS transport error (name — message — code) | Rust transport error |
|---|---|---|---|
| ws_handshake_reject_401 | YES | Error — WebSocket connection to 'ws://127.0.0.1:44481/codex/responses' failed: Expected 101 status code — code None | Error — WebSocket connection to 'ws://127.0.0.1:52941/codex/responses' failed: Expected 101 status code — code None |
| ws_handshake_reject_500 | YES | Error — WebSocket connection to 'ws://127.0.0.1:44481/codex/responses' failed: Expected 101 status code — code None | Error — WebSocket connection to 'ws://127.0.0.1:52941/codex/responses' failed: Expected 101 status code — code None |
| ws_bad_accept_key | YES | Error — WebSocket connection to 'ws://127.0.0.1:44481/codex/responses' failed: Mismatch websocket accept header — code None | Error — WebSocket connection to 'ws://127.0.0.1:52941/codex/responses' failed: Mismatch websocket accept header — code None |
| ws_close_1011_reason | YES | WebSocketCloseError — WebSocket closed 1011 mock server reason — code 1011 | WebSocketCloseError — WebSocket closed 1011 mock server reason — code 1011 |
| ws_close_1009_no_reason | YES | WebSocketCloseError — WebSocket closed 1009 message too big — code 1009 | WebSocketCloseError — WebSocket closed 1009 message too big — code 1009 |
| ws_close_1000_done | YES | WebSocketCloseError — WebSocket closed 1000 done — code 1000 | WebSocketCloseError — WebSocket closed 1000 done — code 1000 |
| ws_close_no_code | YES | WebSocketCloseError — WebSocket closed 1005 — code 1005 | WebSocketCloseError — WebSocket closed 1005 — code 1005 |
| ws_fin_no_close | YES | WebSocketCloseError — WebSocket closed 1006 Connection ended — code 1006 | WebSocketCloseError — WebSocket closed 1006 Connection ended — code 1006 |
| ws_tcp_rst | YES | WebSocketCloseError — WebSocket closed 1006 Connection ended — code 1006 | WebSocketCloseError — WebSocket closed 1006 Connection ended — code 1006 |
| ws_reserved_opcode | YES | WebSocketCloseError — WebSocket closed 1002 Protocol error - unsupported control frame — code 1002 | WebSocketCloseError — WebSocket closed 1002 Protocol error - unsupported control frame — code 1002 |
| ws_reserved_control_opcode | YES | WebSocketCloseError — WebSocket closed 1002 Protocol error - unsupported control frame — code 1002 | WebSocketCloseError — WebSocket closed 1002 Protocol error - unsupported control frame — code 1002 |
| ws_rsv_bits | YES | WebSocketCloseError — WebSocket closed 1011 Compression not implemented yet — code 1011 | WebSocketCloseError — WebSocket closed 1011 Compression not implemented yet — code 1011 |
| ws_wss_to_plain | YES | Error — WebSocket connection to 'wss://127.0.0.1:44481/codex/responses' failed: TLS handshake failed — code None | Error — WebSocket connection to 'wss://127.0.0.1:52941/codex/responses' failed: TLS handshake failed — code None |
| ws_invalid_json | YES | - | - |
| ws_error_event_then_close | YES | - | - |
| ws_start_then_close_1011 | YES | WebSocketCloseError — WebSocket closed 1011 mock server reason — code 1011 | WebSocketCloseError — WebSocket closed 1011 mock server reason — code 1011 |
| ws_start_then_fin | YES | WebSocketCloseError — WebSocket closed 1006 Connection ended — code 1006 | WebSocketCloseError — WebSocket closed 1006 Connection ended — code 1006 |
| ws_start_then_invalid_json | YES | - | - |
| connection_openai-codex-responses | YES | Error — WebSocket connection to 'ws://127.0.0.1:1/codex/responses' failed: Failed to connect — code None | Error — WebSocket connection to 'ws://127.0.0.1:1/codex/responses' failed: Failed to connect — code None |

Mid-stream (after `response.created`) twins surface both the `provider_transport_failure`
and `provider_stream_failure` diagnostics; every field matches (name, message, close code,
kind `unknown`, `providerErrorType` `WebSocketCloseError`, `eventsEmitted`, `phase`,
`fallbackTransport` present only before the stream start, exit codes, stderr):

| scenario | match | TS errorMessage | Rust errorMessage |
|---|---|---|---|
| ws_start_then_close_1011 | YES | WebSocket closed 1011 mock server reason | WebSocket closed 1011 mock server reason |
| ws_start_then_fin | YES | WebSocket closed 1006 Connection ended | WebSocket closed 1006 Connection ended |
| ws_start_then_invalid_json | NO | Invalid Codex WebSocket JSON: JSON Parse error: Unexpected identifier "not" | Invalid Codex WebSocket JSON: expected ident at line 1 column 2 |

## Codex HTTP scenarios now carrying the WS-fallback diagnostic

| scenario | match | TS transport error | Rust transport error |
|---|---|---|---|
| codex_429_usage_limit | YES | Error — WebSocket connection to 'ws://127.0.0.1:46675/codex/responses' failed: Expected 101 status code | Error — WebSocket connection to 'ws://127.0.0.1:54863/codex/responses' failed: Expected 101 status code |
| codex_400_body | YES | Error — WebSocket connection to 'ws://127.0.0.1:46675/codex/responses' failed: Expected 101 status code | Error — WebSocket connection to 'ws://127.0.0.1:54863/codex/responses' failed: Expected 101 status code |
| connection_openai-codex-responses | YES | Error — WebSocket connection to 'ws://127.0.0.1:1/codex/responses' failed: Failed to connect | Error — WebSocket connection to 'ws://127.0.0.1:1/codex/responses' failed: Failed to connect |

## Non-matches (runtime-inherent, documented)

- `ws_invalid_json` / `ws_start_then_invalid_json`: the pinned prefix, error class
  (`CodexProtocolError`), non-transport behavior (no SSE fallback), and diagnostics all match;
  only the JSON parse-cause suffix after `Invalid Codex WebSocket JSON: ` differs by parser
  runtime (bun/JSC: `JSON Parse error: Unexpected identifier "not"`; serde_json: `expected
  ident at line 1 column 2`) — the same class as the SSE JSON path’s serde text.
- `ws_wss_to_plain`: the WS transport diagnostic matches (`TLS handshake failed`); the final SSE
  fetch error differs because the raw-fetch connection profile (#216) pins the refused-connect text
  and bun’s TLS fetch failure text (`unknown certificate verification error`) is a separate,
  pre-existing connection-text surface outside this WS lane.

All 38 scenarios agree on exit codes; the 35 scenarios not listed above match byte-for-byte
(modulo the per-run mock port) on errorMessage, stderr, and every diagnostic field.