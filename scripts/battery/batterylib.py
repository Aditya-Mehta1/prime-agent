#!/usr/bin/env python3
"""Shared helpers for the parity battery: isolated per-side environments,
tmux frame capture, a JSONL daemon-wire client, and evidence-file helpers.

The battery drives the TS binary (ground truth) and the Rust binary side by
side against a deterministic mock provider (mock_provider.py). Nothing here
is part of the product.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import socket
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path

NL = chr(10)
# Environment keys set by this box's own prime-agent session/worker; they must
# never leak into spawned daemons, workers, or interactive sessions.
SCRUB_ENV_PREFIXES = ("PRIME_AGENT_INTERNAL", "RLM_")
SCRUB_ENV_KEYS = (
    "TMUX",
    "TMUX_PANE",
    "PRIME_AGENT_SESSION_DIR",
    "PRIME_AGENT_CODING_AGENT_DIR",
    "PRIME_AGENT_CODING_AGENT_SESSION_DIR",
    "PRIME_AGENT_KERNEL_OWNER_PID",
    "PI_CODING_AGENT_DIR",
    "PI_CODING_AGENT",
)

TMUX_SIZE = (120, 36)


def scrubbed_env(agent_dir: Path, tmpdir: Path, extra: dict | None = None) -> dict:
    """A clean env: no worker/role markers, isolated agent dir + TMPDIR."""
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


def run_cmd(
    argv: list[str],
    env: dict,
    cwd: Path,
    timeout: float = 120.0,
    stdin_text: str | None = "",
) -> dict:
    """Run a command, returning a record with stdout/stderr/exit/duration."""
    start = time.time()
    try:
        proc = subprocess.run(
            argv,
            env=env,
            cwd=str(cwd),
            input=stdin_text,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        return {
            "argv": argv,
            "exit_code": proc.returncode,
            "stdout": proc.stdout,
            "stderr": proc.stderr,
            "duration_s": round(time.time() - start, 2),
            "timeout": False,
        }
    except subprocess.TimeoutExpired as exc:
        return {
            "argv": argv,
            "exit_code": None,
            "stdout": exc.stdout.decode() if isinstance(exc.stdout, bytes) else (exc.stdout or ""),
            "stderr": exc.stderr.decode() if isinstance(exc.stderr, bytes) else (exc.stderr or ""),
            "duration_s": round(time.time() - start, 2),
            "timeout": True,
        }


class MockProvider:
    """One mock-provider process per side, so request logs never interleave."""

    def __init__(self, battery_dir: Path, port_offset_holder: list[int]):
        self.script_path = battery_dir / "mock-script.json"
        self.requests_path = Path(str(self.script_path) + ".requests.jsonl")
        for path in (self.script_path, self.requests_path):
            if path.exists():
                path.unlink()
        self.port = None
        self._holder = port_offset_holder
        self._proc = None

    def set_responses(self, responses: list[dict]) -> None:
        with open(self.script_path, "w") as f:
            json.dump({"responses": responses}, f)

    def start(self) -> int:
        here = Path(__file__).parent
        for _ in range(20):
            self._proc = subprocess.Popen(
                ["python3", str(here / "mock_provider.py"), str(self.script_path)],
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
            )
            line = self._proc.stdout.readline()
            try:
                self.port = int(line.strip())
                return self.port
            except ValueError:
                self._proc.kill()
                time.sleep(0.1)
        raise RuntimeError("mock provider failed to start")

    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}/v1"

    def requests(self) -> list[dict]:
        if not self.requests_path.exists():
            return []
        out = []
        for line in self.requests_path.read_text().splitlines():
            if line.strip():
                out.append(json.loads(line))
        return out

    def stop(self) -> None:
        if self._proc:
            self._proc.terminate()
            try:
                self._proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self._proc.kill()


@dataclass
class Side:
    """One product side (ts or rust) with fully isolated state."""

    name: str
    binary: str
    root: Path  # evidence/<side>
    agent_dir: Path
    work_dir: Path
    daemon_socket: Path
    mock: MockProvider
    env: dict = field(default_factory=dict)
    daemon_proc: subprocess.Popen | None = None

    @property
    def base_url(self) -> str:
        return self.mock.url()

    def write_models_json(self) -> None:
        self.agent_dir.mkdir(parents=True, exist_ok=True)
        models = {
            "providers": {
                "prime-inference": {
                    "api": "openai-completions",
                    "baseUrl": self.base_url,
                    "apiKey": "sk-battery",
                    "models": [
                        {
                            "id": "mock-1",
                            "name": "Mock 1",
                            "api": "openai-completions",
                            "baseUrl": self.base_url,
                            "contextWindow": 128000,
                            "maxTokens": 4096,
                        }
                    ],
                }
            }
        }
        (self.agent_dir / "models.json").write_text(json.dumps(models, indent=1))

    # -- evidence helpers ---------------------------------------------------

    def evidence(self, flow: str, name: str, text: str) -> Path:
        path = self.root / flow / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path

    def evidence_json(self, flow: str, name: str, obj) -> Path:
        path = self.root / flow / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(obj, indent=1, sort_keys=False))
        return path

    def sessions_dir(self) -> Path:
        return self.agent_dir / "sessions"

    def session_files(self) -> list[Path]:
        d = self.sessions_dir()
        if not d.exists():
            return []
        return sorted(d.glob("*.jsonl"), key=lambda p: p.stat().st_mtime)

    # -- daemon --------------------------------------------------------------

    def start_daemon(self, timeout: float = 30.0) -> None:
        """Start the daemon on this side's socket (identical argv both sides)."""
        log = self.root / "daemon.log"
        log.parent.mkdir(parents=True, exist_ok=True)
        with open(log, "ab") as logfile:
            self.daemon_proc = subprocess.Popen(
                [self.binary, "--mode", "daemon", "--daemon-socket", str(self.daemon_socket)],
                env=self.env,
                cwd=str(self.work_dir),
                stdin=subprocess.DEVNULL,
                stdout=logfile,
                stderr=logfile,
            )
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.daemon_socket.exists():
                try:
                    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
                        probe.connect(str(self.daemon_socket))
                        return
                except OSError:
                    pass
            if self.daemon_proc.poll() is not None:
                raise RuntimeError(
                    f"{self.name} daemon exited early: {log.read_text(errors='replace')[-2000:]}"
                )
            time.sleep(0.2)
        raise RuntimeError(f"{self.name} daemon socket never appeared")

    def stop_daemon(self) -> None:
        if self.daemon_proc and self.daemon_proc.poll() is None:
            self.daemon_proc.terminate()
            try:
                self.daemon_proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.daemon_proc.kill()
        # Detached workers spawned by the daemon may outlive it; the next
        # battery run uses a fresh TMPDIR so stale sockets cannot collide.


class Wire:
    """JSONL daemon-protocol client (protocol 7, the TS wire format)."""

    def __init__(self, socket_path: Path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(90)
        self.sock.connect(str(socket_path))
        self.buf = b""
        self.events: list = []
        self.hello = self.read_line()

    def send_command(self, command_id: str, command: dict) -> None:
        envelope = {
            "type": "command",
            "id": command_id,
            "protocol": {"name": "prime-agent.daemon", "version": 7},
            "command": command,
        }
        self.sock.sendall((json.dumps(envelope) + chr(10)).encode())

    def read_line(self, timeout: float = 60.0) -> dict:
        """Read one JSON line; raises TimeoutError when nothing arrives."""
        deadline = time.time() + timeout
        while bytes([10]) not in self.buf:
            remaining = deadline - time.time()
            if remaining <= 0:
                raise TimeoutError("daemon line timeout")
            self.sock.settimeout(max(0.1, remaining))
            chunk = self.sock.recv(65536)
            if not chunk:
                raise EOFError("daemon closed the connection")
            self.buf += chunk
        line, self.buf = self.buf.split(bytes([10]), 1)
        return json.loads(line.decode())

    def request(self, command_id: str, command: dict, timeout: float = 60.0) -> dict:
        """Send a command; return the response carrying the matching id.

        Side events (non-matching lines) are recorded into `self.events`.
        """
        self.send_command(command_id, command)
        deadline = time.time() + timeout
        while True:
            remaining = deadline - time.time()
            if remaining <= 0:
                raise TimeoutError(f"no response for command {command_id}")
            line = self.read_line(timeout=remaining)
            if line.get("id") == command_id:
                return line
            self.events.append(line)

    def drain(self, timeout: float = 3.0) -> list:
        """Read whatever the daemon sends for up to `timeout` seconds,
        buffering side events into `self.events`; returns the new events."""
        deadline = time.time() + timeout
        fresh = []
        while True:
            remaining = deadline - time.time()
            if remaining <= 0:
                return fresh
            try:
                line = self.read_line(timeout=remaining)
            except TimeoutError:
                return fresh
            fresh.append(line)
            self.events.append(line)

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


# -- tmux ---------------------------------------------------------------------

def tmux(*args: str, check: bool = True) -> subprocess.CompletedProcess:
    """Run tmux on the default socket (never the agentui socket)."""
    env = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE")}
    proc = subprocess.run(
        ["env", "-u", "TMUX", "-u", "TMUX_PANE", "tmux", *args],
        env=env,
        capture_output=True,
        text=True,
    )
    if check and proc.returncode != 0:
        raise RuntimeError(f"tmux {args} failed: {proc.stderr}")
    return proc


def tmux_launch(
    session: str, command: list[str], env: dict, cwd: Path, size: tuple[int, int] = TMUX_SIZE
) -> None:
    """Create a detached session of `size` running `command` with `env`.

    tmux panes inherit the tmux server's environment, not the client's, so
    the pane command is wrapped in `env KEY=VALUE ...` (and TMUX unset) to
    guarantee isolation from the ambient agent session.

    The pane runs with `-c cwd` as its working directory, so a relative
    `command[0]` (e.g. `target/release/prime-agent` passed from the repo
    root) would resolve against the pane cwd and vanish instantly. Resolve
    the binary to an absolute path before it reaches the pane.
    """
    if command and not os.path.isabs(command[0]) and "/" in command[0]:
        resolved = Path(command[0]).resolve()
        if not resolved.exists():
            raise RuntimeError(f"tmux_launch: binary {command[0]} not found at {resolved}")
        command = [str(resolved), *command[1:]]
    # The pane inherits the tmux server env, so only the deltas (the scrubbed
    # overrides) need explicit assignment; everything else stays inherited.
    assignment = [
        f"{key}={value}"
        for key, value in sorted(env.items())
        if os.environ.get(key) != value or key in ("PRIME_AGENT_CODING_AGENT_DIR", "TMPDIR")
    ]
    unset = ["-u", "TMUX", "-u", "TMUX_PANE"]
    for key in SCRUB_ENV_KEYS:
        if key not in ("TMUX", "TMUX_PANE"):
            unset += ["-u", key]
    # The tmux server inherited this session's worker markers; unset every
    # one of them so pane processes never see daemon-worker identity.
    for key in list(os.environ):
        if key.startswith(SCRUB_ENV_PREFIXES):
            unset += ["-u", key]
    # tmux runs multi-argument pane commands through its default shell, so
    # wrap explicitly: sh -c 'exec env -u ... KEY=V ... <command>'.
    words = (
        ["exec", "/usr/bin/env"]
        + unset
        + assignment
        + list(command)
    )
    import shlex

    shell_command = " ".join(shlex.quote(w) for w in words)
    tmux(
        "new-session",
        "-d",
        "-x",
        str(size[0]),
        "-y",
        str(size[1]),
        "-s",
        session,
        "-c",
        str(cwd),
        "/bin/sh",
        "-c",
        shell_command,
    )


def tmux_resize(session: str, size: tuple[int, int]) -> None:
    """Resize a detached session's window (frame-parity captures at a
    second terminal size)."""
    tmux("resize-window", "-t", session, "-x", str(size[0]), "-y", str(size[1]), check=False)


def tmux_capture(session: str, pane: str = "0") -> str:
    return tmux("capture-pane", "-p", "-t", f"{session}:{pane}").stdout


def tmux_send(session: str, keys: str, enter: bool = True) -> None:
    tmux("send-keys", "-t", session, keys, "Enter" if enter else "")


def tmux_kill(session: str) -> None:
    tmux("kill-session", "-t", session, check=False)


def tmux_wait_text(session: str, pattern: str, timeout: float = 60.0, poll: float = 0.5) -> str:
    """Poll a pane until `pattern` appears; returns the final frame."""
    deadline = time.time() + timeout
    frame = ""
    while time.time() < deadline:
        frame = tmux_capture(session)
        if re.search(pattern, frame):
            return frame
        time.sleep(poll)
    return frame


# -- normalization + diff helpers ---------------------------------------------

def normalize(value, skip_keys=("timestamp", "id", "parentId", "sessionId", "socketPath", "pid")):
    """Recursively normalize volatile keys for cross-side comparisons."""

    def norm(v):
        if isinstance(v, dict):
            return {k: ("<norm>" if k in skip_keys else norm(val)) for k, val in v.items()}
        if isinstance(v, list):
            return [norm(x) for x in v]
        return v

    return norm(value)


def shape_of(value):
    """A structural fingerprint: dict keys / list shapes / type names."""

    def shp(v):
        if isinstance(v, dict):
            return {k: shp(val) for k, val in sorted(v.items())}
        if isinstance(v, list):
            return [shp(v[0])] if v else []
        return type(v).__name__

    return shp(value)


def copy_dir_contents(src: Path, dst: Path) -> None:
    if not src.exists():
        return
    if dst.exists():
        shutil.rmtree(dst)
    shutil.copytree(src, dst)


def write_report(path: Path, title: str, sections: list[dict]) -> None:
    """Write the markdown gap report."""
    lines = [f"# {title}", ""]
    for section in sections:
        lines.append(f"## {section['title']}")
        lines.append("")
        for row in section.get("rows", []):
            lines.append(f"- {row}")
        lines.append("")
    path.write_text(NL.join(lines))
