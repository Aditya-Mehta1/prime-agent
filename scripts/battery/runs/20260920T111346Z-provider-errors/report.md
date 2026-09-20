# Provider-error shape parity run

Probe: `scripts/battery/provider_error_probe.py` — scripted non-2xx answers and dead-port
connection probes against a local mock, one custom provider per scenario (models.json,
retry disabled). Sides: TS 0.9.5 (`prime-agent` on PATH, mission box) and Rust (this branch,
release build in the `build-provider-errors-2` rust:1 sandbox). Evidence per side: `*/evidence.json`
(`errorMessage`, stderr, `provider_stream_failure` diagnostic).

| scenario | match | TS errorMessage | Rust errorMessage |
|---|---|---|---|
| mistral_400_body | YES | Mistral API error (400): {"message": "mock mistral bad request"} | Mistral API error (400): {"message": "mock mistral bad request"} |
| anthropic_400_body | YES | Provider rejected the request (invalid_request_error, 400): mock anthropic bad request | Provider rejected the request (invalid_request_error, 400): mock anthropic bad request |
| codex_429_usage_limit | YES | You have hit your ChatGPT usage limit (free plan). | You have hit your ChatGPT usage limit (free plan). |
| codex_400_body | YES | mock codex bad request | mock codex bad request |
| bedrock_400_validation | MODE | Protocol error | Validation error: mock bedrock bad request |
| bedrock_400_validation_http1 | YES | Validation error: mock bedrock bad request | Validation error: mock bedrock bad request |
| google_400_body | YES | Provider rejected the request (ApiError, 400) | Provider rejected the request (ApiError, 400) |
| connection_anthropic-messages | YES | Connection error. | Connection error. |
| connection_mistral-conversations | YES | Unexpected HTTP client error: TypeError: Unable to connect. Is the computer able to access the url? | Unexpected HTTP client error: TypeError: Unable to connect. Is the computer able to access the url? |
| connection_openai-codex-responses | YES | Unable to connect. Is the computer able to access the url? | Unable to connect. Is the computer able to access the url? |
| connection_bedrock-converse-stream | MODE | The pending stream has been canceled (caused by: connect ECONNREFUSED 127.0.0.1:1) | connect ECONNREFUSED 127.0.0.1:1 |
| connection_bedrock-converse-stream_http1 | YES | connect ECONNREFUSED 127.0.0.1:1 | connect ECONNREFUSED 127.0.0.1:1 |
| connection_google-generative-ai | YES | Unable to connect. Is the computer able to access the url? | Unable to connect. Is the computer able to access the url? |
| connection_openai-completions | YES | Connection error. | Connection error. |

`MODE` = the bedrock HTTP transport-mode divergence only: the TS default speaks HTTP/2 to
bedrock (bun NodeHttp2Handler), the Rust client HTTP/1.1. Against an HTTP/1.1-only mock the TS
default surfaces http2 transport errors (`Protocol error`, `ERR_HTTP2_STREAM_CANCEL`) before any
response is parsed; with `AWS_BEDROCK_FORCE_HTTP1=1` — the TS product's own http1 mode, which is
the comparable surface — TS and Rust match byte-for-byte, including the diagnostics.
All diagnostics (`error.name`, `kind`, `status`, `providerErrorType`) match on every scenario except
the two bedrock http2-mode rows (their http1-mode twins match).
