#!/usr/bin/env python3
"""Deterministic OpenAI-compatible mock provider for the parity battery.

Serves `/v1/chat/completions` (SSE streaming) and `/v1/models` from a JSON
script file, so the TS binary and the Rust binary can be driven side by side
with identical model responses. Not part of the product; battery harness
only.

Script file format:
    {
      "responses": [
        {"text": "hello"},
        {"toolCall": {"name": "bash", "arguments": {"command": "echo hi"}}},
        {"text": "done"}
      ]
    }
Each POST to /chat/completions pops the next scripted response (round-robin
when the script runs dry: the last entry repeats). Every request and the raw
request bodies are logged to `<script>.requests.jsonl` for wire-level diffs.
"""

from __future__ import annotations

import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class MockState:
    def __init__(self, script_path: str):
        self.script_path = script_path
        self.script_mtime = None
        self.responses = []
        self.index = 0
        self.request_log_path = script_path + ".requests.jsonl"
        self.lock = threading.Lock()
        self._reload_if_changed()

    def _reload_if_changed(self):
        """Reload the script when the file changes; a new script restarts
        the response cursor at 0, so each battery flow can swap scripts
        against one long-lived provider process."""
        mtime = os.stat(self.script_path).st_mtime
        if mtime != self.script_mtime:
            with open(self.script_path) as f:
                script = json.load(f)
            self.script_mtime = mtime
            self.responses = script["responses"]
            self.index = 0

    def next_response(self):
        with self.lock:
            self._reload_if_changed()
            entry = self.responses[min(self.index, len(self.responses) - 1)]
            self.index += 1
            return entry


def chunk_delta(delta, finish_reason=None):
    return {
        "id": "chatcmpl-battery",
        "object": "chat.completion.chunk",
        "created": 1750000000,
        "model": "mock-1",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
    }


class Handler(BaseHTTPRequestHandler):
    state: MockState
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _json(self, status, obj):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            self._json(
                200,
                {"object": "list", "data": [{"id": "mock-1", "object": "model", "owned_by": "battery"}]},
            )
        else:
            self._json(404, {"error": {"message": f"unknown path {self.path}"}})

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length)
        try:
            body = json.loads(raw)
        except json.JSONDecodeError:
            self._json(400, {"error": {"message": "invalid json"}})
            return
        with open(self.state.request_log_path, "a") as f:
            f.write(
                json.dumps(
                    {"path": self.path, "body": body, "auth": self.headers.get("Authorization", "")}
                )
                + "\n"
            )
        if "/chat/completions" not in self.path:
            self._json(404, {"error": {"message": f"unknown path {self.path}"}})
            return
        entry = self.state.next_response()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

        def send(obj):
            data = f"data: {json.dumps(obj)}\r\n\r\n".encode()
            # Proper HTTP chunked framing: each write is one chunk.
            self.wfile.write(f"{len(data):X}\r\n".encode() + data + b"\r\n")

        try:
            if "text" in entry:
                send(chunk_delta({"role": "assistant", "content": entry["text"]}))
            elif "toolCall" in entry:
                call = entry["toolCall"]
                args = json.dumps(call["arguments"])
                send(
                    chunk_delta(
                        {
                            "role": "assistant",
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": call.get("id", "call_battery_1"),
                                    "type": "function",
                                    "function": {"name": call["name"], "arguments": ""},
                                }
                            ],
                        }
                    )
                )
                # Stream the arguments JSON in two pieces like a real server.
                half = max(1, len(args) // 2)
                send(
                    chunk_delta(
                        {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "function": {"arguments": args[:half]},
                                }
                            ]
                        }
                    )
                )
                send(
                    chunk_delta(
                        {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "function": {"arguments": args[half:]},
                                }
                            ],
                        }
                    )
                )
            else:
                send(chunk_delta({"role": "assistant", "content": ""}))
            send(chunk_delta({}, "stop"))
            send(
                {
                    "id": "chatcmpl-battery",
                    "object": "chat.completion.chunk",
                    "created": 1750000000,
                    "model": "mock-1",
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 100,
                        "completion_tokens": 10,
                        "total_tokens": 110,
                        "prompt_tokens_details": {"cached_tokens": 80},
                    },
                }
            )
            done = b"data: [DONE]\r\n\r\n"
            self.wfile.write(f"{len(done):X}\r\n".encode() + done + b"\r\n")
            # Terminal chunk: end of the chunked body.
            self.wfile.write(b"0\r\n\r\n")
        except (BrokenPipeError, ConnectionResetError):
            pass


def main():
    script_path = sys.argv[1]
    Handler.state = MockState(script_path)
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    print(server.server_address[1], flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
