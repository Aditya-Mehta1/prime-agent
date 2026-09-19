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
import signal
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import batterylib as B  # noqa: E402
import perf as P  # noqa: E402

NL = chr(10)

ALL_FLOWS = ["f1_launch", "f2_prompt", "f3_tool", "f4_commands", "f5_side_questions", "f6_attach", "f7_compaction", "f8_resume", "f9_agents_view", "f10_perf", "f11_provider_failure", "f12_scroll", "f13_ctrlc_exit", "f14_compact", "f15_a2a", "f16_refine", "f17_slash_model", "f18_goal_autonomous", "f19_heartbeat", "f20_subagents", "f21_worker_recovery"]

# Real-surface flows (f14-f21): each drives one product surface end to end
# (the daemon session + the attached interactive TUI), captures the frame
# at the key moment, and byte-diffs TS vs Rust after normalization. A flow
# whose diff fails on a surface known to be missing in the Rust build
# records its finding with the owning fix lane: the report lists it as
# EXPECTED-FAIL (the evidence the lane needs), not an unexplained gap.
FLOW_LANES = {
    "f14_compact": "compact-fb-2",
    "f15_a2a": "decorations-3",
    "f16_refine": "decorations-3",
    "f17_slash_model": "model-picker-1",
    "f18_goal_autonomous": "goal-autonomous",
    "f19_heartbeat": "heartbeat-tui",
    "f20_subagents": "subagents-tui",
    "f21_worker_recovery": "worker-recovery",
}

# Heavy flows: opt-in by name (`--flows f12_scale_resume`) plus
# PA_BATTERY_HEAVY=1; they measure transcript-scale resume, not parity, and
# would inflate a normal battery run's wall time.
HEAVY_FLOWS = ["f12_scale_resume"]

# PERF row thresholds, measured on this box by scripts/battery/perf.py:
# the invariant is that the Rust binary is never materially slower than
# the TS binary on cold startup or keystroke-to-render latency.
PERF_STARTUP_MAX_RATIO = 1.5
PERF_TYPING_MAX_RATIO = 1.5
PERF_RUNS = 3

# f12 thresholds: a transcript-scale interactive resume (snapshot ingest +
# first full layout) must be scale-ready — seconds, not minutes. The TS
# binary resumes the same corpus in ~3.4s; the Rust gate is an absolute cap
# (the box's background load makes tight differential ratios noisy) plus a
# regression ratio against the TS side measured in the same run.
SCALE_RESUME_MAX_READY_S = 30.0
SCALE_RESUME_MAX_RATIO = 2.0

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
        # The Rust binary resolves the kernel runtime sidecar through
        # PI_PACKAGE_DIR: a packaged layout ships it next to the exe, but a
        # cargo/sandbox build bakes the BUILD machine's source-checkout path
        # in, which does not exist when the binary runs on this box (run
        # 20260919T002527Z: every kernel ipython cell died with "kernel
        # startup failed ... uv pip install prime-agent-runtime ... exit
        # code 1"). Point it at this checkout, which ships
        # prime-agent-runtime/ (the same layout the packaged product uses).
        if name == "rust":
            side.env["PI_PACKAGE_DIR"] = str(Path(__file__).resolve().parents[2])
        # Point both products at the mock through a provider whose API-key
        # resolution both support: both read the models.json apiKey (the
        # env key stays as the fallback both products share).
        side.env["PRIME_API_KEY"] = "sk-battery"
        side.write_models_json()
        self.sides[name] = side
        return side

    def record(self, flow: str, category: str, summary: str, evidence="", gap: bool = True, lane: str | None = None) -> None:
        if isinstance(evidence, Path):
            evidence = str(evidence.relative_to(self.run_dir))
        self.findings.append(
            {
                "flow": flow,
                "category": category,
                "gap": gap,
                "summary": summary,
                "evidence": evidence,
                "expectedFail": lane,
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
            argv = P.launch_argv(side, side.daemon_socket)
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
        # system prompt comparison. TS-prompt parity is SUPERSEDED for the
        # system prompt (roadmap item 3: the Rust product adopts the layered
        # prompt redesign - cached static layers + dynamic tail - as its
        # native prompt). The battery now checks the Rust prompt against the
        # layered shape instead of the TS text, and keeps the raw TS prompt
        # in the diff file as reference evidence.
        ts_sys = [m["content"] for m in ts_msgs if m.get("role") == "system"]
        rs_sys = [m["content"] for m in rs_msgs if m.get("role") == "system"]
        if rs_sys:
            diff_path = self.run_dir / "protocol-request-diff.txt"
            with open(diff_path, "a") as f:
                f.write(f"=== {flow} system prompt ==={NL}")
                if ts_sys:
                    f.write("--- TS ---" + NL)
                    f.write(ts_sys[0] + NL)
                f.write("--- RUST ---" + NL)
                f.write(rs_sys[0] + NL)
            shape = self.layered_prompt_shape(rs_sys[0])
            if shape:
                self.record(
                    flow,
                    "protocol",
                    f"system prompt: rust layered redesign (cached static layers + dynamic tail; TS-prompt parity superseded; raw prompts in protocol-request-diff.txt)",
                    gap=False,
                )
            else:
                self.record(
                    flow,
                    "protocol",
                    f"system prompt: rust prompt does not match the layered shape (expected '# prime-agent harness' static core, mandatory-rules layer, skills inventory, dynamic tail; raw prompts in protocol-request-diff.txt)",
                    evidence=diff_path,
                )
        elif ts_sys and not rs_sys:
            self.record(flow, "protocol", "system prompt: rust request has no system message but ts does")

    def layered_prompt_shape(self, text: str) -> bool:
        """Whether the Rust system prompt matches the layered redesign:
        the static core layer first, the mandatory usage layer, the
        opinionated layer, then the dynamic tail (packages, skills
        inventory, environment). Marker-based, so layer text edits do not
        flap the battery; content-level pinning lives in the pa-core golden
        snapshot test."""
        markers = [
            "# prime-agent harness",
            "The following are mandatory rules",
            "The following guidelines to agents have been shown",
            "Pre-installed Python packages:",
            "<available_skills>",
            "Working directory:",
            "Recursive agent depth: 0 (root)",
        ]
        at = -1
        for marker in markers:
            found = text.find(marker, at + 1)
            if found == -1:
                return False
            at = found
        return True

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
        """Agents view: section grouping over a scripted roster (running live
        session, idle live session, saved-catalog session), the open action
        attaching to the selected running session, and frame captures at two
        terminal sizes for the cross-side frame diff."""
        flow = "f9_agents_view"
        frames: dict[str, dict[str, str]] = {}
        for side in (self.sides["ts"], self.sides["rust"]):
            self.ensure_daemon(side)
            # The agents view lists the whole daemon universe, so f9 starts
            # from a clean one: restart the side daemon and wipe its session
            # files, leaving only the sessions f9 creates below.
            self.reset_side_sessions(side)
            frames[side.name] = {}
            # Idle live session: one completed exchange.
            side.mock.set_responses([{"text": "f9 idle reply"}])
            wire = B.Wire(side.daemon_socket)
            create = wire.request(
                "c9i",
                {"type": "create", "name": "battery-f9-idle", "config": self.session_config(side)},
                timeout=120,
            )
            if create.get("success") is not True:
                self.record(flow, "protocol", f"{side.name}: daemon create failed: {json.dumps(create)[:300]}")
                wire.close()
                continue
            idle_id = (create.get("data", {}).get("activeSessionId") or create.get("data", {}).get("id") or "")
            idle_prompt = wire.request(
                "p9i",
                {"type": "prompt_and_wait", "activeSessionId": idle_id, "message": "f9 idle prompt"},
                timeout=240,
            )
            side.evidence_json(flow, "idle-prompt-response.json", idle_prompt)
            wire.close()
            # A saved-catalog session: a headless print run whose file
            # remains after the run (the Inactive section source).
            print_rec = B.run_cmd(
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
                    "f9 inactive prompt",
                ],
                side.env,
                side.work_dir,
                timeout=240,
            )
            side.evidence_json(flow, "inactive-print.json", print_rec)
            # Busy live session: an in-flight delayed turn keeps the row
            # Running while the view frames are captured. The idle turn's
            # post-turn requests must settle first so the delayed script
            # can only ever apply to the busy session's requests.
            self.settle_mock(side)
            side.mock.set_responses([{"text": "f9 busy reply", "delayMs": 25000}])
            wire = B.Wire(side.daemon_socket)
            create = wire.request(
                "c9b",
                {"type": "create", "name": "battery-f9-busy", "config": self.session_config(side)},
                timeout=120,
            )
            if create.get("success") is not True:
                self.record(flow, "protocol", f"{side.name}: busy create failed: {json.dumps(create)[:300]}")
                wire.close()
                continue
            busy_id = (create.get("data", {}).get("activeSessionId") or create.get("data", {}).get("id") or "")
            wire.send_command("p9b", {"type": "prompt_and_wait", "activeSessionId": busy_id, "message": "f9 busy prompt"})
            wire.close()
            if not self.wait_roster_status(side, busy_id, "running", timeout=15):
                self.record(
                    flow,
                    "behavior",
                    f"{side.name}: the busy session never reached Running on the roster",
                )
                continue
            # TS suppresses the agents view while onboarding is pending
            # (`shouldOpenAgentsViewForDaemonInteractive`); mark the side
            # onboarded so both products open the view directly.
            settings_path = side.agent_dir / "settings.json"
            settings = json.loads(settings_path.read_text()) if settings_path.exists() else {}
            settings["onboardingShown"] = True
            settings_path.write_text(json.dumps(settings))
            # The view over the scripted roster, captured at both sizes.
            view_session = f"{self.runid}-f9-{side.name}"
            # No --offline: it disables telemetry for the invocation, and TS
            # refuses to attach an active agent across that telemetry mismatch.
            argv = [side.binary, "agents", "--daemon-socket", str(side.daemon_socket)]
            B.tmux_launch(view_session, argv, side.env, side.work_dir)
            frame120 = B.tmux_wait_text(view_session, "Running \(|Idle \(|Inactive \(|No sessions", timeout=30)
            side.evidence(flow, "01-agents-view-120x36.txt", frame120)
            frames[side.name]["120x36"] = frame120
            B.tmux_resize(view_session, (220, 50))
            time.sleep(1.0)
            frame220 = B.tmux_capture(view_session)
            side.evidence(flow, "02-agents-view-220x50.txt", frame220)
            frames[side.name]["220x50"] = frame220
            B.tmux_resize(view_session, (120, 36))
            time.sleep(0.5)
            # Section grouping: each scripted session sits under its section.
            sections = self.parse_agents_view_sections(frame120)
            section_ok = True
            for section, marker in (
                ("Running", "battery-f9-busy"),
                ("Idle", "battery-f9-idle"),
                ("Inactive", "f9 inactive prompt"),
            ):
                rows = sections.get(section, [])
                if not any(marker in row for row in rows):
                    section_ok = False
                    self.record(
                        flow,
                        "behavior",
                        f"{side.name}: '{marker}' is not grouped under the {section} section (sections: {json.dumps(sections)[:400]})",
                        evidence=side.root / flow / "01-agents-view-120x36.txt",
                    )
            if section_ok:
                self.record(
                    flow,
                    "behavior",
                    f"{side.name}: agents view groups the roster into Running/Idle/Inactive sections with the scripted sessions in place",
                    gap=False,
                )
            # Attach target: search down to the running row, open it, and the
            # in-flight reply must render in the attached session UI.
            B.tmux_send(view_session, "busy")
            time.sleep(1.0)
            B.tmux_send(view_session, "Enter", enter=False)
            attached = B.tmux_wait_text(view_session, "f9 busy reply", timeout=60)
            side.evidence(flow, "03-attached-session.txt", attached)
            pane_state = B.tmux(
                "list-panes", "-t", view_session, "-F", "#{pane_dead} #{pane_dead_status}", check=False
            ).stdout.strip()
            if pane_state.startswith("1"):
                self.record(
                    flow,
                    "behavior",
                    f"{side.name}: opening the Running row exited the view instead of attaching ({pane_state})",
                    evidence=side.root / flow / "03-attached-session.txt",
                )
            else:
                self.record(
                    flow,
                    "behavior",
                    f"{side.name}: opening the Running row attached to the live session and its in-flight reply rendered",
                    gap=False,
                )
            B.tmux_kill(view_session)
            # Leave the mock script plain for the flows that follow (f11's
            # healthy exchange expects the HELLO_TEXT response).
            side.mock.set_responses([{"text": HELLO_TEXT}])
        # Frame diff: same scripted roster, same size, TS vs Rust.
        for size_label in ("120x36", "220x50"):
            ts_frame = frames.get("ts", {}).get(size_label)
            rs_frame = frames.get("rust", {}).get(size_label)
            if ts_frame is None or rs_frame is None:
                self.record(flow, "visual", f"missing frames for the {size_label} diff (ts={'y' if ts_frame else 'n'} rust={'y' if rs_frame else 'n'})")
                continue
            ts_norm = self.normalize_agents_view_frame(ts_frame, self.sides["ts"])
            rs_norm = self.normalize_agents_view_frame(rs_frame, self.sides["rust"])
            if ts_norm == rs_norm:
                self.record(
                    flow,
                    "visual",
                    f"agents view frames identical at {size_label} (normalized: paths, ids, ages)",
                    gap=False,
                )
            else:
                diff_path = self.sides["ts"].root / flow / f"frame-diff-{size_label}.txt"
                diff_path.parent.mkdir(parents=True, exist_ok=True)
                diff_path.write_text(
                    f"--- ts ({size_label})\n{ts_frame}\n+++ rust ({size_label})\n{rs_frame}"
                )
                self.record(
                    flow,
                    "visual",
                    f"agents view frames differ at {size_label} (see frame-diff-{size_label}.txt)",
                    evidence=diff_path,
                )

    def parse_agents_view_sections(self, frame: str) -> dict[str, list[str]]:
        """Map each rendered section heading to its row lines."""
        sections: dict[str, list[str]] = {}
        current = None
        for line in frame.splitlines():
            match = re.match(r"\s*(Running|Idle|Inactive) \(\d+\)", line)
            if match:
                current = match.group(1)
                sections.setdefault(current, [])
                continue
            if current is not None and line.strip():
                sections[current].append(line)
        return sections

    def normalize_agents_view_frame(self, frame: str, side: B.Side) -> str:
        """Erase side-varying text (paths, ids, ages, versions) so equal
        layout and content compare equal."""
        text = frame.rstrip("\n")
        # The splash ~-compresses paths; expand so the absolute side paths match.
        text = text.replace("~/", str(Path.home()) + "/")
        text = text.replace(str(side.root), "<run>")
        text = text.replace(str(side.agent_dir), "<agent>")
        text = text.replace(str(side.work_dir), "<work>")
        text = re.sub(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b", "<uuid>", text)
        text = re.sub(r"(?<![a-zA-Z]) *\d+[smhd](?![a-zA-Z])", " <age>", text)
        text = re.sub(r"\bv\d+\.\d+[^ ]*", "<version>", text)
        text = re.sub(r"\$0\.\d\d", "<cost>", text)
        # The animated running-row icon and the per-build version string.
        text = re.sub("[\u25c7\u25c8\u25c6]", "<pulse>", text)
        text = re.sub(r"v\d+\.\d+\.\d+", "<version>", text)
        return text

    def reset_side_sessions(self, side: B.Side) -> None:
        """Restart the side daemon with no sessions: stop it, delete the
        session files, start a fresh one on the same socket."""
        try:
            wire = B.Wire(side.daemon_socket)
            wire.send_command("sd9", {"type": "shutdown"})
            wire.close()
        except (OSError, EOFError):
            pass
        side.stop_daemon()
        time.sleep(1.0)
        sessions = side.sessions_dir()
        if sessions.exists():
            shutil.rmtree(sessions)
        side.start_daemon()

    def settle_mock(self, side: B.Side, quiet_s: float = 2.0, timeout: float = 20.0) -> None:
        """Wait until the side's mock has gone `quiet_s` seconds with no new
        request, so post-turn status-line requests have landed before the
        flow swaps the response script (a swapped script resets the mock's
        response cursor, and a delayed response would hang a status-line
        request and hold the session busy)."""
        deadline = time.time() + timeout
        last_count = len(side.mock.requests())
        last_change = time.time()
        while time.time() < deadline:
            time.sleep(0.5)
            count = len(side.mock.requests())
            if count != last_count:
                last_count = count
                last_change = time.time()
            elif time.time() - last_change >= quiet_s:
                return

    def wait_roster_status(self, side: B.Side, active_session_id: str, status: str, timeout: float = 15.0) -> bool:
        """Poll the roster snapshot until the session reaches `status`."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                wire = B.Wire(side.daemon_socket)
            except (OSError, EOFError):
                time.sleep(0.5)
                continue
            try:
                response = wire.request("rp", {"type": "roster_subscribe"}, timeout=10)
            finally:
                wire.close()
            if response.get("success") is True:
                for entry in (response.get("data", {}).get("roster") or []):
                    if (entry.get("summary", {}).get("activeSessionId") == active_session_id
                            and entry.get("status") == status):
                        return True
            time.sleep(0.5)
        return False

    def f12_scale_resume(self) -> None:
        """HEAVY (opt-in: --flows f12_scale_resume + PA_BATTERY_HEAVY=1):
        transcript-scale interactive `--resume` -> ready, Rust vs TS. The
        corpus is deterministic (scripts/battery/scale_corpus.py): a
        PA_BATTERY_HEAVY_TURNS-turn session (default 5,000 = 15,261
        transcript rows) of user/assistant/ipython-tool turns. Ready means
        the interactive frame a user would type at: ingest of the resume
        snapshot plus the first full layout, in seconds — the regression
        gate for the replay path."""
        import scale_corpus as SC

        flow = "f12_scale_resume"
        if os.environ.get("PA_BATTERY_HEAVY") != "1":
            self.record(
                flow,
                "perf",
                "skipped: set PA_BATTERY_HEAVY=1 (and pass --flows f12_scale_resume) to run the heavy-scale resume gate",
                gap=False,
            )
            return
        turns = int(os.environ.get("PA_BATTERY_HEAVY_TURNS", "5000"))
        timeout_s = float(os.environ.get("PA_BATTERY_HEAVY_TIMEOUT", "300"))
        corpus = SC.corpus_path(turns, self.run_dir, str(self.run_dir))
        rows = SC.corpus_rows(turns)
        ready: dict[str, float | None] = {}
        for side in (self.sides["ts"], self.sides["rust"]):
            self.perf_onboard(side)
            socket = side.root / flow / "scale.sock"
            rec = P.measure_resume(
                side,
                f"{self.runid}-scale-{side.name}",
                socket,
                corpus,
                timeout_s,
            )
            P.stop_perf_daemon(socket)
            side.evidence_json(flow, "resume.json", rec)
            ready[side.name] = rec["ready_s"]
            if rec["ready_s"] is None:
                self.record(
                    flow,
                    "perf",
                    f"{side.name}: {turns}-turn ({rows}-row) interactive resume never reached ready within {timeout_s:.0f}s",
                    evidence=side.root / flow / "resume.json",
                )
            else:
                self.record(
                    flow,
                    "perf",
                    f"{side.name}: {turns}-turn ({rows}-row) interactive resume ready in {rec['ready_s']}s "
                    f"(first frame {rec['first_frame_s']}s)",
                    gap=False,
                )
        ts_ready = ready.get("ts")
        rs_ready = ready.get("rust")
        if ts_ready is not None and rs_ready is not None:
            ratio = rs_ready / ts_ready
            detail = (
                f"scale resume: rust {rs_ready:.3f}s vs ts {ts_ready:.3f}s for {rows} rows "
                f"(ratio {ratio:.2f}, thresholds: ratio <= {SCALE_RESUME_MAX_RATIO}, rust <= {SCALE_RESUME_MAX_READY_S}s)"
            )
            if rs_ready <= SCALE_RESUME_MAX_READY_S and ratio <= SCALE_RESUME_MAX_RATIO:
                self.record(flow, "perf", detail, gap=False)
            else:
                self.record(flow, "perf", f"REGRESSION {detail}", evidence=self.run_dir / flow)
        else:
            self.record(
                flow,
                "perf",
                "scale-resume thresholds not evaluable: a side never reached ready",
                gap=True,
            )

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

    # -- scroll + exit-lane verifiers (f12/f13) --------------------------------

    def settle_first_run(self, session: str, timeout: float = 40.0) -> None:
        """Wait past first-run dialogs (the trace notice) into the ready
        prompt; answers the notice when one shows (perf_onboard pattern)."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            frame = B.tmux_capture(session)
            if "Share agent traces" in frame:
                B.tmux_send(session, "Down")
                time.sleep(0.5)
                B.tmux_send(session, "Enter")
                time.sleep(1.0)
            elif P.is_ready(session, frame) or (">" in frame and "manage" in frame):
                return
            else:
                time.sleep(0.5)

    def f12_scroll(self) -> None:
        """Scrollback: PageUp pages history into view with the follow hint;
        paging back to the tail resumes following. Both sides must move the
        transcript window (pane frames carry the content offsets)."""
        flow = "f12_scroll"
        turns = 16
        for side in (self.sides["ts"], self.sides["rust"]):
            self.ensure_daemon(side)
            side.mock.set_responses([{"text": "scroll filler reply"}])
            session = f"{self.runid}-f12-{side.name}"
            argv = P.launch_argv(side, side.daemon_socket)
            B.tmux_launch(session, argv, side.env, side.work_dir)
            self.settle_first_run(session)
            for i in range(turns):
                mark = len(side.mock.requests())
                B.tmux_send(session, f"prompt {i}")
                deadline = time.time() + 30
                while time.time() < deadline and len(side.mock.requests()) <= mark:
                    time.sleep(0.2)
            time.sleep(2.0)
            follow_frame = B.tmux_capture(session)
            side.evidence(flow, "01-following.txt", follow_frame)
            for _ in range(6):
                B.tmux_send(session, "PageUp", enter=False)
                time.sleep(0.15)
            time.sleep(1.0)
            paged_frame = B.tmux_capture(session)
            side.evidence(flow, "02-paged.txt", paged_frame)
            for _ in range(6):
                B.tmux_send(session, "PageDown", enter=False)
                time.sleep(0.15)
            time.sleep(1.0)
            resume_frame = B.tmux_capture(session)
            side.evidence(flow, "03-resumed.txt", resume_frame)
            B.tmux_kill(session)
            checks = {
                "follow shows tail prompt": f"prompt {turns - 1}" in follow_frame,
                "follow hides top prompt": "prompt 0" not in follow_frame,
                "paged shows early prompt": "prompt 2" in paged_frame,
                "paged hides tail prompt": f"prompt {turns - 1}" not in paged_frame,
                "paged shows follow hint": "to follow" in paged_frame,
                "resumed shows tail prompt": f"prompt {turns - 1}" in resume_frame,
                "resumed hides follow hint": "to follow" not in resume_frame,
            }
            failed = [name for name, ok in checks.items() if not ok]
            if failed:
                self.record(
                    flow,
                    "behavior",
                    f"{side.name}: scrollback checks failed: {', '.join(failed)}",
                    evidence=side.root / flow,
                )
            else:
                self.record(
                    flow,
                    "behavior",
                    f"{side.name}: PageUp pages history into view with the follow hint; paging back to the tail resumes following",
                    gap=False,
                )

    def ctrl_c_exit_timing(self, side: B.Side, case: str, kill: str | None) -> dict:
        """Launch one interactive session, hold it mid-turn against a
        delayed mock response, send C-c C-c, and time the client exit.

        `kill` wedges the run before the exit gesture: "worker" kills the
        session worker (SIGKILL, dead worker socket mid-turn), "daemon"
        kills the supervisor process entirely (the worker is orphaned and
        the client's supervisor socket dies mid-turn). None keeps the run
        healthy."""
        self.ensure_daemon(side)
        side.mock.set_responses([{"text": "held reply", "delayMs": 20_000}])
        session = f"{self.runid}-f13-{side.name}-{case}"
        argv = P.launch_argv(side, side.daemon_socket)
        B.tmux_launch(session, argv, side.env, side.work_dir)
        B.tmux("set-option", "-t", session, "remain-on-exit", "on", check=False)
        self.settle_first_run(session)
        mark = len(side.mock.requests())
        B.tmux_send(session, "hold the turn")
        deadline = time.time() + 30
        while time.time() < deadline and len(side.mock.requests()) <= mark:
            time.sleep(0.2)
        if len(side.mock.requests()) <= mark:
            B.tmux_kill(session)
            return {"case": case, "error": "the held prompt never reached the mock"}
        if kill == "worker":
            # Kill the session worker mid-turn (SIGKILL): the worker socket
            # is dead and cannot answer the abort or the detach.
            children = B.children_of(side.daemon_proc.pid)
            if not children:
                B.tmux_kill(session)
                return {"case": case, "error": "no worker process found to wedge"}
            os.kill(children[-1], 9)
            time.sleep(0.5)
        if kill == "daemon":
            # Kill the supervisor process entirely (SIGKILL): the client's
            # supervisor socket dies mid-turn while the orphaned worker
            # keeps holding the turn against the mock.
            if side.daemon_proc is None or side.daemon_proc.poll() is not None:
                B.tmux_kill(session)
                return {"case": case, "error": "no daemon process to kill"}
            os.kill(side.daemon_proc.pid, 9)
            time.sleep(0.5)
        B.tmux_send(session, "C-c", enter=False)
        time.sleep(0.3)
        exit_at = time.time()
        B.tmux_send(session, "C-c", enter=False)
        deadline = exit_at + 10
        status = None
        while time.time() < deadline:
            out = B.tmux(
                "display-message",
                "-p",
                "-t",
                session,
                "#{pane_dead} #{pane_dead_status}",
                check=False,
            ).stdout.strip()
            if out.startswith("1"):
                status = out
                break
            time.sleep(0.05)
        elapsed_s = round(time.time() - exit_at, 2)
        frame = B.tmux_capture(session)
        side.evidence("f13_ctrlc_exit", f"{side.name}-{case}-final.txt", frame)
        B.tmux_kill(session)
        rec = {"case": case, "elapsed_s": elapsed_s}
        if status is None:
            rec["error"] = "the pane never died"
            return rec
        rec["status"] = status
        rec["exit_code"] = int(status.split()[1]) if len(status.split()) > 1 else None
        # The TS pane process is reaped late, so tmux never populates
        # pane_dead_status for it (the observed pane stays a defunct
        # process); its exit code is unobservable through tmux. The
        # shutdown-path resume hint (formatResumeHint) corroborates a
        # graceful shutdown instead.
        rec["resume_hint"] = "Resume this session with:" in frame
        # TS exit parity: leaving fullscreen flushes the transcript frame
        # onto the main screen (tui.ts exitFullscreen), so the dead pane
        # shows the last submitted prompt — not a blank screen. Both sides
        # must leave the exit frame behind.
        rec["exit_frame"] = "hold the turn" in frame
        return rec

    def f13_ctrlc_exit(self) -> None:
        """Double Ctrl+C exit: a long-turn session must exit promptly after the
        second press in all three daemon states — healthy, worker killed
        -9 mid-turn (dead worker socket), and the daemon process killed
        entirely (dead supervisor socket). The hard contract is exit
        within 2s of the second press with code 0; the healthy/wedged
        Rust cases keep the stricter 1s bound, the daemon-dead case gets
        the full 2s force-quit window."""
        flow = "f13_ctrlc_exit"
        for case, sides, kill in (
            ("healthy", (self.sides["ts"], self.sides["rust"]), None),
            ("wedged", (self.sides["rust"],), "worker"),
            ("daemon_dead", (self.sides["rust"],), "daemon"),
        ):
            for side in sides:
                rec = self.ctrl_c_exit_timing(side, case, kill)
                side.evidence_json(flow, f"{side.name}-{case}.json", rec)
                # The exit-latency contract is the Rust client's (exit within
                # 1s of the second press for the healthy and worker-killed cases,
                # within the 2s force-quit window when the daemon process itself
                # died, exit code 0). The TS reference's
                # timing is recorded as evidence; its shutdown drains input
                # for up to 1s by design, so only a clean exit is required.
                bound_s = 10.0
                if side.name == "rust":
                    bound_s = 2.0 if case == "daemon_dead" else 1.0
                # The TS exit code is often unobservable through tmux (the
                # reaped-late pane leaves no pane_dead_status); the TS source
                # exits via process.exit(0), so a dead pane inside the bound
                # with the shutdown-path resume hint counts as clean.
                exit_ok = rec.get("exit_code") == 0 or (
                    side.name == "ts" and rec.get("exit_code") is None and rec.get("resume_hint")
                )
                ok = (
                    "error" not in rec
                    and rec.get("elapsed_s") is not None
                    and rec["elapsed_s"] <= bound_s
                    and exit_ok
                    and rec.get("exit_frame")
                )
                if ok:
                    exit_code = rec.get("exit_code")
                    code_note = f"exit code {exit_code}" if exit_code is not None else "resume hint after exit"
                    self.record(
                        flow,
                        "behavior",
                        f"{side.name}: C-c C-c exits in {rec['elapsed_s']}s (case {case}, {code_note})",
                        gap=False,
                    )
                else:
                    summary = f"{side.name}: C-c C-c did not exit cleanly (case {case})"
                    if "error" in rec:
                        summary += f": {rec['error']}"
                    elif not rec.get("exit_frame"):
                        summary += ": the exit frame is missing from the dead pane"
                    elif rec.get("elapsed_s") is not None:
                        summary += (
                            f": exit took {rec['elapsed_s']}s, status {rec.get('status')}"
                        )
                    self.record(
                        flow,
                        "behavior",
                        summary,
                        evidence=side.root / flow / f"{side.name}-{case}.json",
                    )

    # -- real-surface helpers (f14-f21) --------------------------------------

    def suppress_first_run_notices(self, side: B.Side) -> dict:
        """Mark the side's onboarding/trace-notice state as already shown so
        the interactive panes settle straight into the conversation. The TS
        product re-runs the Welcome/trace-notice onboarding overlay on any
        fresh TUI instance (including mid-flow rebinds) until the state is
        persisted; a subset battery run (`--flows f15_a2a` without f1) would
        otherwise capture notice-overlaid frames (see the frozen partial run
        20260918T190022Z ts/f15_a2a/01-received.txt). Returns the settings
        dict so a flow can extend it before writing again."""
        settings_path = side.agent_dir / "settings.json"
        settings = json.loads(settings_path.read_text()) if settings_path.exists() else {}
        settings["onboardingShown"] = True
        settings["telemetry"] = {**(settings.get("telemetry") or {}), "noticeShown": True}
        settings_path.write_text(json.dumps(settings))
        return settings

    def settle_frame(self, session: str, quiet_s: float = 2.0, timeout: float = 40.0) -> str:
        """Poll the pane until it stops changing; returns the settled frame."""
        deadline = time.time() + timeout
        last = B.tmux_capture(session)
        last_change = time.time()
        while time.time() < deadline:
            time.sleep(0.5)
            frame = B.tmux_capture(session)
            if frame != last:
                last = frame
                last_change = time.time()
            elif time.time() - last_change >= quiet_s:
                return frame
        return last

    def create_session(self, side: B.Side, flow: str, name: str, wire_id: str, timeout: float = 120) -> tuple[B.Wire, str] | None:
        """Create one daemon session; returns (wire, session_id) or None after recording the failure."""
        self.ensure_daemon(side)
        wire = B.Wire(side.daemon_socket)
        create = wire.request(wire_id, {"type": "create", "name": name, "config": self.session_config(side)}, timeout=timeout)
        if create.get("success") is not True:
            self.record(
                flow,
                "protocol",
                f"{side.name}: daemon create failed: {json.dumps(create)[:300]}",
            )
            wire.close()
            return None
        session_id = (create.get("data", {}).get("activeSessionId") or create.get("data", {}).get("id") or "")
        return wire, session_id

    def tui_ready_loop(self, session: str, ready_marker: str, timeout: float = 90.0) -> str:
        """One ready gate for interactive panes: answer the first-run trace
        notice whenever it shows (it can appear after the first ready-looking
        frame — the splash carries ">" + "manage" too, and the transcript can
        render before the notice overlays it), and return only once
        `ready_marker` has rendered stably with no notice on top."""
        deadline = time.time() + timeout
        frame = ""
        stable_since: float | None = None
        while time.time() < deadline:
            frame = B.tmux_capture(session)
            if "Share agent traces" in frame:
                B.tmux_send(session, "Down")
                time.sleep(0.5)
                B.tmux_send(session, "Enter")
                time.sleep(1.5)
                stable_since = None
                continue
            if ready_marker and ready_marker in frame:
                if stable_since is None:
                    stable_since = time.time()
                elif time.time() - stable_since >= 3.0:
                    return frame
            else:
                stable_since = None
            time.sleep(0.5)
        return frame

    def tui_send(self, session: str, keys: str, enter: bool = True) -> None:
        """Send keys to an interactive pane, answering the first-run notice
        first if it happens to be on top (a notice arriving late would
        otherwise swallow the keystrokes)."""
        frame = B.tmux_capture(session)
        if "Share agent traces" in frame:
            B.tmux_send(session, "Down")
            time.sleep(0.5)
            B.tmux_send(session, "Enter")
            time.sleep(1.5)
        B.tmux_send(session, keys, enter=enter)

    def launch_tui(self, side: B.Side, flow: str, argv: list[str], ready_marker: str, timeout: float = 90.0) -> tuple[str, str]:
        """Launch one interactive pane (`argv`) and gate it through the
        notice-aware ready loop. Returns (tmux session, ready frame)."""
        session = f"{self.runid}-{flow}-{side.name}"
        B.tmux_launch(session, argv, side.env, side.work_dir)
        frame = self.tui_ready_loop(session, ready_marker, timeout=timeout)
        return session, frame

    def attach_tui(self, side: B.Side, flow: str, session_id: str, ready_marker: str = ">") -> tuple[str, str] | None:
        """Launch the interactive TUI attached to a live daemon session
        (`--resume <session id>` attaches on both products); waits past
        first-run notices into the ready prompt. Returns (tmux session,
        ready frame)."""
        # No --offline here: it disables telemetry for the invocation, and TS
        # refuses to attach to an active worker across that telemetry mismatch
        # (f6's CLI attach takes the same stance).
        argv = [
            side.binary,
            "--daemon-socket",
            str(side.daemon_socket),
            "--resume",
            session_id,
        ]
        return self.launch_tui(side, flow, argv, ready_marker)

    def frame_diff(self, flow: str, step: str, frames: dict[str, str], normalizer=None) -> None:
        """Cross-side diff of one captured frame pair, recorded as one
        finding (identical = pass; differ = gap evidence)."""
        ts_frame = frames.get("ts")
        rs_frame = frames.get("rust")
        if ts_frame is None or rs_frame is None:
            self.record(
                flow,
                "visual",
                f"{step}: missing frames (ts={'y' if ts_frame else 'n'} rust={'y' if rs_frame else 'n'})",
            )
            return
        ts_norm = normalizer(ts_frame, self.sides["ts"]) if normalizer else ts_frame
        rs_norm = normalizer(rs_frame, self.sides["rust"]) if normalizer else rs_frame
        if ts_norm == rs_norm:
            self.record(
                flow,
                "visual",
                f"{step}: frames identical TS vs Rust (normalized)",
                gap=False,
            )
        else:
            diff_path = self.sides["ts"].root / flow / f"frame-diff-{step}.txt"
            diff_path.parent.mkdir(parents=True, exist_ok=True)
            diff_path.write_text(f"--- ts ({step})\n{ts_frame}\n+++ rust ({step})\n{rs_frame}")
            self.record(
                flow,
                "visual",
                f"{step}: frames differ TS vs Rust (see frame-diff-{step}.txt)",
                evidence=diff_path,
                lane=FLOW_LANES.get(flow),
            )

    @staticmethod
    def normalize_transcript_frame(frame: str, side: B.Side) -> str:
        """Transcript-frame normalization: scrub side-specific paths, session
        ids, timestamps, token counts, and trailing status lines so the diff
        measures the durable rows, not volatile metadata."""
        text = frame
        for value in (str(side.root), str(side.agent_dir), str(side.work_dir)):
            text = text.replace(value, "<dir>")
        text = re.sub(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", "<uuid>", text)
        text = re.sub(r"\b[0-9,]{4,}\b", "<num>", text)
        text = re.sub(r"\b\d+(\.\d+)?s\b", "<dur>", text)
        return text


    def f14_compact(self) -> None:
        """/compact visible feedback and the auto-compaction threshold
        crossing. Two sessions: the manual /compact loader + durable
        `◆ Context compacted` summary row (session A), and the
        `Auto-compacting...` loader + compacted row when one large turn
        crosses the reserve-token headroom (session B — a manual compact
        first would leave `Already compacted` skips for the auto path).
        Settings shape both: reserveTokens 127500 leaves a 500-token
        headroom on the 128k mock model (the mock-reported 126k usage
        crosses it), and keepRecentTokens 10 makes the seeded turns
        compactable (the TS compactor skips sessions whose recent history
        already fits keepRecentTokens with "Session is too short to
        compact" — the frozen 20260918T190022Z partial run showed exactly
        that skip with the default 20000). Frame diff TS vs Rust at each
        key moment."""
        flow = "f14_compact"
        reply = "f14 parity fixture reply"
        frames: dict[str, dict[str, str]] = {"ts": {}, "rust": {}}
        for side in (self.sides["ts"], self.sides["rust"]):
            settings = self.suppress_first_run_notices(side)
            settings["compaction"] = {"enabled": True, "reserveTokens": 127500, "keepRecentTokens": 10}
            settings_path = side.agent_dir / "settings.json"
            settings_path.write_text(json.dumps(settings))
            # -- session A: manual /compact --------------------------------
            made = self.create_session(side, flow, "battery-f14-manual", "c14m")
            if made:
                wire, session_id = made
                # Seed turns: deterministic content for the compactor to keep.
                side.mock.set_responses([{"text": reply}])
                for n in (1, 2):
                    wire.request(
                        f"p14s{n}",
                        {"type": "prompt_and_wait", "activeSessionId": session_id,
                         "message": f"f14 seed turn {n} of the parity battery with deterministic content"},
                        timeout=240,
                    )
                wire.close()
                self.settle_mock(side)
                attached = self.attach_tui(side, flow, session_id, ready_marker="f14 seed turn 2")
                if not attached:
                    self.record(flow, "visual", f"{side.name}: attached TUI never rendered the seeded transcript")
                else:
                    tui, ready = attached
                    side.evidence(flow, "00-attached.txt", ready)
                    # Manual /compact: the loader while running, the durable
                    # outcome row after (the summarization request pops the
                    # mock's repeating text response; its default usage keeps
                    # the session below the auto-compaction headroom).
                    self.tui_send(tui, "/compact")
                    # The mock summarization returns instantly, so the
                    # loader may be gone before the first poll; keep the wait
                    # short and let the settled row be the real gate.
                    during = B.tmux_wait_text(tui, "ompacting", timeout=8)
                    side.evidence(flow, "01-during-compact.txt", during)
                    settled = self.settle_frame(tui, quiet_s=3.0, timeout=90)
                    side.evidence(flow, "02-after-compact.txt", settled)
                    frames[side.name]["manual-compact"] = settled
                    if "Context compacted" in settled:
                        self.record(
                            flow, "visual",
                            f"{side.name}: /compact shows the durable '◆ Context compacted' summary row",
                            gap=False,
                        )
                    else:
                        self.record(
                            flow, "visual",
                            f"{side.name}: /compact produced no visible '◆ Context compacted' summary row",
                            evidence=side.root / flow / "02-after-compact.txt",
                            lane=FLOW_LANES[flow],
                        )
                    B.tmux_kill(tui)
                self.copy_sessions(side, flow)
            # -- session B: auto-compaction threshold crossing ---------------
            made = self.create_session(side, flow, "battery-f14-auto", "c14a")
            if made:
                wire, session_id = made
                side.mock.set_responses([{"text": reply}])
                wire.request(
                    "p14a",
                    {"type": "prompt_and_wait", "activeSessionId": session_id, "message": "f14 auto seed turn"},
                    timeout=240,
                )
                wire.close()
                self.settle_mock(side)
                attached = self.attach_tui(side, flow, session_id, ready_marker="f14 auto seed turn")
                if not attached:
                    self.record(flow, "visual", f"{side.name}: attached TUI never rendered the auto-compaction seeded transcript")
                else:
                    tui, ready = attached
                    side.evidence(flow, "03-auto-attached.txt", ready)
                    # The mock reports usage past the 500-token headroom
                    # (128000 - reserveTokens 127500), so the threshold check
                    # fires after this turn; the follow-up response carries
                    # the default (small) usage so the compacted session does
                    # not re-cross the threshold and loop.
                    self.settle_mock(side)
                    side.mock.set_responses(
                        [
                            {"text": reply, "usage": {"prompt_tokens": 126000, "completion_tokens": 10, "total_tokens": 126010, "prompt_tokens_details": {"cached_tokens": 80}}},
                            {"text": reply},
                        ]
                    )
                    self.tui_send(tui, "f14 threshold crossing turn")
                    auto_during = B.tmux_wait_text(tui, "Auto-compacting|Context compacted", timeout=90)
                    side.evidence(flow, "04-auto-during.txt", auto_during)
                    settled2 = self.settle_frame(tui, quiet_s=3.0, timeout=120)
                    side.evidence(flow, "05-auto-after.txt", settled2)
                    frames[side.name]["auto-compact"] = settled2
                    if "Context compacted" in settled2:
                        self.record(
                            flow, "behavior",
                            f"{side.name}: crossing the compaction threshold auto-compacts and shows the summary row",
                            gap=False,
                        )
                    else:
                        self.record(
                            flow, "behavior",
                            f"{side.name}: threshold crossing produced no visible auto-compaction outcome",
                            evidence=side.root / flow / "05-auto-after.txt",
                            lane=FLOW_LANES[flow],
                        )
                    side.evidence_json(flow, "mock-requests.json", side.mock.requests())
                    B.tmux_kill(tui)
                self.copy_sessions(side, flow)
            # Restore default compaction settings for the flows that follow.
            settings["compaction"] = {"enabled": True}
            settings_path.write_text(json.dumps(settings))
        for step in ("manual-compact", "auto-compact"):
            self.frame_diff(
                flow, step,
                {name: frames[name].get(step, "") for name in ("ts", "rust")},
                self.normalize_transcript_frame,
            )


    def f15_a2a(self) -> None:
        """agent_message send/receive: the ◆ diamond-decorated rows in both
        directions — the received row in the receiver's transcript, the sent
        summary row inside the sender's ipython cell — with participant
        labels and the expanded preview body. Two sibling daemon sessions
        exchange one message each way through their kernels."""
        flow = "f15_a2a"
        reply = "f15 a2a turn reply"
        frames: dict[str, dict[str, str]] = {"ts": {}, "rust": {}}
        for side in (self.sides["ts"], self.sides["rust"]):
            self.suppress_first_run_notices(side)
            made = self.create_session(side, flow, "battery-f15-a", "c15a")
            if not made:
                continue
            wire_a, id_a = made
            made = self.create_session(side, flow, "battery-f15-b", "c15b")
            if not made:
                wire_a.close()
                continue
            wire_b, id_b = made
            # Seed both sessions so each is idle and named on the roster.
            side.mock.set_responses([{"text": reply}])
            wire_a.request(
                "p15a", {"type": "prompt_and_wait", "activeSessionId": id_a, "message": "f15 seed a"}, timeout=240
            )
            wire_b.request(
                "p15b", {"type": "prompt_and_wait", "activeSessionId": id_b, "message": "f15 seed b"}, timeout=240
            )
            wire_a.close()
            wire_b.close()
            self.settle_mock(side)
            # Attach to B; A's kernel sends one sibling message.
            attached = self.attach_tui(side, flow, id_b, ready_marker="f15 seed b")
            if not attached:
                self.record(flow, "visual", f"{side.name}: attached TUI never rendered the seeded transcript")
                continue
            tui, ready = attached
            side.evidence(flow, "00-attached.txt", ready)
            payload_a = "f15 a2a payload from a to b"
            code_a = (
                "import agent_message; await agent_message.send("
                f"{payload_a!r}, receiver_role='sibling', receiver_name='battery-f15-b')"
            )
            side.mock.set_responses(
                [
                    {"toolCall": {"name": "ipython", "arguments": {"code": code_a}}},
                    {"text": reply},
                ]
            )
            wire_a = B.Wire(side.daemon_socket)
            wire_a.send_command(
                "p15s",
                {"type": "prompt_and_wait", "activeSessionId": id_a, "message": "f15 send the sibling message to b"},
            )
            wire_a.close()
            received = B.tmux_wait_text(tui, "Agent message received|f15 a2a payload", timeout=120)
            side.evidence(flow, "01-received.txt", received)
            settled = self.settle_frame(tui, quiet_s=3.0, timeout=90)
            side.evidence(flow, "02-received-settled.txt", settled)
            frames[side.name]["received"] = settled
            if "Agent message received" in settled:
                self.record(
                    flow, "visual",
                    f"{side.name}: a sibling agent message renders the '◆ Agent message received' row with participant label",
                    gap=False,
                )
            else:
                self.record(
                    flow, "visual",
                    f"{side.name}: the delivered sibling message shows no 'Agent message received' row",
                    evidence=side.root / flow / "02-received-settled.txt",
                    lane=FLOW_LANES[flow],
                )
            # Reverse direction: B's kernel sends back to A (prompted through
            # the attached TUI so the sent row renders in B's own pane).
            self.settle_mock(side)
            payload_b = "f15 a2a payload from b to a"
            code_b = (
                "import agent_message; await agent_message.send("
                f"{payload_b!r}, receiver_role='sibling', receiver_name='battery-f15-a')"
            )
            side.mock.set_responses(
                [
                    {"toolCall": {"name": "ipython", "arguments": {"code": code_b}}},
                    {"text": reply},
                ]
            )
            self.tui_send(tui, "f15 send the sibling message back to a")
            sent = B.tmux_wait_text(tui, "Agent message sent|Agent message queued", timeout=120)
            side.evidence(flow, "03-sent.txt", sent)
            settled2 = self.settle_frame(tui, quiet_s=3.0, timeout=90)
            side.evidence(flow, "04-sent-settled.txt", settled2)
            frames[side.name]["sent"] = settled2
            if "Agent message sent" in settled2 or "Agent message queued" in settled2:
                self.record(
                    flow, "visual",
                    f"{side.name}: the sender's ipython cell renders the '◆ Agent message sent/queued' summary row with the participant label",
                    gap=False,
                )
            else:
                self.record(
                    flow, "visual",
                    f"{side.name}: the sender's ipython cell shows no '◆ Agent message sent/queued' row",
                    evidence=side.root / flow / "04-sent-settled.txt",
                    lane=FLOW_LANES[flow],
                )
            B.tmux_kill(tui)
            self.copy_sessions(side, flow)
        for step in ("received", "sent"):
            self.frame_diff(
                flow, step,
                {name: frames[name].get(step, "") for name in ("ts", "rust")},
                self.normalize_transcript_frame,
            )

    def f16_refine(self) -> None:
        """refine.run() from the kernel: the ◆ decorated refinement-outcome
        row, and the harness_digest user message rendered on the next
        boundary. One daemon session, one kernel-scheduled refinement."""
        flow = "f16_refine"
        reply = "f16 refine fixture reply"
        frames: dict[str, dict[str, str]] = {"ts": {}, "rust": {}}
        for side in (self.sides["ts"], self.sides["rust"]):
            self.suppress_first_run_notices(side)
            made = self.create_session(side, flow, "battery-f16", "c16")
            if not made:
                continue
            wire, session_id = made
            side.mock.set_responses([{"text": reply}])
            wire.request(
                "p16s", {"type": "prompt_and_wait", "activeSessionId": session_id, "message": "f16 seed turn"}, timeout=240
            )
            wire.close()
            self.settle_mock(side)
            attached = self.attach_tui(side, flow, session_id, ready_marker="f16 seed turn")
            if not attached:
                self.record(flow, "visual", f"{side.name}: attached TUI never rendered the seeded transcript")
                continue
            tui, ready = attached
            side.evidence(flow, "00-attached.txt", ready)
            code = (
                "import refine; await refine.run("
                "instructions='create one local memory titled f16-battery-memory describing this parity check')"
            )
            # The refinement runs host-side when the turn ends: after the
            # tool-call reply and the turn's final text, the refinement
            # planning request consumes the next queued response, so the
            # queue scripts the exact refinement proposal JSON (the
            # "You are Prime Agent's /refine continual harness subsystem"
            # prompt's schema). Without it the mock repeats the last plain
            # text response and refinement never applies (TS run
            # 20260918T231155Z: both sides failed the outcome row on this).
            refinement = {
                "summary": "Record the f16 battery parity fixture",
                "rationale": "The parity check requires one local memory entry to prove refinement applies edits.",
                "expectedOutcome": "A local memory titled f16-battery-memory exists.",
                "edits": [
                    {
                        "action": "create",
                        "kind": "memory",
                        "title": "f16-battery-memory",
                        "content": "The f16 battery flow verified the kernel-scheduled refinement end to end.",
                        "metadata": {"scope": "local"},
                        "reason": "battery parity fixture",
                    }
                ],
            }
            side.mock.set_responses(
                [
                    {"toolCall": {"name": "ipython", "arguments": {"code": code}}},
                    {"text": reply},
                    {"text": json.dumps(refinement)},
                    # The harness resumes the model after applying the edits.
                    {"text": reply},
                ]
            )
            self.tui_send(tui, "f16 schedule the refinement now")
            outcome = B.tmux_wait_text(tui, "Harness refined|Harness unchanged|refine", timeout=150)
            side.evidence(flow, "01-refine-outcome.txt", outcome)
            settled = self.settle_frame(tui, quiet_s=4.0, timeout=120)
            side.evidence(flow, "02-refine-settled.txt", settled)
            frames[side.name]["refine"] = settled
            if "Harness refined" in settled:
                self.record(
                    flow, "visual",
                    f"{side.name}: the kernel-scheduled refinement renders the '◆ Harness refined' outcome row",
                    gap=False,
                )
            else:
                self.record(
                    flow, "visual",
                    f"{side.name}: no '◆ Harness refined' outcome row after the kernel-scheduled refinement",
                    evidence=side.root / flow / "02-refine-settled.txt",
                    lane=FLOW_LANES[flow],
                )
            # The harness digest lands on the next turn boundary; the next
            # prompt forces it into the transcript.
            self.settle_mock(side)
            side.mock.set_responses([{"text": reply}])
            self.tui_send(tui, "f16 follow up turn")
            digest = B.tmux_wait_text(tui, "harness-digest", timeout=150)
            side.evidence(flow, "03-digest.txt", digest)
            settled2 = self.settle_frame(tui, quiet_s=4.0, timeout=120)
            side.evidence(flow, "04-digest-settled.txt", settled2)
            frames[side.name]["digest"] = settled2
            if "harness-digest" in settled2:
                self.record(
                    flow, "visual",
                    f"{side.name}: the harness digest renders as a [harness-digest] message row on the boundary",
                    gap=False,
                )
            else:
                self.record(
                    flow, "visual",
                    f"{side.name}: no [harness-digest] message row on the post-refinement boundary",
                    evidence=side.root / flow / "04-digest-settled.txt",
                    lane=FLOW_LANES[flow],
                )
            B.tmux_kill(tui)
            self.copy_sessions(side, flow)
        for step in ("refine", "digest"):
            self.frame_diff(
                flow, step,
                {name: frames[name].get(step, "") for name in ("ts", "rust")},
                self.normalize_transcript_frame,
            )

    def f17_slash_model(self) -> None:
        """/model and /effort pickers: the selector overlay open, a
        selection through the picker, and the picker's confirm rows —
        TS-identical result rows both sides."""
        flow = "f17_slash_model"
        frames: dict[str, dict[str, str]] = {"ts": {}, "rust": {}}
        for side in (self.sides["ts"], self.sides["rust"]):
            self.ensure_daemon(side)
            self.suppress_first_run_notices(side)
            tui, ready = self.launch_tui(side, flow, P.launch_argv(side, side.daemon_socket), ready_marker=">")
            side.evidence(flow, "00-ready.txt", ready)
            # /model: the selector overlay.
            self.tui_send(tui, "/model")
            time.sleep(2.5)
            selector = B.tmux_capture(tui)
            side.evidence(flow, "01-model-selector.txt", selector)
            frames[side.name]["model-selector"] = selector
            if "mock-1" in selector or "Mock 1" in selector:
                self.record(flow, "visual", f"{side.name}: /model opens the selector with the configured model listed", gap=False)
            else:
                self.record(
                    flow, "visual",
                    f"{side.name}: /model selector did not list the configured mock model",
                    evidence=side.root / flow / "01-model-selector.txt",
                    lane=FLOW_LANES[flow],
                )
            # Select the model: type the search, confirm the first row.
            self.tui_send(tui, "mock")
            time.sleep(1.0)
            self.tui_send(tui, "Enter", enter=False)
            selected = B.tmux_wait_text(tui, "Model: |Switching model", timeout=30)
            side.evidence(flow, "02-model-selected.txt", selected)
            settled = self.settle_frame(tui, quiet_s=2.0, timeout=30)
            side.evidence(flow, "03-model-selected-settled.txt", settled)
            frames[side.name]["model-selected"] = settled
            if "Model: " in settled:
                self.record(flow, "visual", f"{side.name}: picking a model in the selector shows the 'Model: <id>' confirm row", gap=False)
            else:
                self.record(
                    flow, "visual",
                    f"{side.name}: no 'Model: <id>' confirm row after picking in the selector",
                    evidence=side.root / flow / "03-model-selected-settled.txt",
                    lane=FLOW_LANES[flow],
                )
            # /effort: the thinking-level picker (or the unsupported-model row).
            self.tui_send(tui, "/effort")
            time.sleep(2.5)
            effort = B.tmux_capture(tui)
            side.evidence(flow, "04-effort-picker.txt", effort)
            frames[side.name]["effort-picker"] = effort
            if "Thinking level" in effort or "thinking" in effort.lower():
                self.record(
                    flow, "visual",
                    f"{side.name}: /effort shows the thinking-level picker or its unsupported-model row",
                    gap=False,
                )
            else:
                self.record(
                    flow, "visual",
                    f"{side.name}: /effort showed no thinking-level surface",
                    evidence=side.root / flow / "04-effort-picker.txt",
                    lane=FLOW_LANES[flow],
                )
            self.tui_send(tui, "Escape", enter=False)
            time.sleep(0.5)
            B.tmux_kill(tui)
        for step in ("model-selector", "model-selected", "effort-picker"):
            self.frame_diff(
                flow, step,
                {name: frames[name].get(step, "") for name in ("ts", "rust")},
                self.normalize_transcript_frame,
            )


    def f18_goal_autonomous(self) -> None:
        """/goal lifecycle rows and /autonomous on/off rendering: the goal
        start context row, the status/pause/resume rows, the completion row
        (kernel `goal.complete()`), and the autonomous_status rows."""
        flow = "f18_goal_autonomous"
        reply = "f18 goal fixture reply"
        objective = "f18 parity objective: land the battery flows"
        frames: dict[str, dict[str, str]] = {"ts": {}, "rust": {}}
        for side in (self.sides["ts"], self.sides["rust"]):
            self.suppress_first_run_notices(side)
            made = self.create_session(side, flow, "battery-f18", "c18")
            if not made:
                continue
            wire, session_id = made
            side.mock.set_responses([{"text": reply}])
            wire.request(
                "p18s", {"type": "prompt_and_wait", "activeSessionId": session_id, "message": "f18 seed turn"}, timeout=240
            )
            wire.close()
            self.settle_mock(side)
            attached = self.attach_tui(side, flow, session_id, ready_marker="f18 seed turn")
            if not attached:
                self.record(flow, "visual", f"{side.name}: attached TUI never rendered the seeded transcript")
                continue
            tui, ready = attached
            side.evidence(flow, "00-attached.txt", ready)
            # /goal <objective>: the start row, goal-context turn, tray
            # label. Wait only for the row: with the goal active the TS
            # daemon immediately starts driving [goal: continuation]
            # turns, and the frame never goes quiet, so a settle here would
            # burn its full timeout while the loop churns (runs
            # 20260918T231155Z / 20260919T002527Z: the TS goal driver
            # issued ~2900 continuation turns and starved queued composer
            # input for minutes per command).
            self.tui_send(tui, f"/goal {objective}")
            settled = B.tmux_wait_text(tui, "Goal active|Pursuing goal|Goal context", timeout=30)
            side.evidence(flow, "01-goal-start.txt", settled)
            frames[side.name]["goal-start"] = settled
            if ("Goal context" in settled or "Pursuing goal" in settled or "Goal active" in settled):
                self.record(
                    flow, "visual",
                    f"{side.name}: /goal start renders the goal context row / active goal label",
                    gap=False,
                )
            else:
                self.record(
                    flow, "visual",
                    f"{side.name}: /goal start produced no visible goal row",
                    evidence=side.root / flow / "01-goal-start.txt",
                    lane=FLOW_LANES[flow],
                )
            # Pause immediately while the driver's queue is still shallow:
            # every queued command lands after the loop drains (minutes when
            # typed late), but lands within a turn or two when typed right
            # after the start. Everything else in the flow then runs against
            # a paused (quiet) goal driver.
            self.tui_send(tui, "/goal pause")
            settled = B.tmux_wait_text(tui, "Goal paused", timeout=120)
            side.evidence(flow, "03-goal-pause.txt", settled)
            frames[side.name]["goal-pause"] = settled
            if "Goal paused" in settled:
                self.record(flow, "visual", f"{side.name}: /goal pause renders the paused row", gap=False)
            else:
                self.record(
                    flow, "visual",
                    f"{side.name}: /goal pause produced no visible paused row",
                    evidence=side.root / flow / "03-goal-pause.txt",
                    lane=FLOW_LANES[flow],
                )
            # /goal (status): the status row (the driver is paused, so the
            # frame settles normally).
            self.tui_send(tui, "/goal")
            settled = self.settle_frame(tui, quiet_s=3.0, timeout=30)
            side.evidence(flow, "02-goal-status.txt", settled)
            frames[side.name]["goal-status"] = settled
            # /goal resume, then pause again before scripting responses: the
            # resumed goal driver keeps issuing [goal: continuation] turns
            # that drain the mock's response queue (run 20260918T231155Z:
            # the loop fired ~870 continuation requests through the rest of
            # the run and starved the f18 completion turn and the f21
            # recovery turn). Pausing right after the resume capture keeps
            # the churn to a turn or two.
            self.tui_send(tui, "/goal resume")
            settled = B.tmux_wait_text(tui, "Goal", timeout=15)
            side.evidence(flow, "04-goal-resume.txt", settled)
            frames[side.name]["goal-resume"] = settled
            self.tui_send(tui, "/goal pause")
            paused2 = B.tmux_wait_text(tui, "Goal paused", timeout=120)
            side.evidence(flow, "04b-goal-pause-2.txt", paused2)
            # Completion: the kernel's goal.complete() (scripted tool call).
            # The 6s quiet window matters: the previous turn's status-line
            # request can land seconds after the turn ends and would
            # otherwise pop the scripted tool call out of the queue.
            self.settle_mock(side, quiet_s=6.0, timeout=30.0)
            code = "import goal; await goal.complete()"
            side.mock.set_responses(
                [
                    {"toolCall": {"name": "ipython", "arguments": {"code": code}}},
                    {"text": reply},
                ]
            )
            self.tui_send(tui, "f18 finish the goal now")
            settled = self.settle_frame(tui, quiet_s=4.0, timeout=120)
            side.evidence(flow, "05-goal-complete.txt", settled)
            frames[side.name]["goal-complete"] = settled
            if "Goal complete" in settled:
                self.record(flow, "visual", f"{side.name}: goal.complete() renders the completion row", gap=False)
            else:
                self.record(
                    flow, "visual",
                    f"{side.name}: goal.complete() produced no visible completion row",
                    evidence=side.root / flow / "05-goal-complete.txt",
                    lane=FLOW_LANES[flow],
                )
            # /autonomous on and off: the autonomous_status rows.
            self.settle_mock(side)
            side.mock.set_responses([{"text": reply}])
            self.tui_send(tui, "/autonomous on")
            settled = self.settle_frame(tui, quiet_s=3.0, timeout=60)
            side.evidence(flow, "06-autonomous-on.txt", settled)
            frames[side.name]["autonomous-on"] = settled
            if "Autonomous" in settled or "autonomous" in settled:
                self.record(flow, "visual", f"{side.name}: /autonomous on renders the autonomous status row", gap=False)
            else:
                self.record(
                    flow, "visual",
                    f"{side.name}: /autonomous on produced no visible status row",
                    evidence=side.root / flow / "06-autonomous-on.txt",
                    lane=FLOW_LANES[flow],
                )
            self.tui_send(tui, "/autonomous off")
            settled = self.settle_frame(tui, quiet_s=3.0, timeout=60)
            side.evidence(flow, "07-autonomous-off.txt", settled)
            frames[side.name]["autonomous-off"] = settled
            B.tmux_kill(tui)
            self.copy_sessions(side, flow)
        for step in ("goal-start", "goal-status", "goal-pause", "goal-resume", "goal-complete", "autonomous-on", "autonomous-off"):
            self.frame_diff(
                flow, step,
                {name: frames[name].get(step, "") for name in ("ts", "rust")},
                self.normalize_transcript_frame,
            )


    def f19_heartbeat(self) -> None:
        """/heartbeat visible surface: the set status row, the fired
        heartbeat prompt row (♥ prefix + schedule label), and the
        /heartbeats manager view."""
        flow = "f19_heartbeat"
        reply = "f19 heartbeat fixture reply"
        frames: dict[str, dict[str, str]] = {"ts": {}, "rust": {}}
        for side in (self.sides["ts"], self.sides["rust"]):
            self.suppress_first_run_notices(side)
            made = self.create_session(side, flow, "battery-f19", "c19")
            if not made:
                continue
            wire, session_id = made
            side.mock.set_responses([{"text": reply}])
            wire.request(
                "p19s", {"type": "prompt_and_wait", "activeSessionId": session_id, "message": "f19 seed turn"}, timeout=240
            )
            wire.close()
            self.settle_mock(side)
            attached = self.attach_tui(side, flow, session_id, ready_marker="f19 seed turn")
            if not attached:
                self.record(flow, "visual", f"{side.name}: attached TUI never rendered the seeded transcript")
                continue
            tui, ready = attached
            side.evidence(flow, "00-attached.txt", ready)
            # /heartbeat every 10s: the set status row.
            self.tui_send(tui, "/heartbeat every 10s f19 heartbeat ping instruction")
            settled = self.settle_frame(tui, quiet_s=3.0, timeout=60)
            side.evidence(flow, "01-heartbeat-set.txt", settled)
            frames[side.name]["heartbeat-set"] = settled
            if "Heartbeat set" in settled:
                self.record(flow, "visual", f"{side.name}: /heartbeat renders the 'Heartbeat set' status row", gap=False)
            else:
                self.record(
                    flow, "visual",
                    f"{side.name}: /heartbeat produced no 'Heartbeat set' status row",
                    evidence=side.root / flow / "01-heartbeat-set.txt",
                    lane=FLOW_LANES[flow],
                )
            # The heartbeat fires (10s schedule): the injected prompt row with
            # the ♥ prefix and schedule label, plus its model turn.
            fired = B.tmux_wait_text(tui, "Heartbeat prompt", timeout=60)
            side.evidence(flow, "02-heartbeat-fired.txt", fired)
            settled = self.settle_frame(tui, quiet_s=4.0, timeout=90)
            side.evidence(flow, "03-heartbeat-fired-settled.txt", settled)
            frames[side.name]["heartbeat-fired"] = settled
            if "Heartbeat prompt" in settled:
                self.record(
                    flow, "behavior",
                    f"{side.name}: a fired heartbeat renders the '♥ Heartbeat prompt · every 10s' row",
                    gap=False,
                )
            else:
                self.record(
                    flow, "behavior",
                    f"{side.name}: the fired heartbeat produced no visible '♥ Heartbeat prompt' row",
                    evidence=side.root / flow / "03-heartbeat-fired-settled.txt",
                    lane=FLOW_LANES[flow],
                )
            # /heartbeats: the manager view.
            self.tui_send(tui, "/heartbeats")
            time.sleep(2.5)
            manager = B.tmux_capture(tui)
            side.evidence(flow, "04-heartbeats-manager.txt", manager)
            frames[side.name]["heartbeats-manager"] = manager
            if "Heartbeats" in manager and ("heartbeat" in manager.lower()):
                self.record(flow, "visual", f"{side.name}: /heartbeats opens the heartbeat manager view", gap=False)
            else:
                self.record(
                    flow, "visual",
                    f"{side.name}: /heartbeats did not open a heartbeat manager view",
                    evidence=side.root / flow / "04-heartbeats-manager.txt",
                    lane=FLOW_LANES[flow],
                )
            self.tui_send(tui, "Escape", enter=False)
            time.sleep(0.5)
            # Stop the heartbeat so the daemon does not keep firing it.
            self.tui_send(tui, "/heartbeat stop")
            time.sleep(1.0)
            B.tmux_kill(tui)
            self.copy_sessions(side, flow)
        for step in ("heartbeat-set", "heartbeat-fired", "heartbeats-manager"):
            self.frame_diff(
                flow, step,
                {name: frames[name].get(step, "") for name in ("ts", "rust")},
                self.normalize_transcript_frame,
            )


    def f20_subagents(self) -> None:
        """rlm.spawn() from the kernel, attached through the agents view: the
        subagent summary line above the editor (live running counts, then the
        child's `RLM child status` no-reply terminal notice), and the scoped
        agents view opened from the focused summary line listing the child by
        name. Frame diff TS vs Rust at each key moment."""
        flow = "f20_subagents"
        reply = "f20 parent fixture reply"
        child_reply = "f20 child fixture reply"
        child_name = "f20-worker"
        frames: dict[str, dict[str, str]] = {"ts": {}, "rust": {}}
        for side in (self.sides["ts"], self.sides["rust"]):
            made = self.create_session(side, flow, "battery-f20", "c20")
            if not made:
                continue
            wire, session_id = made
            side.mock.set_responses([{"text": reply}])
            wire.request(
                "p20s", {"type": "prompt_and_wait", "activeSessionId": session_id, "message": "f20 seed turn"}, timeout=240
            )
            wire.close()
            self.settle_mock(side)
            # TS suppresses the agents view while onboarding is pending
            # (f9 precedent); suppress_first_run_notices marks the side
            # onboarded so both products open the view directly. No --offline
            # on this launch either: TS refuses to attach an active agent
            # across a telemetry mismatch.
            self.suppress_first_run_notices(side)
            view = f"{self.runid}-f20-{side.name}"
            B.tmux_launch(view, [side.binary, "agents", "--daemon-socket", str(side.daemon_socket)], side.env, side.work_dir)
            listed = B.tmux_wait_text(view, "Running \(|Idle \(|Inactive \(|No sessions", timeout=30)
            side.evidence(flow, "00-agents-view.txt", listed)
            # Open the parent session from the view: the conversation then
            # carries returnToAgentsView, which is what makes the subagent
            # summary line openable into the scoped view.
            B.tmux_send(view, "battery-f20")
            time.sleep(1.0)
            B.tmux_send(view, "Enter", enter=False)
            attached = B.tmux_wait_text(view, "f20 seed turn", timeout=60)
            if "f20 seed turn" not in attached:
                self.record(flow, "visual", f"{side.name}: opening battery-f20 from the agents view did not attach to the seeded session", evidence=side.root / flow / "00-agents-view.txt")
                B.tmux_kill(view)
                continue
            # Kernel rlm.spawn through the attached editor: the ipython cell
            # spawns one named child. The child's own model turn pops the next
            # mock response (plain text), so the child finishes without an
            # agent_message reply and the parent receives the no-reply
            # terminal notice.
            self.settle_mock(side)
            code = (
                "import rlm; await rlm.spawn("
                "'f20 child task: reply with the fixture summary', "
                f"name={child_name!r})"
            )
            side.mock.set_responses(
                [
                    {"toolCall": {"name": "ipython", "arguments": {"code": code}}},
                    {"text": child_reply},
                    {"text": reply},
                ]
            )
            self.tui_send(view, "f20 spawn the subagent now")
            spawned = B.tmux_wait_text(view, "subagents", timeout=180)
            side.evidence(flow, "01-spawn.txt", spawned)
            settled = self.settle_frame(view, quiet_s=3.0, timeout=90)
            side.evidence(flow, "02-spawn-settled.txt", settled)
            frames[side.name]["spawn"] = settled
            if "subagents" in settled:
                self.record(
                    flow, "visual",
                    f"{side.name}: a kernel rlm.spawn renders the subagent summary line above the editor with the running/idle/inactive counts",
                    gap=False,
                )
            else:
                self.record(
                    flow, "visual",
                    f"{side.name}: a kernel rlm.spawn produced no visible subagent summary line",
                    evidence=side.root / flow / "02-spawn-settled.txt",
                    lane=FLOW_LANES[flow],
                )
            # Child completion without a reply: the `RLM child status`
            # terminal-notice row in the parent transcript.
            noticed = B.tmux_wait_text(view, "RLM child status", timeout=240)
            side.evidence(flow, "03-child-status.txt", noticed)
            settled2 = self.settle_frame(view, quiet_s=3.0, timeout=90)
            side.evidence(flow, "04-child-status-settled.txt", settled2)
            frames[side.name]["child-status"] = settled2
            if "RLM child status" in settled2:
                self.record(
                    flow, "visual",
                    f"{side.name}: a child finishing without a reply renders the 'RLM child status' terminal-notice row in the parent transcript",
                    gap=False,
                )
            else:
                self.record(
                    flow, "visual",
                    f"{side.name}: the completed child produced no 'RLM child status' terminal-notice row",
                    evidence=side.root / flow / "04-child-status-settled.txt",
                    lane=FLOW_LANES[flow],
                )
            # The scoped agents view: alt+a focuses the summary line,
            # confirm opens the child list.
            self.tui_send(view, "M-a", enter=False)
            time.sleep(1.0)
            focused = B.tmux_capture(view)
            side.evidence(flow, "05-summary-focused.txt", focused)
            B.tmux_send(view, "Enter", enter=False)
            scoped = B.tmux_wait_text(view, child_name, timeout=60)
            side.evidence(flow, "06-scoped-agents.txt", scoped)
            settled3 = self.settle_frame(view, quiet_s=2.0, timeout=30)
            side.evidence(flow, "07-scoped-agents-settled.txt", settled3)
            frames[side.name]["scoped-agents"] = settled3
            if child_name in settled3:
                self.record(
                    flow, "visual",
                    f"{side.name}: the scoped agents view lists the spawned child by name",
                    gap=False,
                )
            else:
                self.record(
                    flow, "visual",
                    f"{side.name}: the scoped agents view did not list the spawned child",
                    evidence=side.root / flow / "07-scoped-agents-settled.txt",
                    lane=FLOW_LANES[flow],
                )
            B.tmux_kill(view)
            self.copy_sessions(side, flow)
        for step in ("spawn", "child-status", "scoped-agents"):
            self.frame_diff(
                flow, step,
                {name: frames[name].get(step, "") for name in ("ts", "rust")},
                self.normalize_transcript_frame,
            )

    def f21_worker_recovery(self) -> None:
        """Worker crash recovery: SIGKILL the session's live worker process
        (pid from the daemon's own session summary), then keep using the
        session through the attached TUI. The supervisor must respawn the
        worker, restore the transcript, and complete the next turn — the
        user-visible invariant is that the session survives its worker's
        death. Frame diff TS vs Rust at the post-kill and post-recovery key
        moments."""
        flow = "f21_worker_recovery"
        reply = "f21 recovery fixture reply"
        followup = "f21 post-recovery fixture reply"
        frames: dict[str, dict[str, str]] = {"ts": {}, "rust": {}}
        for side in (self.sides["ts"], self.sides["rust"]):
            self.suppress_first_run_notices(side)
            made = self.create_session(side, flow, "battery-f21", "c21")
            if not made:
                continue
            wire, session_id = made
            # The daemon's own session summary names the worker pid.
            state = wire.request(
                "g21", {"type": "get_state", "activeSessionId": session_id}, timeout=60
            )
            side.evidence_json(flow, "00-state.json", state)
            summary = state.get("data") or {}
            worker_pid = summary.get("workerPid")
            worker_state = summary.get("workerState")
            if state.get("success") is True and isinstance(worker_pid, int):
                self.record(
                    flow, "protocol",
                    f"{side.name}: the session summary exposes the live worker pid (workerState: {worker_state})",
                    gap=False,
                )
            else:
                self.record(
                    flow, "protocol",
                    f"{side.name}: the session summary exposes no live worker pid",
                    evidence=side.root / flow / "00-state.json",
                    lane=FLOW_LANES[flow],
                )
            # Seed turn: durable content the recovered worker must restore.
            side.mock.set_responses([{"text": reply}])
            wire.request(
                "p21s", {"type": "prompt_and_wait", "activeSessionId": session_id, "message": "f21 seed turn"}, timeout=240
            )
            wire.close()
            self.settle_mock(side)
            attached = self.attach_tui(side, flow, session_id, ready_marker="f21 seed turn")
            if not attached:
                self.record(flow, "visual", f"{side.name}: attached TUI never rendered the seeded transcript")
                continue
            tui, ready = attached
            side.evidence(flow, "01-attached.txt", ready)
            # Key moment 1: the worker dies underneath the attached TUI. The
            # visible frame must keep the transcript; recovery is invisible
            # until the next command needs the worker.
            killed = False
            if isinstance(worker_pid, int):
                try:
                    os.kill(worker_pid, signal.SIGKILL)
                    killed = True
                except OSError as error:
                    self.record(
                        flow, "behavior",
                        f"{side.name}: could not kill the reported worker pid {worker_pid}: {error}",
                        evidence=side.root / flow / "00-state.json",
                    )
            if not killed:
                self.record(
                    flow, "behavior",
                    f"{side.name}: no live worker pid to kill; recovery cannot be exercised",
                    evidence=side.root / flow / "00-state.json",
                    lane=FLOW_LANES[flow],
                )
                B.tmux_kill(tui)
                continue
            time.sleep(2.0)
            post_kill = B.tmux_capture(tui)
            side.evidence(flow, "02-post-kill.txt", post_kill)
            settled = self.settle_frame(tui, quiet_s=2.0, timeout=30)
            side.evidence(flow, "03-post-kill-settled.txt", settled)
            frames[side.name]["post-kill"] = settled
            if "f21 seed turn" in settled:
                self.record(
                    flow, "behavior",
                    f"{side.name}: the attached TUI keeps the transcript after the worker process is killed",
                    gap=False,
                )
            else:
                self.record(
                    flow, "behavior",
                    f"{side.name}: the attached TUI lost the transcript after the worker process was killed",
                    evidence=side.root / flow / "03-post-kill-settled.txt",
                    lane=FLOW_LANES[flow],
                )
            post_kill_wire = B.Wire(side.daemon_socket)
            post_kill_state = post_kill_wire.request(
                "g21k", {"type": "get_state", "activeSessionId": session_id}, timeout=120
            )
            post_kill_wire.close()
            side.evidence_json(flow, "04-post-kill-state.json", post_kill_state)
            # Key moment 2: the next prompt goes through. The daemon respawns
            # the worker (journal + session file restore), the turn completes,
            # and the frame shows the recovered transcript plus the new reply.
            self.settle_mock(side)
            side.mock.set_responses([{"text": followup}])
            self.tui_send(tui, "f21 keep working after the crash")
            recovered = B.tmux_wait_text(tui, followup, timeout=240)
            side.evidence(flow, "05-recovered.txt", recovered)
            settled2 = self.settle_frame(tui, quiet_s=3.0, timeout=90)
            side.evidence(flow, "06-recovered-settled.txt", settled2)
            frames[side.name]["recovered"] = settled2
            if "f21 seed turn" in settled2 and followup in settled2:
                self.record(
                    flow, "behavior",
                    f"{side.name}: after the worker is killed the session recovers — the next turn completes with the transcript intact",
                    gap=False,
                )
            else:
                self.record(
                    flow, "behavior",
                    f"{side.name}: the session did not survive its worker's death (no post-recovery turn)",
                    evidence=side.root / flow / "06-recovered-settled.txt",
                    lane=FLOW_LANES[flow],
                )
            # Wire-level: the recovered session must be served by a new,
            # ready worker process.
            recovered_wire = B.Wire(side.daemon_socket)
            recovered_state = recovered_wire.request(
                "g21r", {"type": "get_state", "activeSessionId": session_id}, timeout=120
            )
            recovered_wire.close()
            side.evidence_json(flow, "07-post-recovery-state.json", recovered_state)
            rsummary = recovered_state.get("data") or {}
            if (
                recovered_state.get("success") is True
                and rsummary.get("workerState") == "ready"
                and rsummary.get("workerPid") not in (None, worker_pid)
            ):
                self.record(
                    flow, "protocol",
                    f"{side.name}: recovery respawned the worker (pid {worker_pid} -> {rsummary.get('workerPid')}, workerState ready)",
                    gap=False,
                )
            else:
                self.record(
                    flow, "protocol",
                    f"{side.name}: recovery did not produce a new ready worker (workerState: {rsummary.get('workerState')}, workerPid: {rsummary.get('workerPid')})",
                    evidence=side.root / flow / "07-post-recovery-state.json",
                    lane=FLOW_LANES[flow],
                )
            B.tmux_kill(tui)
            self.copy_sessions(side, flow)
        for step in ("post-kill", "recovered"):
            self.frame_diff(
                flow, step,
                {name: frames[name].get(step, "") for name in ("ts", "rust")},
                self.normalize_transcript_frame,
            )

    def perf_onboard(self, side: B.Side) -> None:
        """Settle first-run dialogs (the TS trace notice) once before measuring,
        so measured launches settle straight into the main screen. The settle
        launch is the same flagged invocation `P.launch_argv` every measured
        launch uses: the Rust onboarding gate fires only with explicit
        provider/model flags (TS resolves the startup model from settings),
        so a flagless settle run would answer nothing and the notice would
        surface (and stall) the measured launches instead."""
        socket = side.root / "perf" / "onboard.sock"
        socket.parent.mkdir(parents=True, exist_ok=True)
        if socket.exists():
            socket.unlink()
        session = f"{self.runid}-perf-onboard-{side.name}"
        argv = P.launch_argv(side, socket)
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
        expected = [f for f in gaps if f.get("expectedFail")]
        unexpected = [f for f in gaps if not f.get("expectedFail")]
        lines.append(
            f"{len(unexpected)} gaps, {len(expected)} EXPECTED-FAIL (known gaps, owner lanes), {len(oks)} parity checks passed."
        )
        if expected:
            lines.append("")
            lines.append("## Known gaps (EXPECTED-FAIL — evidence for the owning fix lanes)")
            lines.append("")
            for finding in expected:
                ev = f" — evidence: {finding['evidence']}" if finding.get("evidence") else ""
                lines.append(
                    f"- [{finding['flow']}/{finding['category']}] EXPECTED-FAIL (lane: {finding['expectedFail']}): {finding['summary']}{ev}"
                )
        lines.append("")
        current = None
        for finding in unexpected:
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
        order = {
            f: getattr(self, f)
            for f in ALL_FLOWS + HEAVY_FLOWS
            if callable(getattr(self, f, None))
        }
        try:
            for flow in self.flows:
                if flow not in order:
                    print(f"== {flow} (skipped: flow not implemented in this build)", flush=True)
                    continue
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
        if flow not in ALL_FLOWS and flow not in HEAVY_FLOWS:
            parser.error(f"unknown flow {flow}; valid: {ALL_FLOWS + HEAVY_FLOWS}")
    battery = Battery(Path(args.runs_root), args.ts_bin, args.rust_bin, flows)
    return battery.run()


if __name__ == "__main__":
    raise SystemExit(main())
