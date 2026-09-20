# Bedrock http2 transport parity run

Probe: `scripts/battery/provider_error_probe.py` (this branch extends it with a raw-frame
h2c mock and http2-mode scenarios) — the #216 `MODE` divergence closed: the bedrock client
now speaks the TS default transport (NodeHttp2Handler: h2c prior-knowledge HTTP/2 over
cleartext, h2-preferred TLS ALPN over https, http1 for AWS_BEDROCK_FORCE_HTTP1/proxy env)
and surfaces the byte-exact bun node:http2 failure texts. Sides: TS 0.9.5 (`prime-agent` on
PATH, mission box; the shared ts-identity guard asserts the PATH binary is the TS
product) and Rust (this branch, musl release build from the rust:1 sandbox,
run on the box). Evidence per side: `*/evidence.json` (errorMessage, stderr, exit code,
and the `provider_stream_failure` diagnostic: error.name / kind / status / providerErrorType).

| scenario | match | TS errorMessage | Rust errorMessage |
|---|---|---|---|
| mistral_400_body | YES | Mistral API error (400): {"message": "mock mistral bad request"} | Mistral API error (400): {"message": "mock mistral bad request"} |
| anthropic_400_body | YES | Provider rejected the request (invalid_request_error, 400): mock anthropic bad request | Provider rejected the request (invalid_request_error, 400): mock anthropic bad request |
| codex_429_usage_limit | YES | You have hit your ChatGPT usage limit (free plan). | You have hit your ChatGPT usage limit (free plan). |
| codex_400_body | YES | mock codex bad request | mock codex bad request |
| bedrock_400_validation | YES | Protocol error | Protocol error |
| bedrock_400_validation_http1 | YES | Validation error: mock bedrock bad request | Validation error: mock bedrock bad request |
| google_400_body | YES | Provider rejected the request (ApiError, 400) | Provider rejected the request (ApiError, 400) |
| bedrock_h2_400_validation | YES | Validation error: mock bedrock bad request | Validation error: mock bedrock bad request |
| bedrock_h2_rststream_midstream | YES | Stream closed with error code NGHTTP2_INTERNAL_ERROR   Deserialization error: to see the raw response, inspect the hidden field {error}.$response on this object. | Stream closed with error code NGHTTP2_INTERNAL_ERROR   Deserialization error: to see the raw response, inspect the hidden field {error}.$response on this object. |
| bedrock_h2_goaway_midstream | YES | Session closed with error code 1   Deserialization error: to see the raw response, inspect the hidden field {error}.$response on this object. | Session closed with error code 1   Deserialization error: to see the raw response, inspect the hidden field {error}.$response on this object. |
| bedrock_h2_tcp_rst_midstream | YES | The pending stream has been canceled   Deserialization error: to see the raw response, inspect the hidden field {error}.$response on this object. | The pending stream has been canceled   Deserialization error: to see the raw response, inspect the hidden field {error}.$response on this object. |
| bedrock_h2_tcp_fin_midstream | YES | The pending stream has been canceled   Deserialization error: to see the raw response, inspect the hidden field {error}.$response on this object. | The pending stream has been canceled   Deserialization error: to see the raw response, inspect the hidden field {error}.$response on this object. |
| bedrock_h2_400_validation_http1 | YES | read ECONNRESET | read ECONNRESET |
| connection_anthropic-messages | YES | Connection error. | Connection error. |
| connection_mistral-conversations | YES | Unexpected HTTP client error: TypeError: Unable to connect. Is the computer able to access the url? | Unexpected HTTP client error: TypeError: Unable to connect. Is the computer able to access the url? |
| connection_openai-codex-responses | YES | Unable to connect. Is the computer able to access the url? | Unable to connect. Is the computer able to access the url? |
| connection_bedrock-converse-stream | YES | The pending stream has been canceled (caused by: connect ECONNREFUSED 127.0.0.1:1) | The pending stream has been canceled (caused by: connect ECONNREFUSED 127.0.0.1:1) |
| connection_bedrock-converse-stream_http1 | YES | connect ECONNREFUSED 127.0.0.1:1 | connect ECONNREFUSED 127.0.0.1:1 |
| connection_google-generative-ai | YES | Unable to connect. Is the computer able to access the url? | Unable to connect. Is the computer able to access the url? |
| connection_openai-completions | YES | Connection error. | Connection error. |

All 20 scenarios match byte-for-byte, including the diagnostic fields (error.name, kind,
providerErrorType) on every row. The two former `MODE` rows (bedrock_400_validation,
connection_bedrock-converse-stream) now match in the default mode; the http1 twins keep
matching through the new `AWS_BEDROCK_FORCE_HTTP1` handler mode. The http2-mode rows:

- `bedrock_h2_400_validation` — a working h2 answer parses to the same `{prefix}: {message}`
  form over the h2c transport (vs `Protocol error` when the peer is http1-only).
- `bedrock_h2_rststream_midstream` — `Stream closed with error code NGHTTP2_INTERNAL_ERROR` +
  the AWS SDK deserialization hint, `ERR_HTTP2_STREAM_ERROR` code (RST_STREAM mid-body).
- `bedrock_h2_goaway_midstream` — `Session closed with error code 1` + hint,
  `ERR_HTTP2_SESSION_ERROR` (GOAWAY mid-body; a following TCP reset does not clobber it —
  the wire observer in `providers/bedrock/goaway.rs` keeps the session error the TS
  transport reports).
- `bedrock_h2_tcp_rst_midstream` / `bedrock_h2_tcp_fin_midstream` — `The pending stream has
  been canceled` + hint, `ERR_HTTP2_STREAM_CANCEL`.
- `bedrock_h2_400_validation_http1` — the http1 handler mode against an h2-only peer:
  `read ECONNRESET`, recorded under the `TimeoutError` name with the `ECONNRESET` code.

In-crate wire regression tests pin the same texts (`pa-ai` `h2_wire_tests`: rst/goaway/close
mid-body, http1 answer, refused connect).
