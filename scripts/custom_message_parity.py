#!/usr/bin/env python3
"""Custom-message decoration parity verifier: frame-diff the Rust TUI
against the installed TS prime-agent binary rendering the SAME session
transcript containing every decorated custom-message row:

  - a received agent message (diamond + participant + body),
  - a heartbeat prompt (pulse + schedule),
  - a goal-context continuation row,
  - a restored-python-kernel row,
  - an RLM child terminal notice,
  - a background-shell completion,
  - a compaction outcome,
  - a refinement outcome,
  - an autonomous-status row (the generic custom box).

The session JSONL is assembled from real captured rows and resumed in the
TS binary (`prime-agent -r <path>`) and replayed in the Rust TUI
(`pa-tui-replay <path>`), both live in tmux at 120x36. States: the idle
transcript (collapsed) and the expanded view (Ctrl+O twice, where the
agent-message bodies and shell-completion output open up). Frames are
normalized for volatile content and diffed; the exit code is non-zero when
any state differs.

tmux rules: default socket only (`env -u TMUX`), cmparity-* session names,
no kill-server; sessions are killed individually at the end.
"""

import argparse
import difflib
import glob
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "battery"))
import batterylib  # noqa: E402  (the shared daemon-reap sweep)
import ts_identity  # noqa: E402  (the shared PATH-binary identity guard)

SIZES = [("120", "36")]

# The transcript skeleton (entries copied from real captured sessions so
# both binaries parse byte-identical payloads). Timestamps are re-stamped
# sequentially; the parent chain is linear.
CUSTOM_ROWS = [
    ("agent_message", {"display": True, "details": {
        "id": "agentmsg_cmparity",
        "message": "Decorations parity: the received row renders with the diamond, label, and participant.",
        "from": {"sessionName": "model-probe", "sessionId": "sess-probe", "activeSessionId": "aaa111", "runtimeKind": "subagent"},
        "fromRelationship": "child",
    }}),
    ("heartbeat_prompt", {"display": True, "details": {
        "jobId": "job-1", "schedule": "every 10m", "status": "running", "runCount": 3,
    }}),
    ("goal_context", {"display": True, "details": {
        "kind": "continuation", "goalId": "goal-1",
        "objective": "Keep the parity harnesses green.",
        "status": "active", "continuationsUsed": 1,
    }}),
    ("ipython_state_restored", {"display": True, "details": {"restored": True}}),
    ("rlm_child_terminal_notice", {"display": True, "details": {
        "kind": "completed_without_reply", "childId": "sub-1", "sessionName": "lane-decorations",
    }}),
    ("async_bash_completion", {"display": True, "details": {"pid": 4371, "command": "seq 1 3", "exitCode": 0}}),
    ("compaction_outcome", {"display": True, "details": {"reason": "threshold", "outcome": "skipped"}}),
    ("refinement_outcome", {"display": True, "details": {
        "refinementId": "refine_cmparity", "summary": "Create one local memory.", "scope": "local",
        "edits": [{
            "action": "create", "kind": "memory", "id": "cmparity-memory", "applied": True,
            "title": "Parity memory", "content": "The harness stays green.",
            "after": {"id": "cmparity-memory", "kind": "memory", "title": "Parity memory",
                      "content": "The harness stays green.", "scope": "local"},
        }],
    }}),
    ("autonomous_status", {"display": True, "details": {"enabled": False}}),
]

MARKER_TEXT = "decorations parity transcript complete"


def newest_assistant_message(sessions):
    """A real assistant message envelope (api/provider/usage fields) so the
    synthetic replies deserialize in BOTH loaders: the Rust loader degrades
    a malformed message to an unknown entry that renders nothing, which
    would silently drop the rows the verifier waits for. The SMALLEST
    captured envelope wins: replaying a large one ballooned the TS daemon
    to ~12GB RSS on this fixture. The provider/model fields are rewritten
    to a model the TS daemon can restore, so the resumed session does not
    render a model-restore warning the Rust replay would not show."""
    best = None
    for path in reversed(sessions):
        with open(path, encoding="utf-8") as f:
            for line in f:
                try:
                    row = json.loads(line)
                except ValueError:
                    continue
                message = row.get("message") if row.get("type") == "message" else None
                if (
                    isinstance(message, dict)
                    and message.get("role") == "assistant"
                    and isinstance(message.get("content"), list)
                    and any(
                        isinstance(block, dict) and block.get("type") == "text"
                        and len(block.get("text", "")) < 200
                        for block in message["content"]
                    )
                    and (best is None or len(line) < best[0])
                ):
                    best = (len(line), message)
    if best is None:
        raise SystemExit("no assistant message found in captured sessions")
    return json.loads(json.dumps(best[1]))


def session_paths():
    return sorted(
        glob.glob(os.path.expanduser("~/.prime/agent/sessions/**/*.jsonl"), recursive=True),
        key=os.path.getmtime,
    )


def assistant_with(template, text, ms):
    """Clone the captured assistant envelope around one text block. The
    provider/model point at the model the TS daemon restores by default, so
    the resume never renders a model-restore warning."""
    message = json.loads(json.dumps(template))
    message["content"] = [{"type": "text", "text": text}]
    message["provider"] = "prime-inference"
    message["model"] = "z-ai/glm-5.3"
    message["timestamp"] = ms
    return message


def build_session(path, source_header, assistant_template, cwd):
    """Assemble the parity session: one user turn, one assistant reply,
    then every custom row, then a closing assistant reply. The header cwd
    must match the sandbox cwd: the TS resume rejects sessions that belong
    to a different project."""
    # A synthetic root-session header: the TS resume follows a cloned real
    # session id (and its parentSession chain) into the live multi-GB
    # sessions on this box, which ballooned the resume client to ~12GB
    # RSS and wedged the render. Only the wire shape is preserved.
    header = {
        "type": "session",
        "version": source_header.get("version", 3),
        "id": "cmparity-session",
        "cwd": cwd,
        "timestamp": source_header.get("timestamp", "2026-09-18T00:00:00.000Z"),
    }
    entries = [header]
    base_ms = 1_700_000_000_000
    def entry(type_, fields, ms):
        row = {"type": type_}
        row.update(fields)
        # Unique sequential ids: the TS resume follows the parentId chain,
        # and a self-referential id (an earlier bug produced `e0000` for
        # every row) made the TS loader loop until the process ballooned
        # to ~12GB RSS.
        row["id"] = f"e{ms - 1_700_000_000_000:04d}"
        row["parentId"] = entries[-1]["id"]
        row["timestamp"] = time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime(ms / 1000))
        return row
    entries.append(entry("message", {
        "message": {"role": "user", "content": "Run the decoration checks.", "timestamp": base_ms},
    }, base_ms))
    base_ms += 1
    entries.append(entry("message", {
        "message": assistant_with(
            assistant_template, "Rows below cover every decorated custom message.", base_ms
        ),
    }, base_ms))
    for custom_type, extra in CUSTOM_ROWS:
        base_ms += 1
        content = {
            "agent_message": "[agent-message from child:model-probe]\n\nDecorations parity: the received row renders with the diamond, label, and participant.",
            "heartbeat_prompt": "[heartbeat: every 10m run#3]\n\nContinue the mission.",
            "goal_context": "[goal: continuation]\n\nContinue the goal.",
            "ipython_state_restored": "[python-state-restored]\n\nKernel state revived.",
            "rlm_child_terminal_notice": "[child-exited: no-reply child:lane-decorations]",
            "async_bash_completion": "[bash-done pid:4371 exit:0]\n\nCommand: \"seq 1 3\"",
            "compaction_outcome": "Compaction skipped: below the token threshold.",
            "refinement_outcome": "Refinement complete: Create one local memory.",
            "autonomous_status": "[autonomous-status: off]\n\nContinuations: 0/0. Turns: 0/0.",
        }[custom_type]
        fields = {"customType": custom_type, "content": content}
        fields.update(extra)
        entries.append(entry("custom_message", fields, base_ms))
    base_ms += 1
    entries.append(entry("message", {
        "message": assistant_with(assistant_template, MARKER_TEXT, base_ms),
    }, base_ms))
    with open(path, "w") as f:
        for row in entries:
            f.write(json.dumps(row) + "\n")
    return path


def newest_session_header():
    """A real session header (current on-disk format) for the synthetic file."""
    sessions = session_paths()
    for path in reversed(sessions):
        with open(path, encoding="utf-8") as f:
            first = f.readline()
        try:
            header = json.loads(first)
        except ValueError:
            continue
        if header.get("type") == "session":
            return header
    raise SystemExit("no captured session header found under ~/.prime/agent/sessions")


def tmux(*args, check=True):
    result = subprocess.run(["env", "-u", "TMUX", "tmux", *args], capture_output=True, text=True)
    if check and result.returncode != 0:
        raise RuntimeError(f"tmux {' '.join(args)} failed: {result.stderr}")
    return result.stdout


def capture(session):
    return tmux("capture-pane", "-e", "-p", "-t", session)


def capture_plain(session):
    return tmux("capture-pane", "-p", "-t", session)


def normalize(frame, root):
    frame = frame.replace(root, "<SANDBOX>")
    frame = re.sub(r"v\d+\.\d+\.\d+", "vX.X.X", frame)
    frame = re.sub(r"\b[0-9a-f]{12}\b", "<SID>", frame)
    frame = re.sub(r"\b\d+(\.\d+)?(ms|s)\b", "<T>", frame)
    frame = re.sub(r"\d+(\.\d+)?[kM]? \(\d+%\)", "<TOK> (<PCT>)", frame)
    frame = re.sub(r"[\u2193\u2191] [\d.kM]+ tokens", "<DIR> <TOK> tokens", frame)
    spinners = "".join("\u280b\u2819\u2839\u2838\u283c\u2834\u2826\u2827\u2807\u280f")
    frame = re.sub("[" + spinners + "]", "<SPIN>", frame)
    pulses = "".join("\u25f4\u25f7\u25f6\u25f5\u25cb\u25f8\u25fb\u25fc")
    frame = re.sub("[" + pulses + "]", "<PULSE>", frame)
    frame = re.sub("\x1b\[39m\n", "\n", frame)
    frame = re.sub("\n\x1b\[39m(?= )", "\n", frame)
    frame = re.sub("\x1b\[49m\n", "\n", frame)
    frame = re.sub("\n\x1b\[49m(?= )", "\n", frame)
    frame = re.sub(
        r" +((?:\x1b\[[0-9;]*m)*(?:faux-1 \u00b7 )?<TOK> \(<PCT>\)\s*)$",
        r" <TRAY-RIGHT>\1",
        frame,
        flags=re.MULTILINE,
    )
    # The Rust replay harness renders the transcript without the TS
    # client chrome: the top info bar (`cwd ... $0.00`) and the bottom nav
    # bar (`← manage ...`). Drop those rows and trim the viewport blank
    # rows at the frame edges so the diff covers the transcript only.
    kept = []
    for line in frame.split("\n"):
        if "cwd" in line and "$0.00" in line:
            continue
        # The nav arrow and the manage label are separated by ANSI color
        # codes in the capture, so match them independently.
        if "←" in line and "manage" in line:
            continue
        kept.append(line)
    while kept and not kept[0].strip():
        kept.pop(0)
    while kept and not kept[-1].strip():
        kept.pop()
    return "\n".join(kept)


def diff_lines(left, right):
    return "\n".join(
        difflib.unified_diff(left.split("\n"), right.split("\n"), fromfile="ts", tofile="rust", lineterm="", n=1)
    )


def wait_for(session, needle, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if needle in capture_plain(session):
            return
        time.sleep(0.3)
    raise TimeoutError(f"session {session} never showed {needle!r}")


def find_runtime_package_dir():
    releases = os.path.expanduser("~/.local/share/prime-agent/releases")
    candidates = [
        entry
        for entry in sorted(os.listdir(releases))
        if os.path.isdir(os.path.join(releases, entry, "prime-agent-runtime"))
    ]
    if not candidates:
        raise SystemExit("no release with prime-agent-runtime/ under " + releases)
    return os.path.join(releases, candidates[-1])


def run_ts(session_path, sandbox, size, out_dir):
    session = f"cmparity-ts-{size[0]}x{size[1]}"
    tmux("kill-session", "-t", session, check=False)
    tmux("new-session", "-d", "-s", session, "-x", size[0], "-y", size[1], "-c", sandbox["cwd"])
    env = (
        f"HOME={sandbox['home']} "
        f"TMPDIR={sandbox['tmp']} "
        f"PRIME_AGENT_CODING_AGENT_DIR={sandbox['agent']} "
        "PRIME_AGENT_DISABLE_ANALYTICS=1"
    )
    # A dedicated daemon socket keeps the sandbox main daemon off the
    # shared socket path; the isolated TMPDIR keeps its supervisor off
    # the shared box root too, so the cleanup reap can sweep both.
    command = (
        f"{env} prime-agent --daemon-socket {sandbox['agent']}/daemon.sock "
        f"--offline -r {session_path}"
    )
    tmux("send-keys", "-t", session, command, "Enter")
    wait_for(session, MARKER_TEXT, timeout=60)
    time.sleep(1.5)
    frames = {"a_collapsed": capture(session)}
    # Ctrl+O twice: all mode (expanded bodies and shell output).
    tmux("send-keys", "-t", session, "C-o")
    tmux("send-keys", "-t", session, "C-o")
    time.sleep(1.5)
    frames["b_expanded"] = capture(session)
    tmux("kill-session", "-t", session, check=False)
    for state, frame in frames.items():
        with open(os.path.join(out_dir, f"ts-{state}-{size[0]}x{size[1]}.txt"), "w") as f:
            f.write(frame)
    return frames


def run_rust(session_path, sandbox, size, out_dir):
    session = f"cmparity-rust-{size[0]}x{size[1]}"
    tmux("kill-session", "-t", session, check=False)
    tmux("new-session", "-d", "-s", session, "-x", size[0], "-y", size[1], "-c", sandbox["cwd"])
    rust = os.environ.get(
        "PA_RUST_REPLAY",
        os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "pa-tui-replay"),
    )
    env = f"HOME={sandbox['home']}"
    command = f"{env} {rust} {session_path}"
    tmux("send-keys", "-t", session, command, "Enter")
    wait_for(session, MARKER_TEXT, timeout=60)
    time.sleep(1.5)
    frames = {"a_collapsed": capture(session)}
    tmux("send-keys", "-t", session, "C-o")
    tmux("send-keys", "-t", session, "C-o")
    time.sleep(1.5)
    frames["b_expanded"] = capture(session)
    tmux("kill-session", "-t", session, check=False)
    for state, frame in frames.items():
        with open(os.path.join(out_dir, f"rust-{state}-{size[0]}x{size[1]}.txt"), "w") as f:
            f.write(frame)
    return frames


def prepare_sandbox(base):
    cwd = os.path.join(base, "cwd")
    os.makedirs(cwd, exist_ok=True)
    sandboxes = {}
    for binary in ("ts", "rust"):
        home = os.path.join(base, binary, "home")
        agent = os.path.join(base, binary, "agent")
        tmp = os.path.join(base, binary, "tmp")
        os.makedirs(home, exist_ok=True)
        os.makedirs(tmp, exist_ok=True)
        os.makedirs(os.path.join(agent, "sessions"), exist_ok=True)
        with open(os.path.join(agent, "settings.json"), "w") as f:
            json.dump({"onboardingCompleted": True}, f)
        # The isolated TMPDIR keeps the TS supervisor's socket (the
        # default daemon-socket dir) off the shared box root, so the
        # cleanup reap can sweep the side's daemons by path alone.
        sandboxes[binary] = {"home": home, "agent": agent, "cwd": cwd, "tmp": tmp}
    return sandboxes


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sizes", default=",".join(f"{w}x{h}" for w, h in SIZES))
    parser.add_argument("--keep", action="store_true")
    parser.add_argument("--out", default=None)
    parser.add_argument("--only", default=None, choices=["ts", "rust"])
    args = parser.parse_args()
    sizes = [tuple(entry.split("x")) for entry in args.sizes.split(",")]

    # Fail fast before any launch: a non-TS `prime-agent` on PATH plays a
    # Rust build as the "ts" side and reports false divergences.
    if args.only in (None, "ts"):
        ts_identity.assert_ts_side_is_the_ts_product()

    base = tempfile.mkdtemp(prefix="custom-message-parity-")
    out_dir = args.out or tempfile.mkdtemp(prefix="custom-message-captures-")
    sandboxes = prepare_sandbox(base)
    sessions = session_paths()
    session_path = build_session(
        os.path.join(base, "parity-session.jsonl"),
        newest_session_header(),
        newest_assistant_message(sessions),
        os.path.join(base, "cwd"),
    )
    print(f"session: {session_path}")
    failures = []
    try:
        if args.only:
            runner = run_ts if args.only == "ts" else run_rust
            frames = runner(session_path, sandboxes[args.only], sizes[0], out_dir)
            print(f"captures for {args.only} in {out_dir}: {sorted(frames)}")
            return 0
        for size in sizes:
            ts_frames = run_ts(session_path, sandboxes["ts"], size, out_dir)
            rust_frames = run_rust(session_path, sandboxes["rust"], size, out_dir)
            for state in ("a_collapsed", "b_expanded"):
                ts_norm = normalize(ts_frames[state], base)
                rust_norm = normalize(rust_frames[state], base)
                name = f"{state}-{size[0]}x{size[1]}"
                if ts_norm == rust_norm:
                    print(f"PASS {name}")
                else:
                    print(f"FAIL {name}")
                    report = os.path.join(out_dir, f"diff-{name}.txt")
                    with open(report, "w") as f:
                        f.write(diff_lines(ts_norm, rust_norm))
                    print(f"  diff: {report}")
                    failures.append(name)
    finally:
        # Rmtree alone leaks the scenario daemons (a killed TUI pane does
        # not take its detached daemon/supervisor pair down; #223): sweep
        # every daemon this run spawned before deleting the sandbox.
        batterylib.reap_daemons(needles=[base], cwd_roots=[base])
        if not args.keep:
            shutil.rmtree(base, ignore_errors=True)
    if failures:
        print(f"{len(failures)} state(s) differ; captures in {out_dir}")
        return 1
    print(f"all states match; captures in {out_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
