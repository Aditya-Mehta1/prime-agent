#!/usr/bin/env python3
"""Provider-error shape parity probe (#215 sibling divergences).

Drives one product binary (the installed TS `prime-agent`, or a Rust build)
against a local mock provider whose per-API endpoints return scripted
non-2xx responses, plus dead-port connection probes, and captures what the
binary surfaces for each provider-error class:

  - mistral      `Mistral API error (N): <body>`           (formatMistralError)
  - anthropic    classified `Provider rejected the request (...)` form
  - codex        `CodexApiError.message` verbatim (usage-limit friendly text)
  - bedrock      `{prefix}: <message>`                      (formatBedrockError)
  - bedrock h2   the http2 transport surface the TS default (NodeHttp2Handler,
                 h2c prior knowledge) produces: working-path 400 parsing,
                 `Protocol error` against http1-only peers, mid-stream RST /
                 GOAWAY / TCP-reset texts, and the refused-connect stream-cancel
                 text (vs the AWS_BEDROCK_FORCE_HTTP1 http1 surface)
  - connection   the per-SDK connection texts ("Connection error.", "fetch
                 failed", "Unable to make request: ...")

Evidence per scenario: exit code, stdout/stderr, the persisted assistant
message's errorMessage, and the `provider_stream_failure` diagnostic
(error.name / kind / status / retryAfterMs). Run with:

    python3 provider_error_probe.py --side ts  --binary prime-agent --out evidence/ts
    python3 provider_error_probe.py --side rust --binary <rust-build>/prime-agent \
        --out evidence/rust

Not part of the product; parity-harness only.
"""

from __future__ import annotations

import argparse
import base64
import itertools
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import ts_identity  # the shared PATH-binary identity guard (same dir)

SCRUB_ENV_PREFIXES = ("PRIME_AGENT", "PI_", "OPENAI_", "ANTHROPIC_", "MISTRAL_", "GOOGLE_", "AWS_")
SCRUB_ENV_KEYS = {"HOME", "XDG_CONFIG_HOME", "TMPDIR"}

MOCK_JWT_CLAIM = {"https://api.openai.com/auth": {"chatgpt_account_id": "mock-account"}}


def mock_codex_jwt() -> str:
    """A JWT-shaped api key both sides accept: the TS reads the payload with
    `atob` (standard base64) and the Rust with URL_SAFE_NO_PAD, so the payload
    must encode to plain alphanumeric base64 with no padding."""
    for spacer in range(1, 100):
        payload = json.dumps(MOCK_JWT_CLAIM, separators=(", ", ": "), indent=spacer).encode()
        if len(payload) % 3:
            continue
        encoded = base64.b64encode(payload).decode()
        if encoded.isalnum():
            return "x." + encoded + ".y"
    raise RuntimeError("no alnum base64 payload found")


HTTP2_PREFACE = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"
H2_SETTINGS, H2_HEADERS, H2_RST_STREAM, H2_GOAWAY = 0x4, 0x1, 0x3, 0x7
H2_FLAG_ACK, H2_FLAG_END_STREAM, H2_FLAG_END_HEADERS = 0x1, 0x1, 0x4
H2_ERR_INTERNAL_ERROR, H2_ERR_PROTOCOL_ERROR = 0x2, 0x1


def h2_frame(ftype: int, flags: int, stream_id: int, payload: bytes = b"") -> bytes:
    """One HTTP/2 wire frame (RFC 7540: 9-byte header, big-endian)."""
    return len(payload).to_bytes(3, "big") + bytes((ftype, flags)) + stream_id.to_bytes(4, "big") + payload


def hpack_str(value: bytes) -> bytes:
    """A length-prefixed HPACK string literal (no Huffman; probes only)."""
    assert len(value) < 127, "probe headers are short"
    return bytes((len(value),)) + value


def hpack_response_headers(status: int, content_type: str, request_id: str | None) -> bytes:
    """A minimal HPACK block: indexed `:status` (static table), literal
    `content-type` (indexed name 31), optional literal `x-amzn-requestid`."""
    indexed_status = {200: 0x88, 204: 0x89, 206: 0x8A, 304: 0x8B, 400: 0x8D, 404: 0x8E, 500: 0x8F}
    block = (
        bytes([indexed_status[status]])
        if status in indexed_status
        else b"\x00" + hpack_str(b":status") + hpack_str(str(status).encode())
    )
    block += b"\x0f\x10" + hpack_str(content_type.encode())  # literal, indexed name 31
    if request_id is not None:
        block += b"\x00" + hpack_str(b"x-amzn-requestid") + hpack_str(request_id.encode())
    return block


def h2_send_response(
    sock: socket.socket,
    stream_id: int,
    status: int,
    body: bytes,
    end_stream: bool,
    request_id: str | None = None,
) -> None:
    """HEADERS + DATA for the scripted response (`application/json` answers)."""
    headers = hpack_response_headers(status, "application/json", request_id)
    sock.sendall(h2_frame(H2_HEADERS, H2_FLAG_END_HEADERS, stream_id, headers))
    flags = H2_FLAG_END_STREAM if end_stream else 0
    sock.sendall(h2_frame(0x0, flags, stream_id, body))


def h2_send_partial_eventstream(sock: socket.socket, stream_id: int) -> None:
    """HEADERS(200, event-stream) + a truncated aws eventstream message prelude,
    so both sides are inside the response body when the transport failure hits."""
    headers = hpack_response_headers(200, "application/vnd.amazon.eventstream", "probe-request-id")
    sock.sendall(h2_frame(H2_HEADERS, H2_FLAG_END_HEADERS, stream_id, headers))
    # aws eventstream prelude prefix: total length, headers length; truncated
    # before the prelude CRC so the body decode stays mid-message.
    partial = b"\x00\x00\x00\x40\x00\x00\x00\x10\x00\x00\x00\x00\x00\x00\x00\x00"
    sock.sendall(h2_frame(0x0, 0, stream_id, partial))


def recv_exact(sock: socket.socket, count: int) -> bytes:
    out = bytearray()
    while len(out) < count:
        chunk = sock.recv(count - len(out))
        if not chunk:
            raise ConnectionError("client closed early")
        out += chunk
    return bytes(out)


def serve_h2_connection(conn: socket.socket, scenario_path: Path) -> None:
    """One scripted HTTP/2 connection: the aws-sdk default bedrock transport
    (NodeHttp2Handler, h2c prior knowledge) served by a raw-frame mock."""
    conn.settimeout(30)
    try:
        if recv_exact(conn, len(HTTP2_PREFACE)) != HTTP2_PREFACE:
            conn.close()
            return
        conn.sendall(h2_frame(H2_SETTINGS, 0, 0))
        stream_id: int | None = None
        while stream_id is None:
            header = recv_exact(conn, 9)
            length = int.from_bytes(header[:3], "big")
            ftype, flags = header[3], header[4]
            stream = int.from_bytes(header[5:9], "big") & 0x7FFFFFFF
            if length:
                recv_exact(conn, length)
            if ftype == H2_SETTINGS and not flags & H2_FLAG_ACK:
                conn.sendall(h2_frame(H2_SETTINGS, H2_FLAG_ACK, 0))
            elif ftype == H2_HEADERS and flags & H2_FLAG_END_HEADERS and stream:
                stream_id = stream

        scenario = json.loads(scenario_path.read_text())
        action = scenario.get("h2_action", "respond_error")
        if action == "respond_error":
            body = scenario["body"].encode()
            h2_send_response(
                conn, stream_id, scenario["status"], body, end_stream=True, request_id="probe-request-id"
            )
            # Keep the connection open until the client finishes its request
            # body and closes, so no RST races the response away.
            conn.settimeout(10)
            try:
                while conn.recv(65536):
                    pass
            except OSError:
                pass
        else:
            h2_send_partial_eventstream(conn, stream_id)
            time.sleep(0.2)
            if action == "rststream":
                conn.sendall(h2_frame(H2_RST_STREAM, 0, stream_id, H2_ERR_INTERNAL_ERROR.to_bytes(4, "big")))
                time.sleep(0.2)
                conn.close()
            elif action == "goaway":
                payload = stream_id.to_bytes(4, "big") + H2_ERR_PROTOCOL_ERROR.to_bytes(4, "big")
                conn.sendall(h2_frame(H2_GOAWAY, 0, 0, payload))
                time.sleep(0.2)
                conn.close()
            elif action == "tcp_rst":
                conn.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
                conn.close()
            elif action == "tcp_fin":
                conn.close()
            else:
                raise ValueError(f"unknown h2 action {action}")
    except (ConnectionError, OSError):
        pass
    finally:
        try:
            conn.close()
        except OSError:
            pass


class H2MockServer:
    """Accept-loop front for `serve_h2_connection`; one scenario file per run."""

    def __init__(self, scenario_path: Path):
        self.scenario_path = scenario_path
        self.socket = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.socket.bind(("127.0.0.1", 0))
        self.socket.listen(8)
        self.port = self.socket.getsockname()[1]
        self.stop = threading.Event()
        threading.Thread(target=self._loop, daemon=True).start()

    def _loop(self):
        self.socket.settimeout(0.5)
        while not self.stop.is_set():
            try:
                conn, _ = self.socket.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            threading.Thread(
                target=serve_h2_connection, args=(conn, self.scenario_path), daemon=True
            ).start()

    def shutdown(self):
        self.stop.set()
        try:
            self.socket.close()
        except OSError:
            pass


class ErrorMockHandler(BaseHTTPRequestHandler):
    """Serves scripted non-2xx responses per API path; the scenario file
    (rewritten by the probe between runs) selects status + body. The codex
    provider's websocket handshake (GET) is answered the same way, so every ws
    attempt fails and the provider falls back to SSE."""

    def log_message(self, *args):  # silence
        pass

    def _respond(self):
        try:
            scenario = json.loads(Path(self.server.scenario_path).read_text())  # type: ignore[attr-defined]
        except Exception:
            scenario = {"status": 500, "body": "{}"}
        # Request log for wire-level diffs, appended per request.
        log = Path(self.server.scenario_path).with_suffix(".requests.log")  # type: ignore[attr-defined]
        with log.open("a") as handle:
            handle.write(
                json.dumps(
                    {"command": self.command, "path": self.path, "scenario": scenario["name"]}
                )
                + "\n"
            )
        body = scenario["body"].encode()
        self.send_response(scenario["status"])
        self.send_header("Content-Type", scenario.get("contentType", "application/json"))
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _drain_request(self):
        """Read the full request body before answering: closing a socket
        with unread request data sends a TCP RST that can race the response
        body away from the client (the client then sees an empty body)."""
        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            length = 0
        remaining = length
        while remaining > 0:
            chunk = self.rfile.read(min(remaining, 65536))
            if not chunk:
                break
            remaining -= len(chunk)

    def do_POST(self):
        self._drain_request()
        self._respond()

    def do_GET(self):
        self._drain_request()
        self._respond()


def start_error_mock(scenario_path: Path) -> tuple[ThreadingHTTPServer, int]:
    server = ThreadingHTTPServer(("127.0.0.1", 0), ErrorMockHandler)
    server.scenario_path = str(scenario_path)  # type: ignore[attr-defined]
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


# One mock answer per scenario; the provider api decides which endpoint of
# the mock the request hits (mistral /v1/chat/completions, anthropic
# /v1/messages, codex /codex/responses, bedrock /model/<id>/converse-stream,
# openai-completions /v1/chat/completions).
HTTP_SCENARIOS = [
    {
        "name": "mistral_400_body",
        "api": "mistral-conversations",
        "status": 400,
        "body": json.dumps({"message": "mock mistral bad request"}),
    },
    {
        "name": "anthropic_400_body",
        "api": "anthropic-messages",
        "status": 400,
        "body": json.dumps(
            {
                "type": "error",
                "error": {"type": "invalid_request_error", "message": "mock anthropic bad request"},
            }
        ),
    },
    {
        "name": "codex_429_usage_limit",
        "api": "openai-codex-responses",
        "status": 429,
        "body": json.dumps(
            {
                "error": {
                    "code": "usage_limit_reached",
                    "message": "mock usage limit",
                    "plan_type": "free",
                }
            }
        ),
    },
    {
        "name": "codex_400_body",
        "api": "openai-codex-responses",
        "status": 400,
        "body": json.dumps({"error": {"message": "mock codex bad request"}}),
    },
    {
        "name": "bedrock_400_validation",
        "api": "bedrock-converse-stream",
        "status": 400,
        "body": json.dumps(
            {
                "__type": "com.amazonaws.bedrock#ValidationException",
                "message": "mock bedrock bad request",
            }
        ),
    },
    # The TS bedrock client speaks HTTP/2 by default; the Rust one HTTP/1.1.
    # The `AWS_BEDROCK_FORCE_HTTP1` mode is the comparable surface (the TS
    # request-handler override the product itself ships for proxies).
    {
        "name": "bedrock_400_validation_http1",
        "api": "bedrock-converse-stream",
        "force_http1": True,
        "status": 400,
        "body": json.dumps(
            {
                "__type": "com.amazonaws.bedrock#ValidationException",
                "message": "mock bedrock bad request",
            }
        ),
    },
    {
        "name": "google_400_body",
        "api": "google-generative-ai",
        "status": 400,
        "body": json.dumps(
            {
                "error": {
                    "code": 400,
                    "message": "mock google bad request",
                    "status": "INVALID_ARGUMENT",
                }
            }
        ),
    },
]

# HTTP/2 transport scenarios: a raw-frame h2c mock serves the bedrock default
# mode (the TS NodeHttp2Handler path). `h2_action` scripts the response:
# `respond_error` completes a parsed non-2xx; the mid-stream actions fail the
# transport inside the response body (RST_STREAM, GOAWAY, TCP RST/FIN); the
# `_http1` twin drives the http1 (AWS_BEDROCK_FORCE_HTTP1) surface at the same
# h2-only peer.
H2_SCENARIOS = [
    {
        "name": "bedrock_h2_400_validation",
        "api": "bedrock-converse-stream",
        "transport": "h2",
        "h2_action": "respond_error",
        "status": 400,
        "body": json.dumps(
            {
                "__type": "com.amazonaws.bedrock#ValidationException",
                "message": "mock bedrock bad request",
            }
        ),
    },
    {
        "name": "bedrock_h2_rststream_midstream",
        "api": "bedrock-converse-stream",
        "transport": "h2",
        "h2_action": "rststream",
    },
    {
        "name": "bedrock_h2_goaway_midstream",
        "api": "bedrock-converse-stream",
        "transport": "h2",
        "h2_action": "goaway",
    },
    {
        "name": "bedrock_h2_tcp_rst_midstream",
        "api": "bedrock-converse-stream",
        "transport": "h2",
        "h2_action": "tcp_rst",
    },
    {
        "name": "bedrock_h2_tcp_fin_midstream",
        "api": "bedrock-converse-stream",
        "transport": "h2",
        "h2_action": "tcp_fin",
    },
    {
        "name": "bedrock_h2_400_validation_http1",
        "api": "bedrock-converse-stream",
        "transport": "h2",
        "h2_action": "respond_error",
        "force_http1": True,
        "status": 400,
        "body": json.dumps(
            {
                "__type": "com.amazonaws.bedrock#ValidationException",
                "message": "mock bedrock bad request",
            }
        ),
    },
]

CONNECTION_SCENARIOS = [
    {"name": "connection_anthropic-messages", "api": "anthropic-messages", "connection": True},
    {"name": "connection_mistral-conversations", "api": "mistral-conversations", "connection": True},
    {"name": "connection_openai-codex-responses", "api": "openai-codex-responses", "connection": True},
    {"name": "connection_bedrock-converse-stream", "api": "bedrock-converse-stream", "connection": True},
    {
        "name": "connection_bedrock-converse-stream_http1",
        "api": "bedrock-converse-stream",
        "force_http1": True,
        "connection": True,
    },
    {"name": "connection_google-generative-ai", "api": "google-generative-ai", "connection": True},
    {"name": "connection_openai-completions", "api": "openai-completions", "connection": True},
]

DEAD_PORT = 1  # nothing listens here: every connect() is refused instantly


def scrubbed_env(agent_dir: Path, tmpdir: Path, extra: dict | None = None) -> dict:
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.startswith(SCRUB_ENV_PREFIXES) and k not in SCRUB_ENV_KEYS
    }
    env["PRIME_AGENT_CODING_AGENT_DIR"] = str(agent_dir)
    env["TMPDIR"] = str(tmpdir)
    if extra:
        env.update(extra)
    return env


def prepare_agent_dir(agent_dir: Path, api: str, base_url: str, api_key: str) -> str:
    """models.json custom provider + retry-disabled settings; returns the
    provider id. Retries stay off so each failure surfaces once,
    deterministically."""
    agent_dir.mkdir(parents=True, exist_ok=True)
    provider_id = f"mock-{api}"
    models = {
        "providers": {
            provider_id: {
                "api": api,
                "baseUrl": base_url,
                "apiKey": api_key,
                "models": [
                    {
                        "id": "mock-1",
                        "name": "Mock 1",
                        "api": api,
                        "baseUrl": base_url,
                        "contextWindow": 128000,
                        "maxTokens": 4096,
                    }
                ],
            }
        }
    }
    (agent_dir / "models.json").write_text(json.dumps(models, indent=1))
    (agent_dir / "settings.json").write_text(json.dumps({"retry": {"enabled": False}}, indent=1))
    return provider_id


def run_scenario(
    binary: str, agent_dir: Path, tmpdir: Path, provider_id: str, extra: dict | None = None
) -> dict:
    tmpdir.mkdir(parents=True, exist_ok=True)
    argv = [binary, "-p", "Reply with the word done.", "--provider", provider_id, "--model", "mock-1"]
    start = time.time()
    try:
        proc = subprocess.run(
            argv,
            env=scrubbed_env(agent_dir, tmpdir, extra),
            cwd=str(tmpdir),
            input="",
            capture_output=True,
            text=True,
            timeout=120,
        )
        return {
            "exit_code": proc.returncode,
            "stdout": proc.stdout,
            "stderr": proc.stderr,
            "duration_s": round(time.time() - start, 2),
            "timeout": False,
        }
    except subprocess.TimeoutExpired as exc:
        return {
            "exit_code": None,
            "stdout": exc.stdout or "",
            "stderr": exc.stderr or "",
            "duration_s": round(time.time() - start, 2),
            "timeout": True,
        }


def session_evidence(agent_dir: Path) -> dict:
    """The persisted assistant message (errorMessage + provider_stream_failure
    diagnostic) from the most recent session file."""
    sessions = agent_dir / "sessions"
    files = sorted(sessions.glob("*.jsonl"), key=lambda p: p.stat().st_mtime) if sessions.exists() else []
    if not files:
        return {"session_file": None}
    result: dict = {"session_file": files[-1].name}
    for line in files[-1].read_text().splitlines():
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        message = row.get("message") if isinstance(row, dict) else None
        if not isinstance(message, dict) or message.get("role") != "assistant":
            continue
        if message.get("errorMessage") is not None or message.get("stopReason") == "error":
            diagnostics = message.get("diagnostics") or []
            failure = next((d for d in diagnostics if d.get("type") == "provider_stream_failure"), None)
            result["errorMessage"] = message.get("errorMessage")
            result["stopReason"] = message.get("stopReason")
            if failure:
                result["diagnostic"] = {
                    "name": (failure.get("error") or {}).get("name"),
                    "kind": (failure.get("details") or {}).get("kind"),
                    "status": (failure.get("details") or {}).get("status"),
                    "providerErrorType": (failure.get("details") or {}).get("providerErrorType"),
                    "retryAfterMs": (failure.get("details") or {}).get("retryAfterMs"),
                }
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--side", required=True, choices=("ts", "rust"))
    parser.add_argument("--binary", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()

    out = Path(args.out)
    if args.side == "ts":
        # Fail fast: a non-TS ts binary (e.g. a Rust build symlinked onto
        # PATH as `prime-agent`) would capture the wrong product's errors.
        ts_identity.assert_ts_side_is_the_ts_product(args.binary)
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix=f"provider-errors-{args.side}-"))
    scenario_path = root / "scenario.json"

    server, port = start_error_mock(scenario_path)
    mock_url = f"http://127.0.0.1:{port}"
    h2_server = H2MockServer(scenario_path)
    h2_url = f"http://127.0.0.1:{h2_server.port}"
    jwt = mock_codex_jwt()

    scenarios: list[dict] = list(HTTP_SCENARIOS) + H2_SCENARIOS + CONNECTION_SCENARIOS

    evidence = []
    for scenario in scenarios:
        api = scenario["api"]
        if scenario.get("connection"):
            base_url = f"http://127.0.0.1:{DEAD_PORT}"
        elif scenario.get("transport") == "h2":
            base_url = h2_url
        else:
            base_url = mock_url
        if "status" in scenario or "h2_action" in scenario:
            scenario_path.write_text(json.dumps(scenario))
        api_key = jwt if api == "openai-codex-responses" else "mock-key"
        extra = {}
        if api == "bedrock-converse-stream":
            extra["AWS_BEDROCK_SKIP_AUTH"] = "1"
            extra["AWS_REGION"] = "us-east-1"
        if scenario.get("force_http1"):
            extra["AWS_BEDROCK_FORCE_HTTP1"] = "1"

        agent_dir = root / scenario["name"] / "agent"
        provider_id = prepare_agent_dir(agent_dir, api, base_url, api_key)
        run = run_scenario(args.binary, agent_dir, root / scenario["name"] / "tmp", provider_id, extra)
        entry = {"scenario": scenario["name"], "api": api, "run": run}
        entry.update(session_evidence(agent_dir))
        evidence.append(entry)
        print(json.dumps(entry, indent=1))
        (out / f"{scenario['name']}.json").write_text(json.dumps(entry, indent=1))

    (out / "evidence.json").write_text(json.dumps(evidence, indent=1))
    server.shutdown()
    h2_server.shutdown()
    return 0


if __name__ == "__main__":
    sys.exit(main())
