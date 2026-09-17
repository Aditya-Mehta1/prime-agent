#!/usr/bin/env python3
"""Standing live A/B parity battery: run the TS binary (ground truth) and the
Rust binary side by side over the same user flows, against the same
deterministic mock provider, and capture evidence + a gap report.

One-command re-run (from the repo root):

    python3 scripts/battery/run_battery.py

Options:
    --flows f1,f2       comma list to run (default: all)
    --ts-bin PATH       TS binary (default: prime-agent on PATH)
    --rust-bin PATH     Rust binary (default: target/release/prime-agent)
    --runs-root PATH    evidence root (default: scripts/battery/runs)

Each run writes scripts/battery/runs/<UTC stamp>/ with:
    ts/, rust/          per-side evidence per flow (frames, wire logs, copies)
    report.md           automated comparison table + findings
    findings.json       machine-readable findings

Isolation: every side gets its own agent dir, TMPDIR, daemon socket, and mock
provider port. The ambient daemon (this session's own host) is never touched:
all spawned daemons live on battery-specific sockets, and cleanup kills only
processes whose environment references the run directory.
"""

from __future__ import annotations

import argparse
import datetime
import re
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import batterylib as B  # noqa: E402
import perf as P  # noqa: E402

NL = chr(10)

ALL_FLOWS = ["f1_launch", "f2_prompt", "f3_tool", "f4_commands", "f5_side_questions", "f6_attach", "f7_compaction", "f8_resume", "f9_agents_view", "f10_perf", "f11_provider_failure"]

# PERF row thresholds, measured on this box by scripts/battery/perf.py:
# the invariant is that the Rust binary is never materially slower than
# the TS binary on cold startup or keystroke-to-render latency.
PERF_STARTUP_MAX_RATIO = 1.5
PERF_TYPING_MAX_RATIO = 1.5
PERF_RUNS = 3

HELLO_TEXT = "battery hello from mock"
# The dashboard status-line model the TS daemon asks after each turn (B-7).
STATUSLINE_MODEL_ID = "qwen/qwen3-30b-a3b-instruct-2507"
AGENT_STATUS_SYSTEM_PROMPT_PREFIX = "You generate a status line for an AI coding agent dashboard."


def is_statusline_request(req) -> bool:
    """A dashboard status-line request: the small model plus the fixed system prompt."""
    body = req.get("body") or {}
    messages = body.get("messages", [])
    system = ""
    if messages and isinstance(messages[0].get("content"), str):
        system = messages[0]["content"]
    return (
        body.get("model") == STATUSLINE_MODEL_ID
        and system.startswith(AGENT_STATUS_SYSTEM_PROMPT_PREFIX)
    )


class Battery:
    def __init__(self, runs_root: Path, ts_bin: str, rust_bin: str, flows: list[str]):
        self.stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        self.run_dir = runs_root / self.stamp
        self.run_dir.mkdir(parents=True)
        self.flows = flows
        self.ts_bin = ts_bin
        self.rust_bin = rust_bin
        self.findings: list[dict] = []
        self.sides: dict[str, B.Side] = {}
        self.runid = "vbat" + self.stamp

    # -- setup / teardown ----------------------------------------------------

    def make_side(self, name: str, binary: str) -> B.Side:
        root = self.run_dir / name
        root.mkdir(parents=True, exist_ok=True)
        agent = root / "agent"
        work = root / "work"
        work.mkdir(parents=True, exist_ok=True)
        if agent.exists():
            shutil.rmtree(agent)
        agent.mkdir(parents=True)
        mock = B.MockProvider(root, [])
        mock.set_responses([{"text": HELLO_TEXT}])
        mock.start()
        # Short TMPDIR: worker sockets live under TMPDIR and must stay under
        # the 107-char AF_UNIX path limit (the run dir is too deep).
        tmpdir = Path("/tmp") / f"{self.runid}-{name}"
        if tmpdir.exists():
            shutil.rmtree(tmpdir)
        tmpdir.mkdir(parents=True)
        side = B.Side(
            name=name,
            binary=binary,
            root=root,
            agent_dir=agent,
            work_dir=work,
            daemon_socket=root / "daemon.sock",
            mock=mock,
        )
        side.env = B.scrubbed_env(agent, tmpdir)
        # Point both products at the mock through a provider whose API-key
        # resolution both support: both read the models.json apiKey (the
        # env key stays as the fallback both products share).
        side.env["PRIME_API_KEY"] = "sk-battery"
        side.write_models_json()
        self.sides[name] = side
        return side

    def record(self, flow: str, category: str, summary: str, evidence="", gap: bool = True) -> None:
        if isinstance(evidence, Path):
            evidence = str(evidence.relative_to(self.run_dir))
        self.findings.append(
            {
                "flow": flow,
                "category": category,
                "gap": gap,
                "summary": summary,
                "evidence": evidence,
            }
        )

    def new_mock_requests(self, side: B.Side, mark: int) -> list[dict]:
        return side.mock.requests()[mark:]

    def copy_sessions(self, side: B.Side, flow: str) -> None:
        dst = side.root / flow / "sessions"
        if dst.exists():
            shutil.rmtree(dst)
        src = side.sessions_dir()
        if src.exists():
            shutil.copytree(src, dst)

    def stop(self) -> None:
        """Shut down every battery-owned daemon and process."""
        for side in self.sides.values():
            # Graceful wire shutdown, best effort.
            try:
                wire = B.Wire(side.daemon_socket)
                wire.send_command("sd", {"type": "shutdown"})
                wire.close()
                time.sleep(2)
            except Exception:
                pass
        # Kill anything still referencing the run dir (own processes only).
        self.kill_run_processes()
        for side in self.sides.values():
            side.mock.stop()
            if side.daemon_proc and side.daemon_proc.poll() is None:
                side.daemon_proc.terminate()
        subprocess.run(["tmux", "ls", "-F", "#{session_name}"], capture_output=True, text=True)

    def kill_run_processes(self) -> None:
        run_path = str(self.run_dir)
        for proc_dir in Path("/proc").iterdir():
            if not proc_dir.name.isdigit():
                continue
            pid = int(proc_dir.name)
            if pid == 1 or pid == os.getpid():
                continue
            try:
                environ = (proc_dir / "environ").read_bytes().decode(errors="replace")
                cmdline = (proc_dir / "cmdline").read_bytes().decode(errors="replace")
            except (OSError, PermissionError):
                continue
            if run_path in environ or run_path in cmdline:
                try:
                    os.kill(pid, 15)
                except (ProcessLookupError, PermissionError):
                    pass
        time.sleep(1)

    def kill_tmux_sessions(self) -> None:
        out = subprocess.run(
            ["tmux", "ls", "-F", "#{session_name}"], capture_output=True, text=True
        ).stdout
        for name in out.split():
            if name.startswith(self.runid):
                B.tmux_kill(name)

    # -- flows ---------------------------------------------------------------

    def f1_launch(self) -> None:
        """Fresh install state: splash, first-run notice, first prompt+reply."""
        noticed: dict[str, bool] = {}
        for side in (self.sides["ts"], self.sides["rust"]):
            flow = "f1_launch"
            session = f"{self.runid}-f1-{side.name}"
            argv = [
                side.binary,
                "--daemon-socket",
                str(side.daemon_socket),
                "--provider",
                "prime-inference",
                "--model",
                "mock-1",
                "--offline",
            ]
            B.tmux_launch(session, argv, side.env, side.work_dir)
            # The TS splash animation swallows keystrokes, and a fresh install
            # first shows a trace-sharing notice: wait for the notice (up to
            # 30s), answer it if present, then wait for the settled main
            # screen (two identical frames) before typing.
            frame = ""
            deadline = time.time() + 30
            while time.time() < deadline:
                frame = B.tmux_capture(session)
                if "Share agent traces" in frame:
                    break
                time.sleep(1.0)
            side.evidence(flow, "01-launch.txt", frame)
            noticed[side.name] = "Share agent traces" in frame
            if noticed[side.name]:
                # B-2: the notice is answerable (Down + Enter = Not now) and
                # the pane settles into the main screen afterwards.
                B.tmux_send(session, "Down")
                time.sleep(0.5)
                B.tmux_send(session, "Enter")
                time.sleep(2.0)
                side.evidence(flow, "02-notice-answered.txt", B.tmux_capture(session))
            elif "prime agent" in frame.lower() or "manage" in frame:
                side.evidence(flow, "02-no-notice.txt", frame)
            else:
                self.record(flow, "visual", f"{side.name} launch frame shows no splash/welcome text", gap=True)
            # Wait for a settled main screen before sending the prompt.
            stable = False
            deadline = time.time() + 30
            while time.time() < deadline and not stable:
                first = B.tmux_capture(session)
                time.sleep(2.0)
                second = B.tmux_capture(session)
                stable = first == second and ("manage" in first or ">" in first)
            if not stable:
                self.record(flow, "behavior", f"{side.name}: TUI never reached a stable main screen within 30s", gap=True)
            # Send the first prompt.
            mark = len(side.mock.requests())
            B.tmux_send(session, "hi")
            frame2 = B.tmux_wait_text(session, HELLO_TEXT, timeout=90)
            side.evidence(flow, "02-after-first-prompt.txt", frame2)
            if HELLO_TEXT not in frame2:
                self.record(
                    flow,
                    "behavior",
                    f"{side.name}: first interactive prompt did not reach the mock provider (mock requests: {len(self.new_mock_requests(side, mark))})",
                    evidence=side.root / flow / "02-after-first-prompt.txt",
                )
            else:
                self.record(flow, "behavior", f"{side.name}: first interactive prompt answered by the mock provider", gap=False)
            # B-1: the explicit --provider/--model flags must be authoritative
            # end-to-end; the first request the mock sees must be the flagged
            # model, not a fallback.
            requests = self.new_mock_requests(side, mark)
            side.evidence_json(flow, "first-prompt-mock-requests.json", requests)
            request_models = [
                request["body"].get("model") for request in requests if request.get("body")
            ]
            if request_models and all(model == "mock-1" for model in request_models):
                self.record(
                    flow,
                    "protocol",
                    f"{side.name}: interactive model flags are authoritative (request model: {request_models[0]})",
                    gap=False,
                )
            else:
                self.record(
                    flow,
                    "protocol",
                    f"{side.name}: interactive model flags did not reach the provider request (models seen: {request_models})",
                    evidence=side.root / flow / "first-prompt-mock-requests.json",
                )
            B.tmux_kill(session)
        # B-2 cross-side verdict: the first-run surface (splash + trace
        # notice) is parity when both products show and answer it.
        flow = "f1_launch"
        if noticed.get("ts") and noticed.get("rust"):
            self.record(
                flow,
                "visual",
                "first-run splash + trace-sharing notice rendered and answerable on both sides (fresh install)",
                gap=False,
            )
        elif noticed.get("ts") and not noticed.get("rust"):
            self.record(
                flow,
                "visual",
                "ts shows a first-run notice on fresh install: splash + 'Share agent traces with Prime Intellect?' dialog (Share / Not now, /traces hint)",
                evidence=self.sides["ts"].root / flow / "01-launch.txt",
            )
            self.record(
                flow,
                "visual",
                "Rust launches straight into the TUI: no splash ASCII art and no first-run trace-sharing notice",
                evidence=self.sides["rust"].root / flow / "01-launch.txt",
            )
        elif noticed.get("rust") and not noticed.get("ts"):
            self.record(
                flow,
                "visual",
                "rust shows the first-run notice but ts does not (ts agent dir already onboarded?)",
                evidence=self.sides["ts"].root / flow / "01-launch.txt",
            )

    def f2_prompt(self) -> None:
        """Headless print mode: one prompt, one model response, protocol capture."""
        flow = "f2_prompt"
        recs = {}
        for side in (self.sides["ts"], self.sides["rust"]):
            mark = len(side.mock.requests())
            argv = [
                side.binary,
                "-p",
                "--daemon-socket",
                str(side.daemon_socket),
                "--provider",
                "prime-inference",
                "--model",
                "mock-1",
                "--offline",
                "Say hello",
            ]
            rec = B.run_cmd(argv, side.env, side.work_dir, timeout=240)
            recs[side.name] = rec
            side.evidence_json(flow, "cmd.json", rec)
            reqs = self.new_mock_requests(side, mark)
            side.evidence_json(flow, "mock-requests.json", reqs)
            self.copy_sessions(side, flow)
            if rec["timeout"] or rec["exit_code"] != 0:
                self.record(
                    flow,
                    "behavior",
                    f"{side.name}: print-mode run failed (exit={rec['exit_code']}, timeout={rec['timeout']}); stderr: {rec['stderr'][:200]}",
                )
        # stdout comparison
        ts_out = recs["ts"]["stdout"].strip()
        rs_out = recs["rust"]["stdout"].strip()
        if ts_out == rs_out and ts_out:
            self.record(flow, "behavior", f"print-mode stdout identical: {ts_out!r}", gap=False)
        else:
            self.record(
                flow,
                "behavior",
                f"print-mode stdout differs: ts={ts_out!r} vs rust={rs_out!r}",
            )
        self.wire_request_diff(flow, "mock-requests.json")

    def wire_request_diff(self, flow: str, reqfile: str) -> None:
        """Diff the mock request bodies between the two sides."""
        ts_reqs = json.loads((self.sides["ts"].root / flow / reqfile).read_text())
        rs_reqs = json.loads((self.sides["rust"].root / flow / reqfile).read_text())
        if not ts_reqs or not rs_reqs:
            self.record(flow, "protocol", f"missing mock requests: ts={len(ts_reqs)} rust={len(rs_reqs)}")
            return
        def session_request(reqs):
            for req in reqs:
                if req["body"].get("model") == "mock-1":
                    return req["body"]
            return reqs[0]["body"] if reqs else None

        ts_body = session_request(ts_reqs)
        rs_body = session_request(rs_reqs)
        if ts_body is None or rs_body is None:
            self.record(flow, "protocol", f"no session request captured: ts={len(ts_reqs)} rust={len(rs_reqs)}")
            return
        # The post-turn status-line request (B-7) is checked separately in
        # f5 with a settled wait; a timing-sensitive capture here must not
        # flap, so exclude it from the extra-request diff.
        def extra_models(reqs):
            return [
                r["body"].get("model")
                for r in reqs
                if r["body"].get("model") not in ("mock-1", STATUSLINE_MODEL_ID)
            ]

        ts_extra = extra_models(ts_reqs)
        rs_extra = extra_models(rs_reqs)
        if ts_extra != rs_extra:
            self.record(
                flow,
                "protocol",
                f"extra provider requests beyond the session turn differ: ts={ts_extra} rust={rs_extra}",
            )
        ts_keys = set(ts_body)
        rs_keys = set(rs_body)
        if ts_keys != rs_keys:
            self.record(
                flow,
                "protocol",
                f"request body keys differ: ts-only={sorted(ts_keys - rs_keys)} rust-only={sorted(rs_keys - ts_keys)}",
                evidence=self.run_dir / "protocol-request-diff.txt",
            )
        # tools
        ts_tools = [t.get("function", {}).get("name") for t in ts_body.get("tools", [])]
        rs_tools = [t.get("function", {}).get("name") for t in rs_body.get("tools", [])]
        if ts_tools != rs_tools:
            self.record(
                flow,
                "protocol",
                f"model tool surface differs: ts={ts_tools} rust={rs_tools}",
            )
        # messages
        ts_msgs = ts_body.get("messages", [])
        rs_msgs = rs_body.get("messages", [])
        ts_roles = [m.get("role") for m in ts_msgs]
        rs_roles = [m.get("role") for m in rs_msgs]
        if ts_roles != rs_roles:
            self.record(flow, "protocol", f"message roles differ: ts={ts_roles} rust={rs_roles}")
        # harness digest
        def has_digest(msgs):
            for m in msgs:
                content = m.get("content")
                if isinstance(content, str) and "[harness-digest]" in content:
                    return True
                if isinstance(content, list):
                    for block in content:
                        if isinstance(block, dict) and "[harness-digest]" in str(block.get("text", "")):
                            return True
            return False
        if has_digest(ts_msgs) != has_digest(rs_msgs):
            self.record(
                flow,
                "protocol",
                f"harness-digest user message: ts={has_digest(ts_msgs)} rust={has_digest(rs_msgs)}",
            )
        # system prompt comparison (only when both have one)
        ts_sys = [m["content"] for m in ts_msgs if m.get("role") == "system"]
        rs_sys = [m["content"] for m in rs_msgs if m.get("role") == "system"]
        if ts_sys and rs_sys:
            diff_path = self.run_dir / "protocol-request-diff.txt"
            with open(diff_path, "a") as f:
                f.write(f"=== {flow} system prompt ==={NL}")
                f.write("--- TS ---" + NL)
                f.write(ts_sys[0] + NL)
                f.write("--- RUST ---" + NL)
                f.write(rs_sys[0] + NL)
            ts_norm = self.normalize_system_prompt(self.sides["ts"], ts_sys[0])
            rs_norm = self.normalize_system_prompt(self.sides["rust"], rs_sys[0])
            if ts_norm != rs_norm:
                self.record(
                    flow,
                    "protocol",
                    f"system prompt text differs (ts {len(ts_sys[0])} chars vs rust {len(rs_sys[0])} chars; normalized diff in protocol-request-diff.txt)",
                    evidence=diff_path,
                )
            else:
                with open(diff_path, "a") as f:
                    f.write(f"=== {flow} normalized system prompt === identical{NL}")

    def normalize_system_prompt(self, side: B.Side, text: str) -> str:
        """Normalize per-side and per-run values that can never match across
        binaries: the side's run directory (cwd + conversation-log path),
        session UUIDs, skill SKILL.md locations (each binary ships skills in
        its own install/workspace directory), and directory-listing order
        (both products list skills in raw readdir order, which differs
        between the two skills directories)."""
        text = text.replace(str(side.root), "<side-root>")
        text = re.sub(
            r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
            "<session-id>",
            text,
        )
        text = re.sub(r"(<location>)[^<]*(</location>)", r"\1<skills-dir>\2", text)
        # Skill order is raw readdir order on both sides; compare as sets.
        def sort_skill_block(match: "re.Match[str]") -> str:
            header, block = match.group(1), match.group(2)
            entries = sorted(re.findall(r"  <skill>.*?  </skill>", block, re.DOTALL))
            return header + NL.join(entries) + "</available_skills>"

        text = re.sub(
            r"(The following skills provide specialized instructions.*?\n<available_skills>\n)(.*?)</available_skills>",
            sort_skill_block,
            text,
            flags=re.DOTALL,
        )
        text = re.sub(
            r"Installed Python skill modules \(pre-imported\): ([^\n]+)",
            lambda m: "Installed Python skill modules (pre-imported): "
            + ", ".join(sorted(re.findall(r"`([^`]+)`", m.group(0)))),
            text,
        )
        return text

    def f3_tool(self) -> None:
        """Tool call turn: deterministic ipython tool call in both products."""
        flow = "f3_tool"
        tool_responses = [
            {"toolCall": {"name": "ipython", "arguments": {"code": "print('battery-tool-ok')"}}},
            {"text": "tool turn done"},
        ]
        recs = {}
        for side in (self.sides["ts"], self.sides["rust"]):
            side.mock.set_responses(list(tool_responses))
            mark = len(side.mock.requests())
            argv = [
                side.binary,
                "-p",
                "--daemon-socket",
                str(side.daemon_socket),
                "--provider",
                "prime-inference",
                "--model",
                "mock-1",
                "--offline",
                "Run the tool",
            ]
            rec = B.run_cmd(argv, side.env, side.work_dir, timeout=600)
            recs[side.name] = rec
            side.evidence_json(flow, "cmd.json", rec)
            side.evidence_json(flow, "mock-requests.json", self.new_mock_requests(side, mark))
            self.copy_sessions(side, flow)
            sessions = side.root / flow / "sessions"
            body = ""
            for sf in sorted(sessions.glob("*.jsonl")) if sessions.exists() else []:
                body += sf.read_text()
            side.evidence(flow, "session-entries.txt", body)
            tool_ok = "battery-tool-ok" in body or "battery-tool-ok" in rec["stdout"]
            if tool_ok:
                self.record(flow, "behavior", f"{side.name}: ipython tool call executed and output captured", gap=False)
            else:
                self.record(
                    flow,
                    "behavior",
                    f"{side.name}: tool-call turn did not produce the tool output (stdout tail: {rec['stdout'][-200:]!r})",
                )
        # session entry types
        self.session_shape_diff(flow)

    def session_shape_diff(self, flow: str) -> None:
        counts = {}
        for name in ("ts", "rust"):
            side = self.sides[name]
            types = {}
            d = side.root / flow / "sessions"
            for sf in sorted(d.glob("*.jsonl")) if d.exists() else []:
                for line in sf.read_text().splitlines():
                    try:
                        entry = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    t = entry.get("type", "?")
                    types[t] = types.get(t, 0) + 1
            counts[name] = types
        (self.run_dir / f"{flow}-session-shapes.json").write_text(json.dumps(counts, indent=1))
        ts_only = set(counts["ts"]) - set(counts["rust"])
        rs_only = set(counts["rust"]) - set(counts["ts"])
        if ts_only or rs_only:
            self.record(
                flow,
                "protocol",
                f"session entry types differ: ts-only={sorted(ts_only)} rust-only={sorted(rs_only)}; full counts in {flow}-session-shapes.json",
                evidence=self.run_dir / f"{flow}-session-shapes.json",
            )
        else:
            self.record(flow, "protocol", f"session entry type sets match ({sorted(counts['ts'])})", gap=False)

    def f4_commands(self) -> None:
        """Slash command surface: the '/' menu and one benign command run."""
        flow = "f4_commands"
        for side in (self.sides["ts"], self.sides["rust"]):
            session = f"{self.runid}-f4-{side.name}"
            argv = [side.binary, "--daemon-socket", str(side.daemon_socket), "--offline"]
            B.tmux_launch(session, argv, side.env, side.work_dir)
            # TS fresh-install notice is already answered in f1 for these dirs,
            # but print/daemon state may re-show it; press through if present.
            B.tmux_wait_text(session, ">|manage|PRIME Agent", timeout=25)
            B.tmux_send(session, "/", enter=False)
            time.sleep(1.5)
            frame = B.tmux_capture(session)
            side.evidence(flow, "01-slash-menu.txt", frame)
            if "/" in frame and ("commands" in frame.lower() or "model" in frame.lower() or "quit" in frame.lower()):
                self.record(flow, "visual", f"{side.name}: '/' shows a slash-command menu", gap=False)
            else:
                self.record(flow, "visual", f"{side.name}: '/' did not show a command menu", evidence=side.root / flow / "01-slash-menu.txt")
            B.tmux_send(session, "C-c", enter=False)
            time.sleep(0.5)
            # Benign run: /session shows session info in the TS product.
            B.tmux_send(session, "/session")
            frame2 = B.tmux_wait_text(session, "session|Session|cwd|error|Error|unknown|Unknown", timeout=15)
            side.evidence(flow, "02-session-command.txt", frame2)
            B.tmux_kill(session)

    def f5_side_questions(self) -> None:
        """Side questions over the daemon socket on both daemons."""
        flow = "f5_side_questions"
        for side in (self.sides["ts"], self.sides["rust"]):
            side.mock.set_responses([{"text": "side answer from mock"}])
            self.ensure_daemon(side)
            wire = B.Wire(side.daemon_socket)
            side.evidence_json(flow, "hello.json", wire.hello)
            create = wire.request(
                "c1",
                {"type": "create", "name": "battery-side-questions", "config": self.session_config(side)},
                timeout=120,
            )
            side.evidence_json(flow, "create-response.json", create)
            if create.get("success") is not True:
                self.record(flow, "protocol", f"{side.name}: daemon create failed: {json.dumps(create)[:300]}")
                wire.close()
                continue
            session_id = (create.get("data", {}).get("activeSessionId") or create.get("data", {}).get("id") or "")
            # Side-question events stream only to clients attached to the
            # session (the real /btw flow is an attached TUI client), so
            # attach this wire first.
            attach = wire.request(
                "a0", {"type": "attach", "activeSessionId": session_id}, timeout=60
            )
            side.evidence_json(flow, "attach-response.json", attach)
            if attach.get("success") is not True:
                self.record(flow, "protocol", f"{side.name}: attach before side question failed: {json.dumps(attach)[:200]}")
                wire.close()
                continue
            # Prime the session with one turn first (mock reply #1 is fresh).
            side.mock.set_responses([{"text": "main turn reply"}, {"text": "side answer from mock"}])
            prompt = wire.request(
                "p1",
                {"type": "prompt_and_wait", "activeSessionId": session_id, "message": "start a turn"},
                timeout=240,
            )
            side.evidence_json(flow, "prompt-response.json", prompt)
            mark = len(side.mock.requests())
            sq = wire.request(
                "sq1",
                {
                    "type": "start_side_question",
                    "activeSessionId": session_id,
                    "sideQuestionId": "q1",
                    "question": "what is the mock answer?",
                },
                timeout=240,
            )
            side.evidence_json(flow, "side-question-response.json", sq)
            side.evidence_json(flow, "side-question-events.json", wire.events)
            # The side question runs async after the ack; wait for its
            # completion to stream back as side_question_event frames.
            deadline = time.time() + 90
            while time.time() < deadline:
                fresh = wire.drain(timeout=5)
                text = json.dumps(fresh)
                if "complete" in text or "side answer from mock" in text or "cancelled" in text:
                    break
            events = wire.events
            complete = any("complete" in json.dumps(e) for e in events)
            got_side_answer = any("side answer from mock" in json.dumps(e) for e in events)
            if sq.get("success") is True and (complete or got_side_answer):
                self.record(
                    flow,
                    "protocol",
                    f"{side.name}: start_side_question answered via mock with side_question_event stream",
                    gap=False,
                )
            else:
                self.record(
                    flow,
                    "protocol",
                    f"{side.name}: side question outcome differs (success={sq.get('success')}, events={len(events)}, complete={complete})",
                    evidence=side.root / flow / "side-question-response.json",
                )
            # Abort for an unknown id (guard parity).
            abort = wire.request(
                "ab0",
                {"type": "abort_side_question", "activeSessionId": session_id, "sideQuestionId": "nope"},
                timeout=60,
            )
            side.evidence_json(flow, "abort-unknown.json", abort)
            side.evidence_json(flow, "mock-requests.json", self.new_mock_requests(side, mark))
            wire.close()
            # B-7: after a completed turn, the daemon session issues a second
            # provider request for the dashboard status line (a small model).
            # It may fire while the side question runs or up to the settle
            # debounce later; wait out the debounce and filter the whole
            # wire log for it.
            time.sleep(5)
            statusline = [req for req in side.mock.requests() if is_statusline_request(req)]
            side.evidence_json(flow, "statusline-requests.json", statusline)
            self.copy_sessions(side, flow)

        # B-7 differential: both sides must have issued the status-line
        # request to the same small model after the turn.
        status_rows = {}
        for side in (self.sides["ts"], self.sides["rust"]):
            path = side.root / flow / "statusline-requests.json"
            requests = json.loads(path.read_text()) if path.exists() else []
            status_rows[side.name] = len(requests)
        if status_rows["ts"] and status_rows["rust"]:
            self.record(
                flow,
                "protocol",
                f"post-turn status-line request issued by both sides "
                f"(ts={status_rows['ts']}, rust={status_rows['rust']} requests, model {STATUSLINE_MODEL_ID})",
                gap=False,
            )
        else:
            self.record(
                flow,
                "protocol",
                f"post-turn status-line request missing: ts={status_rows['ts']} rust={status_rows['rust']}",
                evidence="statusline-requests.json",
            )

    def ensure_daemon(self, side: B.Side) -> None:
        """A daemon must be listening on the side socket; start one if not."""
        try:
            probe = B.Wire(side.daemon_socket)
            probe.close()
            return
        except (OSError, EOFError):
            pass
        side.start_daemon()

    def f6_attach(self) -> None:
        """Attach: wire-level snapshot + event stream, and the CLI attach command."""
        flow = "f6_attach"
        for side in (self.sides["ts"], self.sides["rust"]):
            self.ensure_daemon(side)
            owner = B.Wire(side.daemon_socket)
            create = owner.request(
                "c1",
                {
                    "type": "create",
                    "name": "battery-attach",
                    "config": self.session_config(side),
                },
                timeout=120,
            )
            side.evidence_json(flow, "create-response.json", create)
            session_id = (create.get("data", {}).get("activeSessionId") or create.get("data", {}).get("id") or "")
            if create.get("success") is not True:
                self.record(flow, "protocol", f"{side.name}: daemon create failed: {json.dumps(create)[:300]}")
                owner.close()
                continue
            attacher = B.Wire(side.daemon_socket)
            attach = attacher.request(
                "a1",
                {"type": "attach", "activeSessionId": session_id},
                timeout=60,
            )
            side.evidence_json(flow, "attach-response.json", attach)
            side.evidence_json(flow, "attach-events.json", attacher.events)
            attach_ok = attach.get("success") is True and "data" in attach
            if attach_ok:
                data_keys = sorted((attach.get("data") or {}).keys())
                side.evidence_json(flow, "attach-data-keys.json", data_keys)
                self.record(flow, "protocol", f"{side.name}: wire attach returned a snapshot (data keys: {data_keys})", gap=False)
            else:
                self.record(
                    flow,
                    "protocol",
                    f"{side.name}: wire attach failed: {json.dumps(attach)[:300]}",
                    evidence=side.root / flow / "attach-response.json",
                )
            # Event stream: prompt from the owner, capture events on the attacher.
            side.mock.set_responses([{"text": "attach stream reply"}])
            mark = len(side.mock.requests())
            prompt = owner.request(
                "p1",
                {"type": "prompt_and_wait", "activeSessionId": session_id, "message": "say something"},
                timeout=240,
            )
            side.evidence_json(flow, "prompt-response.json", prompt)
            # Drain whatever streamed to the attacher during the turn.
            attacher.drain(timeout=5)
            side.evidence_json(flow, "attacher-events-after-prompt.json", attacher.events)
            if attacher.events:
                self.record(
                    flow,
                    "protocol",
                    f"{side.name}: attached client received {len(attacher.events)} events during the turn",
                    gap=False,
                )
            else:
                self.record(flow, "protocol", f"{side.name}: attached client received no events during the turn")
            owner.close()
            attacher.close()
            # CLI-level attach is an interactive surface: run it in tmux.
            attach_session = f"{self.runid}-f6cli-{side.name}"
            argv = [side.binary, "attach", session_id, "--daemon-socket", str(side.daemon_socket)]
            B.tmux_launch(attach_session, argv, side.env, side.work_dir)
            frame = B.tmux_wait_text(attach_session, "attach stream reply|hello|Error|error|>", timeout=25)
            side.evidence(flow, "cli-attach.txt", frame)
            pane_state = B.tmux(
                "list-panes", "-t", attach_session, "-F", "#{pane_dead} #{pane_dead_status}", check=False
            ).stdout.strip()
            B.tmux_kill(attach_session)
            if pane_state.startswith("1"):
                self.record(
                    flow,
                    "behavior",
                    f"{side.name}: CLI 'attach' exited in tmux ({pane_state}); visible frame tail: {[l for l in frame.splitlines() if l.strip()][-3:]}",
                    evidence=side.root / flow / "cli-attach.txt",
                )
            else:
                self.record(
                    flow,
                    "behavior",
                    f"{side.name}: CLI 'attach' opened the session in tmux (frame captured)",
                    gap=False,
                )
            self.copy_sessions(side, flow)

        # f6 cross-side verdict: the attach event stream must carry the same
        # wire events (type + message role / customType), in order.
        from collections import Counter

        def fingerprints(side_name):
            path = self.sides[side_name].root / flow / "attacher-events-after-prompt.json"
            if not path.exists():
                return None
            out = []
            for entry in json.loads(path.read_text()):
                event = entry.get("event", entry)
                message = event.get("message") or {}
                etype = event.get("type")
                # Row scope: the event-set parity locked here excludes the
                # two documented model-surface diffs (see PORTING-NOTES and
                # the f6 commit): the harness-digest custom message pair
                # (TS delivers the per-turn digest as a custom message;
                # Rust composes it into the request only) and the
                # turn_end/agent_end payloads (TS carries the final message).
                # Drop the filter when the model-surface lane lands
                # custom-message wire parity.
                if (
                    etype in ("message_start", "message_end")
                    and message.get("customType") == "harness_digest"
                ):
                    continue
                if etype in ("turn_end", "agent_end"):
                    out.append(str(etype))
                    continue
                out.append(
                    "{}:{}".format(
                        etype,
                        message.get("role") or message.get("customType") or "",
                    )
                )
            return out

        ts_events = fingerprints("ts")
        rust_events = fingerprints("rust")
        if ts_events and rust_events:
            if ts_events == rust_events:
                self.record(
                    flow,
                    "protocol",
                    f"attach event sequences match ({len(ts_events)} projected events, in order; "
                    "harness-digest custom pairs and turn_end/agent_end payloads are out of scope here — "
                    "documented model-surface diffs, see PORTING-NOTES)",
                    gap=False,
                )
            else:
                ts_counts = Counter(ts_events)
                rust_counts = Counter(rust_events)
                ts_only = ts_counts - rust_counts
                rust_only = rust_counts - ts_counts
                self.record(
                    flow,
                    "protocol",
                    "attach event sequences differ: "
                    f"ts={len(ts_events)} rust={len(rust_events)}; "
                    f"ts-only={sorted(ts_only.elements())} rust-only={sorted(rust_only.elements())}",
                    evidence=self.run_dir / flow,
                )

    def session_config(self, side: B.Side) -> dict:
        # Identical on both sides: explicit provider/model flags ride the
        # create config over the wire and are authoritative in the worker
        # (B-1 differential check; no env-based model workaround).
        return {
            "cwd": str(side.work_dir),
            "sessionDir": str(side.agent_dir / "sessions"),
            "provider": "prime-inference",
            "model": "mock-1",
            "executionMode": "print",
        }

    def f7_compaction(self) -> None:
        """Compaction: what the TS daemon does on 'compact'; what Rust does."""
        flow = "f7_compaction"
        for side in (self.sides["ts"], self.sides["rust"]):
            self.ensure_daemon(side)
            wire = B.Wire(side.daemon_socket)
            create = wire.request(
                "c1",
                {
                    "type": "create",
                    "name": "battery-compact",
                    "config": self.session_config(side),
                },
                timeout=120,
            )
            side.evidence_json(flow, "create-response.json", create)
            session_id = (create.get("data", {}).get("activeSessionId") or create.get("data", {}).get("id") or "")
            if create.get("success") is not True:
                self.record(flow, "protocol", f"{side.name}: daemon create failed: {json.dumps(create)[:300]}")
                wire.close()
                continue
            # Grow the session before compacting: several turns of
            # deterministic content, so the TS compactor keeps the recent
            # turns and has older ones to summarize.
            turn_text = "Turn {n} of the parity battery. " * 800
            side.mock.set_responses([{"text": f"pre-compaction reply {i}"} for i in range(7)])
            for turn_index in range(1, 7):
                prompt = wire.request(
                    f"p{turn_index}",
                    {
                        "type": "prompt_and_wait",
                        "activeSessionId": session_id,
                        "message": turn_text.format(n=turn_index),
                    },
                    timeout=240,
                )
                side.evidence_json(flow, f"prompt-{turn_index}-response.json", prompt)
            compact = wire.request(
                "k1",
                {"type": "compact", "activeSessionId": session_id},
                timeout=240,
            )
            side.evidence_json(flow, "compact-response.json", compact)
            if compact.get("success") is True:
                self.record(
                    flow,
                    "protocol",
                    f"{side.name}: daemon 'compact' succeeded: {json.dumps(compact.get('data', {}))[:200]}",
                    gap=False,
                )
            else:
                self.record(
                    flow,
                    "protocol",
                    f"{side.name}: daemon 'compact' failed: {json.dumps(compact)[:300]}",
                    evidence=side.root / flow / "compact-response.json",
                )
            wire.close()
            self.copy_sessions(side, flow)

    def f8_resume(self) -> None:
        """Exit + resume: headless session persisted, then continued in both."""
        flow = "f8_resume"
        recs = {}
        for side in (self.sides["ts"], self.sides["rust"]):
            side.mock.set_responses([{"text": "resumed turn reply"}])
            mark = len(side.mock.requests())
            rec = B.run_cmd(
                [
                    side.binary,
                    "-p",
                    "--daemon-socket",
                    str(side.daemon_socket),
                    "--provider",
                    "prime-inference",
                    "--model",
                    "mock-1",
                    "--offline",
                    "-c",
                    "Continue this session",
                ],
                side.env,
                side.work_dir,
                timeout=240,
            )
            recs[side.name] = rec
            side.evidence_json(flow, "continue-cmd.json", rec)
            side.evidence_json(flow, "mock-requests.json", self.new_mock_requests(side, mark))
            self.copy_sessions(side, flow)

        # B-11 differential: both sides must refuse to continue a session
        # that is already active in the daemon, with the same message shape
        # (the TS print path fails the daemon create with
        # SessionAlreadyActiveError; the Rust print path guards identically).
        def refused(rec):
            return (
                rec["exit_code"] == 1
                and "Session is already active in " in rec["stdout"] + rec["stderr"]
            )

        def refusal_id(rec):
            match = re.search(
                r"Session is already active in ([0-9a-f]+):", rec["stdout"] + rec["stderr"]
            )
            return match.group(1) if match else None

        if refused(recs["ts"]) and refused(recs["rust"]):
            self.record(
                flow,
                "behavior",
                f"print '-c' refuses a session active in the daemon on both sides: "
                f"{refusal_id(recs['ts'])} (ts) vs {refusal_id(recs['rust'])} (rust)",
                gap=False,
            )
        else:
            for name, rec in recs.items():
                if refused(rec):
                    continue
                detail = (rec["stdout"] + rec["stderr"]).strip()[:200]
                self.record(
                    flow,
                    "behavior",
                    f"{name}: print '-c' did not refuse the active session (exit={rec['exit_code']}): {detail}",
                    evidence=f"{name}/{flow}/continue-cmd.json",
                )

        # Interactive resume of the same session.
        for side in (self.sides["ts"], self.sides["rust"]):
            files = side.session_files()
            if files:
                session = f"{self.runid}-f8-{side.name}"
                argv = [
                    side.binary,
                    "--daemon-socket",
                    str(side.daemon_socket),
                    "--resume",
                    str(files[-1]),
                    "--offline",
                ]
                B.tmux_launch(session, argv, side.env, side.work_dir)
                frame = B.tmux_wait_text(session, "resumed turn reply|battery hello|>", timeout=30)
                side.evidence(flow, "resume-frame.txt", frame)
                B.tmux_kill(session)
        self.session_shape_diff(flow)

    def f9_agents_view(self) -> None:
        """Agents view: interactive session list pane states."""
        flow = "f9_agents_view"
        for side in (self.sides["ts"], self.sides["rust"]):
            session = f"{self.runid}-f9-{side.name}"
            argv = [side.binary, "agents", "--daemon-socket", str(side.daemon_socket), "--offline"]
            B.tmux_launch(session, argv, side.env, side.work_dir)
            frame = B.tmux_wait_text(session, "agents|Agents|session|Session|No |error|Error", timeout=30)
            side.evidence(flow, "agents-view.txt", frame)
            B.tmux_kill(session)
            self.record(
                flow,
                "visual",
                f"{side.name}: agents view frame captured (see evidence); frame-level diffing belongs to the visual-parity lane",
                gap=False,
            )

    # -- perf -----------------------------------------------------------------

    def f11_provider_failure(self) -> None:
        """Kill the mock provider mid-session: the interactive transcript
        must surface the provider failure (retry banner + error row(s))
        exactly like the TS product, and the earlier exchange stays
        rendered exactly once."""
        flow = "f11_provider_failure"
        # Bounded, fast retries so the flow settles in seconds instead of
        # minutes. The provider recovery wait (TS retry.provider.waitForUsage)
        # is disabled on both sides so exhaustion surfaces instead of
        # pinging a dead provider for up to 15 minutes.
        retry_settings = {
            "retry": {
                "enabled": True,
                "maxRetries": 2,
                "baseDelayMs": 200,
                "provider": {"waitForUsage": {"enabled": False}},
            }
        }
        for side in (self.sides["ts"], self.sides["rust"]):
            session = f"{self.runid}-f11-{side.name}"
            settings_path = side.agent_dir / "settings.json"
            prior_settings = (
                settings_path.read_text() if settings_path.exists() else None
            )
            settings_path.write_text(json.dumps(retry_settings))
            try:
                argv = [
                    side.binary,
                    "--daemon-socket",
                    str(side.daemon_socket),
                    "--provider",
                    "prime-inference",
                    "--model",
                    "mock-1",
                    "--offline",
                ]
                B.tmux_launch(session, argv, side.env, side.work_dir)
                frame = ""
                deadline = time.time() + 30
                while time.time() < deadline:
                    frame = B.tmux_capture(session)
                    if "Share agent traces" in frame:
                        break
                    time.sleep(1.0)
                if "Share agent traces" in frame:
                    B.tmux_send(session, "Down")
                    time.sleep(0.5)
                    B.tmux_send(session, "Enter")
                    time.sleep(2.0)
                # A settled main screen before the exchange.
                stable = False
                deadline = time.time() + 30
                while time.time() < deadline and not stable:
                    first = B.tmux_capture(session)
                    time.sleep(2.0)
                    second = B.tmux_capture(session)
                    stable = first == second and ("manage" in first or ">" in first)
                # A healthy exchange first: the reply must render. Own the
                # mock script for this flow — the response queue is shared
                # with every earlier flow in the same battery process, so a
                # leftover response would break the HELLO_TEXT assertion.
                side.mock.set_responses([{"text": HELLO_TEXT}])
                B.tmux_send(session, "hello")
                healthy = B.tmux_wait_text(session, HELLO_TEXT, timeout=90)
                side.evidence(flow, "01-healthy-exchange.txt", healthy)
                # Kill the provider mid-session, then prompt again.
                side.mock.stop()
                B.tmux_send(session, "again")
                settled = ""
                deadline = time.time() + 120
                while time.time() < deadline:
                    settled = B.tmux_capture(session)
                    if "Retry failed after" in settled:
                        # Let the final frame settle (retry banner + rows).
                        time.sleep(1.0)
                        settled = B.tmux_capture(session)
                        break
                    time.sleep(1.0)
                side.evidence(flow, "02-provider-failure.txt", settled)
                retry_banner = "Retry failed after" in settled
                error_rows = len(re.findall(r"Error: ", settled))
                hello_rows = settled.count(HELLO_TEXT)
                side.evidence_json(
                    flow,
                    "verdict.json",
                    {
                        "retryBanner": retry_banner,
                        "errorRows": error_rows,
                        "helloRenders": hello_rows,
                    },
                )
                if not retry_banner or error_rows == 0:
                    self.record(
                        flow,
                        "behavior",
                        f"{side.name}: provider failure is silent in the interactive transcript (retry banner: {retry_banner}, error rows: {error_rows})",
                        evidence=side.root / flow / "02-provider-failure.txt",
                    )
                else:
                    self.record(
                        flow,
                        "behavior",
                        f"{side.name}: provider failure surfaces (retry banner + {error_rows} error row(s))",
                        gap=False,
                    )
                if hello_rows != 1:
                    self.record(
                        flow,
                        "behavior",
                        f"{side.name}: the healthy exchange renders {hello_rows} times (expected exactly 1)",
                        evidence=side.root / flow / "02-provider-failure.txt",
                    )
            finally:
                B.tmux_kill(session)
                if prior_settings is None:
                    settings_path.unlink(missing_ok=True)
                else:
                    settings_path.write_text(prior_settings)
        # Cross-side parity: same number of failed-attempt error rows, same
        # single render of the healthy exchange.
        verdicts = {
            name: json.loads(
                (self.sides[name].root / flow / "verdict.json").read_text()
            )
            for name in ("ts", "rust")
            if (self.sides[name].root / flow / "verdict.json").exists()
        }
        if len(verdicts) == 2:
            if verdicts["ts"]["errorRows"] != verdicts["rust"]["errorRows"]:
                self.record(
                    flow,
                    "visual",
                    f"failed-attempt error rows differ: ts={verdicts['ts']['errorRows']} rust={verdicts['rust']['errorRows']}",
                    evidence=self.run_dir / flow,
                )
            else:
                self.record(
                    flow,
                    "visual",
                    f"provider-failure rendering parity: {verdicts['rust']['errorRows']} error row(s) on both sides",
                    gap=False,
                )

    def perf_onboard(self, side: B.Side) -> None:
        """Settle first-run dialogs (the TS trace notice) once before measuring,
        so measured launches settle straight into the main screen."""
        socket = side.root / "perf" / "onboard.sock"
        socket.parent.mkdir(parents=True, exist_ok=True)
        if socket.exists():
            socket.unlink()
        session = f"{self.runid}-perf-onboard-{side.name}"
        argv = [side.binary, "--daemon-socket", str(socket), "--offline"]
        B.tmux_launch(session, argv, side.env, side.work_dir)
        deadline = time.time() + 30
        while time.time() < deadline:
            frame = B.tmux_capture(session)
            if "Share agent traces" in frame:
                B.tmux_send(session, "Down")
                time.sleep(0.5)
                B.tmux_send(session, "Enter")
                time.sleep(1.0)
            elif P.is_ready(side.name, frame):
                break
            else:
                time.sleep(0.5)
        deadline = time.time() + 30
        while time.time() < deadline:
            if P.is_ready(side.name, B.tmux_capture(session)):
                break
            time.sleep(0.5)
        B.tmux_kill(session)
        P.stop_perf_daemon(socket)

    def f10_perf(self) -> None:
        """PERF row: cold startup + keystroke-to-render latency, Rust vs TS."""
        flow = "f10_perf"
        measurements: dict[str, dict] = {}
        for side in (self.sides["ts"], self.sides["rust"]):
            self.perf_onboard(side)
            binary_path = shutil.which(side.binary) or side.binary
            binary_bytes = None
            if Path(binary_path).is_file():
                binary_bytes = os.path.getsize(binary_path)
            measurements[side.name] = {
                "binary": side.binary,
                "binary_bytes": binary_bytes,
                "ready_s": [],
                "first_frame_s": [],
                "typing_ms": [],
            }
        # Interleave the measured launches (ts run i, rust run i, ts run i+1,
        # ...) so background load on the box hits both sides evenly; a
        # per-side block would let one side measure during a build and bias
        # the differential.
        for i in range(PERF_RUNS):
            for side in (self.sides["ts"], self.sides["rust"]):
                socket = side.root / "perf" / f"sock-{i}.sock"
                rec = P.measure_launch(
                    side, f"{self.runid}-perf-{side.name}-{i}", socket
                )
                side.evidence_json(flow, f"launch-{i}.json", rec)
                P.stop_perf_daemon(socket)
                if rec["ready_s"] is None:
                    self.record(
                        flow,
                        "perf",
                        f"{side.name}: measured launch {i} never reached an interactive-ready frame within 60s",
                        evidence=side.root / flow / f"launch-{i}.json",
                    )
                    continue
                bucket = measurements[side.name]
                bucket["ready_s"].append(rec["ready_s"])
                bucket["first_frame_s"].append(rec["first_frame_s"] or rec["ready_s"])
                bucket["typing_ms"].extend(rec["typing_ms"])
        for side in (self.sides["ts"], self.sides["rust"]):
            bucket = measurements[side.name]
            side.evidence_json(flow, "summary.json", bucket)
            ready = P.median(bucket["ready_s"])
            typing = P.summarize(bucket["typing_ms"])
            if ready is not None:
                self.record(
                    flow,
                    "perf",
                    f"{side.name}: cold startup to interactive-ready median {ready:.3f}s "
                    f"over {len(bucket['ready_s'])} launches (first frame median {P.median(bucket['first_frame_s']):.3f}s); "
                    f"typing latency median {typing['median']}ms, p95 {typing['p95']}ms "
                    f"over {typing['n']} keystrokes",
                    gap=False,
                )
        ts = measurements["ts"]
        rs = measurements["rust"]
        # Startup threshold: rust median cold startup vs the TS binary.
        ts_ready = P.median(ts["ready_s"])
        rs_ready = P.median(rs["ready_s"])
        if ts_ready is not None and rs_ready is not None:
            ratio = rs_ready / ts_ready
            detail = (
                f"startup: rust {rs_ready:.3f}s vs ts {ts_ready:.3f}s cold-ready median "
                f"(ratio {ratio:.2f}, threshold {PERF_STARTUP_MAX_RATIO})"
            )
            if ratio <= PERF_STARTUP_MAX_RATIO:
                self.record(flow, "perf", detail, gap=False)
            else:
                self.record(flow, "perf", f"REGRESSION {detail}", evidence=rs["binary"])
        else:
            self.record(flow, "perf", "startup threshold not evaluable: a side never reached ready", gap=True)
        # Typing threshold: rust p95 keystroke latency vs the TS binary.
        ts_p95 = P.summarize(ts["typing_ms"]).get("p95")
        rs_p95 = P.summarize(rs["typing_ms"]).get("p95")
        if ts_p95 is not None and rs_p95 is not None:
            ratio = rs_p95 / ts_p95
            detail = (
                f"typing: rust p95 {rs_p95}ms vs ts p95 {ts_p95}ms keystroke-to-render "
                f"(ratio {ratio:.2f}, threshold {PERF_TYPING_MAX_RATIO})"
            )
            if ratio <= PERF_TYPING_MAX_RATIO:
                self.record(flow, "perf", detail, gap=False)
            else:
                self.record(flow, "perf", f"REGRESSION {detail}", evidence=rs["binary"])
        else:
            self.record(flow, "perf", "typing threshold not evaluable: a side produced no keystroke samples", gap=True)
        # Release-build note: the perf row is only meaningful against a
        # release build; a debug binary is a finding, not a baseline. The
        # workspace release profile keeps line-tables-only debug info
        # (~103MB); a debug build of the same tree measures ~290MB, so the
        # size threshold sits between them.
        size_mb = (rs["binary_bytes"] or 0) / 1_000_000
        if "/debug/" in rs["binary"] or (rs["binary_bytes"] or 0) > 150_000_000:
            self.record(
                flow,
                "perf",
                f"rust binary measured is a debug build ({rs['binary']}, {size_mb:.0f}MB); "
                "the perf posture is cargo build --release",
                evidence=rs["binary"],
            )
        else:
            self.record(
                flow,
                "perf",
                f"rust binary measured: {rs['binary']} ({size_mb:.1f}MB, release posture)",
                gap=False,
            )

    # -- report ---------------------------------------------------------------

    def write_report(self) -> Path:
        path = self.run_dir / "report.md"
        lines = [f"# Parity battery run {self.stamp}", ""]
        # Battery greenness is scoped: it proves only the scripted flows
        # below, never overall product parity (docs/completion-matrix.md).
        lines.append(
            "Note: 0 gaps below covers only these scripted flows; it is not a product-parity verdict (docs/completion-matrix.md)."
        )
        lines.append(f"- ts binary: {self.ts_bin}")
        lines.append(f"- rust binary: {self.rust_bin}")
        lines.append(f"- flows: {', '.join(self.flows)}")
        lines.append("")
        lines.append("## Findings")
        lines.append("")
        gaps = [f for f in self.findings if f["gap"]]
        oks = [f for f in self.findings if not f["gap"]]
        lines.append(f"{len(gaps)} gaps, {len(oks)} parity checks passed.")
        lines.append("")
        current = None
        for finding in self.findings:
            if not finding["gap"]:
                continue
            if finding["flow"] != current:
                current = finding["flow"]
                lines.append(f"### {current}")
                lines.append("")
            ev = f" — evidence: {finding['evidence']}" if finding.get("evidence") else ""
            lines.append(f"- [{finding['category']}] {finding['summary']}{ev}")
        lines.append("")
        lines.append("## Passed checks")
        lines.append("")
        for finding in oks:
            lines.append(f"- [{finding['flow']}/{finding['category']}] {finding['summary']}")
        lines.append("")
        path.write_text(NL.join(lines))
        (self.run_dir / "findings.json").write_text(json.dumps(self.findings, indent=1))
        return path

    def run(self) -> int:
        print(f"battery run dir: {self.run_dir}")
        self.make_side("ts", self.ts_bin)
        self.make_side("rust", self.rust_bin)
        order = {f: getattr(self, f) for f in ALL_FLOWS}
        try:
            for flow in self.flows:
                print(f"== {flow}", flush=True)
                order[flow]()
        finally:
            self.kill_tmux_sessions()
            self.stop()
        report = self.write_report()
        gaps = sum(1 for f in self.findings if f["gap"])
        print(NL + report.read_text()[:4000])
        print(f"report: {report}")
        return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    here = Path(__file__).resolve().parent
    repo = here.parent.parent
    parser.add_argument("--flows", default=",".join(ALL_FLOWS))
    parser.add_argument("--ts-bin", default="prime-agent")
    parser.add_argument("--rust-bin", default=str(repo / "target" / "release" / "prime-agent"))
    parser.add_argument("--runs-root", default=str(here / "runs"))
    args = parser.parse_args()
    flows = [f.strip() for f in args.flows.split(",") if f.strip()]
    for flow in flows:
        if flow not in ALL_FLOWS:
            parser.error(f"unknown flow {flow}; valid: {ALL_FLOWS}")
    battery = Battery(Path(args.runs_root), args.ts_bin, args.rust_bin, flows)
    return battery.run()


if __name__ == "__main__":
    raise SystemExit(main())
