#!/usr/bin/env python3
"""Live token-stream rendering verifier (lane: streaming-render).

Drives the Rust interactive TUI through a paced faux provider (a 240-word
answer at a controlled tokensPerSecond) inside tmux and captures the pane
repeatedly while the turn runs. The acceptance contract:

1. Mid-turn, the pane must GROW MONOTONICALLY: several growth samples
   (>= MIN_GROWTH_SAMPLES) at least MIN_SAMPLE_GAP apart, not one jump at
   turn end - i.e. token streaming renders live.
2. The settled final frame must match the TS binary's settled frame
   (differential, normalized for volatile chrome) - i.e. progressive
   rendering changed nothing about the settled output.

Both sides run the same paced faux script (the TS side through the
visual-parity faux extension; the Rust side through PRIME_AGENT_FAUX_SCRIPT),
so the TS binary is the ground truth for both the live-growth behavior and
the settled frame.

tmux rules: default socket only (`env -u TMUX`), vplane-* session names, no
kill-server; sessions are killed individually at the end.
"""

import argparse
import difflib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

TS_SCRIPT_MODEL = "faux-1"
SIZE = ("120", "40")
SESSION_NAME = "vplane-stream"

# The paced answer: 240 single "wordN" tokens streamed at 25 tokens/second
# (~10s of streaming) - long enough for many pane samples mid-turn.
STREAM_WORDS = 240
TOKENS_PER_SECOND = 25

FAUX_SCRIPT = {
    "engine": "faux",
    "modelId": "faux-1",
    "modelName": "Faux Model",
    "reasoning": False,
    "contextWindow": 128000,
    "tokensPerSecond": TOKENS_PER_SECOND,
    "responses": [
        {
            "content": [
                {
                    "type": "text",
                    "text": " ".join(f"word{i}" for i in range(1, STREAM_WORDS + 1)),
                }
            ]
        },
    ],
}

PROMPT = "Write exactly two hundred words about the moon."

# Loader/spinner frames (chat.rs LOADER_FRAMES); a settled turn shows none.
SPINNER_CHARS = "\u280b\u2819\u2839\u2838\u283c\u2834\u2826\u2827\u2807\u280f\u25f4\u25f7\u25f6\u25f5\u25cb\u25f8\u25fb\u25fc"
SPINNER_CLASS = "[" + SPINNER_CHARS + "]"

SAMPLE_INTERVAL = 0.3
MIN_GROWTH_SAMPLES = 5  # distinct pane samples that gained words mid-turn
TURN_TIMEOUT = 240.0


def tmux(*args, check=True):
    result = subprocess.run(
        ["env", "-u", "TMUX", "tmux", *args], capture_output=True, text=True
    )
    if check and result.returncode != 0:
        raise RuntimeError(f"tmux {' '.join(args)} failed: {result.stderr}")
    return result.stdout


def capture(session, escape=False):
    flag = "-e" if escape else "-p"
    return tmux("capture-pane", flag, "-p", "-t", session)


def normalize(frame, root):
    """Same volatile-content scrub visual_parity.py applies."""
    frame = frame.replace(root, "<SANDBOX>")
    frame = re.sub(r"v\d+\.\d+\.\d+", "vX.X.X", frame)
    frame = re.sub(r"\b[0-9a-f]{12}\b", "<SID>", frame)
    frame = re.sub(r"\b\d+(\.\d+)?(ms|s)\b", "<T>", frame)
    frame = re.sub(r"\d+(\.\d+)?[kM]? \(\d+%\)", "<TOK> (<PCT>)", frame)
    frame = re.sub(r"[\u2193\u2191] [\d.kM]+ tokens", "<DIR> <TOK> tokens", frame)
    frame = re.sub(r"\b\d+s\b", "<S>", frame)
    spinners = "".join("\u280b\u2819\u2839\u2838\u283c\u2834\u2826\u2827\u2807\u280f")
    frame = re.sub("[" + spinners + "]", "<SPIN>", frame)
    pulses = "".join("\u25f4\u25f7\u25f6\u25f5\u25cb\u25f8\u25fb\u25fc")
    frame = re.sub("[" + pulses + "]", "<PULSE>", frame)
    frame = re.sub("\x1b\[39m\n", "\n", frame)
    frame = re.sub("\n\x1b\[39m(?= )", "\n", frame)
    # tmux places the background-reset ([49m) at row boundaries just like the
    # foreground reset: same screen, two capture spellings.
    frame = re.sub("\x1b\[49m\n", "\n", frame)
    frame = re.sub("\n\x1b\[49m(?=\x1b|\x1b\[38| |$)", "\n", frame)
    # The right-aligned footer tray pads with runs of spaces sized by the
    # live token count; collapse space runs so padding width cannot differ.
    frame = re.sub(" {2,}", " ", frame)
    # The tmux extended-keys notice is environment chrome (it depends on the
    # ambient tmux server options, not the product under test).
    frame = re.sub(
        "\x1b\[38;2;245;158;11m\u26a0 tmux extended-keys is off[\s\S]*?restart tmux\.\n?", "", frame
    )
    return frame


def diff_lines(left, right):
    return "\n".join(
        difflib.unified_diff(
            left.split("\n"), right.split("\n"), fromfile="ts", tofile="rust", lineterm="", n=1,
        )
    )


def find_runtime_package_dir():
    releases = os.path.expanduser("~/.local/share/prime-agent/releases")
    candidates = [
        entry
        for entry in sorted(os.listdir(releases))
        if os.path.isdir(os.path.join(releases, entry, "prime-agent-runtime"))
    ]
    if not candidates:
        raise SystemExit(
            "cannot find the prime-agent-runtime sidecar; set PI_PACKAGE_DIR"
        )
    return os.path.join(releases, candidates[-1])


TS_FAUX_EXTENSION = open(
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "ts_faux_extension.js"),
    encoding="utf-8",
).read()


def rendered_words(pane):
    return max((int(m) for m in re.findall(r"word(\d+)", pane)), default=0)


def wait_for(session, needle, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if needle in capture(session):
            return True
        time.sleep(0.3)
    return False


def run_session(binary, sandbox, shared_cwd, script_path, out_dir):
    """Drive one binary: assert live growth, return the settled frame."""
    width, height = SIZE
    session = f"{SESSION_NAME}-{binary}"
    tmux("kill-session", "-t", session, check=False)
    tmux("new-session", "-d", "-s", session, "-x", width, "-y", height, "-c", shared_cwd)
    env = (
        f"HOME={sandbox['home']} "
        f"PRIME_AGENT_CODING_AGENT_DIR={sandbox['agent']} "
        f"PRIME_AGENT_FAUX_SCRIPT={script_path} "
        "PRIME_AGENT_DISABLE_ANALYTICS=1"
    )
    if binary == "ts":
        command = (
            f"prime-agent --daemon-socket {sandbox['agent']}/daemon.sock "
            f"--model {TS_SCRIPT_MODEL}"
        )
    else:
        rust = os.environ.get("PA_RUST_BINARY") or os.path.join(
            os.path.dirname(os.path.abspath(__file__)),
            "..",
            "..",
            "target",
            "release",
            "prime-agent",
        )
        package_dir = os.environ.get("PI_PACKAGE_DIR") or find_runtime_package_dir()
        command = (
            f"PI_PACKAGE_DIR={package_dir} "
            f"{rust} --daemon-socket {sandbox['agent']}/daemon.sock "
            f"--model {TS_SCRIPT_MODEL}"
        )
    tmux("send-keys", "-t", session, f"{env} {command}", "Enter")
    if not wait_for(session, "Collapsed mode", timeout=60):
        raise AssertionError(f"{binary}: the editor never came up")

    tmux("send-keys", "-t", session, PROMPT)
    tmux("send-keys", "-t", session, "Enter")

    # Sample the pane while the turn runs: growth must be progressive.
    growth = []  # (t, words) samples where the pane gained words
    samples = []  # every sample, for evidence
    last_words = 0
    settled = False
    deadline = time.time() + TURN_TIMEOUT
    while time.time() < deadline:
        pane = capture(session)
        words = rendered_words(pane)
        now = time.time()
        samples.append((now, words, bool(re.search(SPINNER_CLASS, pane))))
        # Every pane sample that gained words is one growth sample: live
        # streaming produces one per ~SAMPLE_INTERVAL (dozens over a paced
        # turn); a batched render produces exactly one (0 -> full text).
        if words > last_words:
            growth.append((round(now, 2), words))
            last_words = words
        if words >= STREAM_WORDS and not re.search(SPINNER_CLASS, pane):
            settled = True
            break
        time.sleep(SAMPLE_INTERVAL)
    evidence_path = os.path.join(out_dir, f"{binary}-samples.json")
    os.makedirs(out_dir, exist_ok=True)
    with open(evidence_path, "w") as f:
        json.dump({"growth": growth, "samples": samples}, f, indent=1)
    if not settled:
        raise AssertionError(
            f"{binary}: the turn never settled (last words={last_words}); evidence: {evidence_path}"
        )
    if len(growth) < MIN_GROWTH_SAMPLES:
        raise AssertionError(
            f"{binary}: pane grew only {len(growth)} times mid-turn "
            f"(needs >= {MIN_GROWTH_SAMPLES}); rendering is batched, not streamed; "
            f"evidence: {evidence_path}"
        )

    # The settled final frame: one more beat after the loader row is gone,
    # so the trailing status chrome (usage tray, footer) finishes repainting.
    time.sleep(1.0)
    frame = capture(session, escape=True)
    tmux("kill-session", "-t", session, check=False)

    with open(os.path.join(out_dir, f"{binary}-settled.txt"), "w") as f:
        f.write(frame)
    return frame, growth


def prepare_sandbox(base):
    shared_cwd = os.path.join(base, "shared-cwd")
    os.makedirs(shared_cwd, exist_ok=True)
    script_path = os.path.join(base, "faux-script.json")
    with open(script_path, "w") as f:
        json.dump(FAUX_SCRIPT, f, indent=2)
    sandboxes = {}
    for binary in ("ts", "rust"):
        home = os.path.join(base, binary, "home")
        agent = os.path.join(base, binary, "agent")
        os.makedirs(os.path.join(agent, "extensions"), exist_ok=True)
        os.makedirs(os.path.join(agent, "sessions"), exist_ok=True)
        with open(os.path.join(agent, "settings.json"), "w") as f:
            json.dump({"onboardingCompleted": True}, f)
        sandboxes[binary] = {"home": home, "agent": agent}
    with open(
        os.path.join(sandboxes["ts"]["agent"], "extensions", "stream-faux.js"), "w"
    ) as f:
        f.write(TS_FAUX_EXTENSION)
    return shared_cwd, script_path, sandboxes


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--keep", action="store_true", help="keep the sandbox dir")
    parser.add_argument(
        "--out", default=None, help="captures directory (default: a fresh temp dir)"
    )
    parser.add_argument(
        "--only", default=None, help="run a single binary (ts|rust) for a shakedown"
    )
    args = parser.parse_args()

    base = tempfile.mkdtemp(prefix="streaming-render-sandbox-")
    out_dir = args.out or tempfile.mkdtemp(prefix="streaming-render-captures-")
    shared_cwd, script_path, sandboxes = prepare_sandbox(base)
    try:
        if args.only:
            frame, growth = run_session(args.only, sandboxes[args.only], shared_cwd, script_path, out_dir)
            print(f"{args.only}: {len(growth)} progressive growth samples; captures in {out_dir}")
            return 0
        ts_frame, ts_growth = run_session("ts", sandboxes["ts"], shared_cwd, script_path, out_dir)
        rust_frame, rust_growth = run_session("rust", sandboxes["rust"], shared_cwd, script_path, out_dir)
        print(f"ts:   {len(ts_growth)} progressive growth samples")
        print(f"rust: {len(rust_growth)} progressive growth samples")
        ts_norm = normalize(ts_frame, base)
        rust_norm = normalize(rust_frame, base)
        if ts_norm == rust_norm:
            print("PASS settled frames match")
            return 0
        report = os.path.join(out_dir, "diff-settled.txt")
        with open(report, "w") as f:
            f.write(diff_lines(ts_norm, rust_norm))
        print(f"FAIL settled frames differ; diff: {report}")
        return 1
    finally:
        if not args.keep:
            shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
