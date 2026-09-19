#!/usr/bin/env python3
"""Follow-up-queue parity: frame-diff the queued-prompt strip against the
installed TS prime-agent binary in tmux.

Drives both binaries to the same defined state — a slow first turn with one
steering prompt (Enter) and one follow-up prompt (alt+enter) parked behind
it, then the drained idle after delivery — and compares the strip rows the
user sees (the dim `Steering:`/`Follow-up:` previews and the browse hint).
Exit code is non-zero when any state's visible strip rows differ.

Reuses the visual_parity faux-provider harness (tmux rules: default socket,
vplane-* session names, sessions killed individually).
"""

import argparse
import difflib
import json
import os
import re
import shutil
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import visual_parity as vp

# Turn 1 streams slowly (6 tokens/s over ~30 words) so the window stays open
# for the two parked submissions; turns 2/3 answer the queued prompts.
QUEUE_FAUX_SCRIPT = {
    "engine": "faux",
    "modelId": vp.TS_SCRIPT_MODEL,
    "modelName": "Faux Model",
    "reasoning": False,
    "contextWindow": 128000,
    "tokensPerSecond": 6,
    "responses": [
        {
            "content": [
                {
                    "type": "text",
                    "text": "The first turn streams slowly so the queue strip has time to render.",
                }
            ]
        },
        {"content": [{"type": "text", "text": "steering delivered"}]},
        {"content": [{"type": "text", "text": "follow-up delivered"}]},
    ],
}

FIRST_PROMPT = "tell me something slowly"
STEERING_PROMPT = "steering prompt text"
FOLLOW_UP_PROMPT = "follow-up prompt text"

# Strip rows both binaries must render (the visible queue surface).
STEERING_ROW = f"Steering: {STEERING_PROMPT}"
FOLLOW_UP_ROW = f"Follow-up: {FOLLOW_UP_PROMPT}"
HINT_ROW = "to browse and edit queued messages"


def strip_rows(frame):
    """The queue-strip rows of one frame, ANSI-stripped, in render order."""
    plain = re.sub("\x1b\\[[0-9;]*[A-Za-z]", "", frame)
    return [
        line.strip()
        for line in plain.split("\n")
        if any(marker in line for marker in ("Steering: ", "Follow-up: ", HINT_ROW))
    ]


def prepare_queue_sandbox(base):
    """The visual_parity fixture with the queue-specific faux script."""
    shared_cwd, script_path, sandboxes = vp.prepare_sandbox(base)
    script_path = os.path.join(base, "queue-faux-script.json")
    with open(script_path, "w") as f:
        json.dump(QUEUE_FAUX_SCRIPT, f, indent=2)
    return shared_cwd, script_path, sandboxes


def run_queue_session(binary, sandbox, shared_cwd, script_path, size, out_dir, prefix):
    """Drive one binary through the queue states, capturing each frame."""
    width, height = size
    session = f"vplane-{prefix}-queue-{binary}-{width}x{height}"
    vp.tmux("kill-session", "-t", session, check=False)
    vp.tmux("new-session", "-d", "-s", session, "-x", width, "-y", height, "-c", shared_cwd)
    env = (
        f"HOME={sandbox['home']} "
        f"PRIME_AGENT_CODING_AGENT_DIR={sandbox['agent']} "
        f"PRIME_AGENT_FAUX_SCRIPT={script_path} "
        "PRIME_AGENT_DISABLE_ANALYTICS=1"
    )
    if binary == "ts":
        command = (
            f"prime-agent --daemon-socket {sandbox['agent']}/daemon.sock "
            f"--model {vp.TS_SCRIPT_MODEL}"
        )
    else:
        rust = os.environ.get(
            "PA_RUST_BINARY",
            os.path.join(
                os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "prime-agent"
            ),
        )
        package_dir = os.environ.get("PI_PACKAGE_DIR") or vp.find_runtime_package_dir()
        command = (
            f"PI_PACKAGE_DIR={package_dir} "
            f"{rust} --daemon-socket {sandbox['agent']}/daemon.sock "
            f"--model {vp.TS_SCRIPT_MODEL}"
        )
    vp.tmux("send-keys", "-t", session, f"{env} {command}", "Enter")

    frames = {}
    # (a) fresh start.
    vp.wait_for(session, "Collapsed mode", timeout=60)
    # (q) the queue strip: submit the slow first turn, wait until it streams,
    # then park one steering prompt (Enter) and one follow-up prompt
    # (alt+enter) behind it.
    vp.tmux("send-keys", "-t", session, FIRST_PROMPT)
    vp.tmux("send-keys", "-t", session, "Enter")
    vp.wait_for(session, "streams slowly", timeout=60)
    vp.tmux("send-keys", "-t", session, STEERING_PROMPT)
    vp.tmux("send-keys", "-t", session, "Enter")
    vp.tmux("send-keys", "-t", session, FOLLOW_UP_PROMPT)
    vp.tmux("send-keys", "-t", session, "M-Enter")
    vp.wait_for(session, HINT_ROW, timeout=30)
    vp.wait_for(session, STEERING_ROW, timeout=15)
    vp.wait_for(session, FOLLOW_UP_ROW, timeout=15)
    frames["q_queue_strip"] = vp.capture(session)

    # (b) the browse state: alt+up selects the newest parked message (the
    # follow-up) and shows the dim browse header.
    vp.tmux("send-keys", "-t", session, "M-Up")
    vp.wait_for(session, "browse", timeout=15)
    frames["q_browse_header"] = vp.capture(session)
    vp.tmux("send-keys", "-t", session, "M-Down")
    time.sleep(1.0)

    # (c) drained: both queued prompts delivered, strip cleared.
    vp.wait_for(session, "steering delivered", timeout=120)
    vp.wait_for(session, "follow-up delivered", timeout=120)
    for _ in range(200):
        pane = vp.capture(session, escape=False)
        if HINT_ROW not in pane:
            break
        time.sleep(0.3)
    frames["q_drained"] = vp.capture(session)

    # Exit: ctrl+c aborts the settled run, a second press exits.
    vp.tmux("send-keys", "-t", session, "C-c")
    time.sleep(0.5)
    vp.tmux("send-keys", "-t", session, "C-c")
    time.sleep(1.0)
    vp.tmux("kill-session", "-t", session, check=False)

    os.makedirs(out_dir, exist_ok=True)
    for state, frame in frames.items():
        with open(os.path.join(out_dir, f"{binary}-{state}-{width}x{height}.txt"), "w") as f:
            f.write(frame)
    return frames


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--size", default="120x36")
    parser.add_argument("--keep", action="store_true")
    parser.add_argument("--out", default=None)
    parser.add_argument("--only", default=None, choices=("ts", "rust"))
    parser.add_argument("--session-prefix", default="vp")
    args = parser.parse_args()
    width, height = args.size.split("x")
    size = (width, height)

    base = tempfile.mkdtemp(prefix="queue-parity-sandbox-")
    out_dir = args.out or tempfile.mkdtemp(prefix="queue-parity-captures-")
    shared_cwd, script_path, sandboxes = prepare_queue_sandbox(base)
    try:
        if args.only:
            run_queue_session(
                args.only, sandboxes[args.only], shared_cwd, script_path, size, out_dir, args.session_prefix
            )
            print(f"captures for {args.only} in {out_dir}")
            return 0
        ts_frames = run_queue_session(
            "ts", sandboxes["ts"], shared_cwd, script_path, size, out_dir, args.session_prefix
        )
        rust_frames = run_queue_session(
            "rust", sandboxes["rust"], shared_cwd, script_path, size, out_dir, args.session_prefix
        )
        failures = []
        for state in ("q_queue_strip", "q_browse_header", "q_drained"):
            ts_rows = strip_rows(ts_frames[state])
            rust_rows = strip_rows(rust_frames[state])
            name = f"{state}-{args.size}"
            if ts_rows == rust_rows:
                print(f"PASS {name}: {len(ts_rows)} strip row(s) match")
                for row in ts_rows:
                    print(f"  | {row}")
            else:
                print(f"FAIL {name}")
                print(
                    "\n".join(
                        difflib.unified_diff(
                            [str(r) for r in ts_rows], [str(r) for r in rust_rows],
                            fromfile="ts", tofile="rust", lineterm="", n=1,
                        )
                    )
                )
                failures.append(name)
        if failures:
            print(f"{len(failures)} queue state(s) differ; captures in {out_dir}")
            return 1
        print(f"all queue states match; captures in {out_dir}")
        return 0
    finally:
        if not args.keep:
            shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
