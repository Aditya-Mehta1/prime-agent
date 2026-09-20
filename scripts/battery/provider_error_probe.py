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
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

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
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix=f"provider-errors-{args.side}-"))
    scenario_path = root / "scenario.json"

    server, port = start_error_mock(scenario_path)
    mock_url = f"http://127.0.0.1:{port}"
    jwt = mock_codex_jwt()

    scenarios: list[dict] = list(HTTP_SCENARIOS) + CONNECTION_SCENARIOS

    evidence = []
    for scenario in scenarios:
        api = scenario["api"]
        base_url = f"http://127.0.0.1:{DEAD_PORT}" if scenario.get("connection") else mock_url
        if "status" in scenario:
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
    return 0


if __name__ == "__main__":
    sys.exit(main())
