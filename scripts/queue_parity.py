#!/usr/bin/env python3
"""Prompt-surface parity: frame-diff the queued-prompt strip and the live
editor highlight against the installed TS prime-agent binary in tmux.

Drives both binaries to the same defined states and compares what the user
sees:

- the queued strip behind a slow running turn — one steering prompt (Enter)
  and one follow-up prompt (alt+enter), drained after delivery — with the
  strip rows compared ANSI byte-exact (the TS prompt-highlight styling: dim
  previews, accent on a leading slash command's `/name` segment,
  success/mdLink on @path/--flag tokens);
- a second parked lane with slash-command and token-bearing prompts, so the
  accent styling itself is compared byte-exact (captured before delivery,
  aborted away after);
- the live editor with a slash command and argument tokens typed but not
  submitted — cursor at the end (command segment accented, tokens colored)
  and cursor moved inside the command token (accent suppressed), the
  editor's text row compared byte-exact.

Exit code is non-zero when any state's visible rows differ.

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

# Turn 1/4 stream slowly (6 tokens/s) so the window stays open for the
# parked submissions; turns 2/3 answer the queued prompts.
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
                    "text": "The first turn streams slowly and keeps going for a good while so the parked preview strip has plenty of time to render before this turn settles and the queue drains.",
                }
            ]
        },
        {"content": [{"type": "text", "text": "steering delivered"}]},
        {"content": [{"type": "text", "text": "follow-up delivered"}]},
        {
            "content": [
                {
                    "type": "text",
                    "text": "The second turn streams even more slowly and keeps going for a while so the parked slash command strip has plenty of time to render before the turn settles and the queue drains.",
                }
            ]
        },
    ],
}

FIRST_PROMPT = "tell me something slowly"
STEERING_PROMPT = "steering prompt text"
FOLLOW_UP_PROMPT = "follow-up prompt text"
SECOND_PROMPT = "tell me something slowly again"

# The editor states type a recognized argument-taking command with both
# argument-token forms. The parked slash prompts are session commands
# (`/compact`, `/goal`): client commands execute immediately instead of
# parking, while session commands queue behind the running turn and render
# in the accent-styled strip — captured before delivery and aborted away.
EDITOR_COMMAND = "/new @docs/plan.md --draft"
SLASH_STEERING_PROMPTS = [
    "/compact focus the summary on tests",
    "/goal finish the port @docs/plan.md --verbose",
    "fix @Cargo.toml --quiet now",
]

# Strip rows both binaries must render (the visible queue surface).
STEERING_ROW = f"Steering: {STEERING_PROMPT}"
FOLLOW_UP_ROW = f"Follow-up: {FOLLOW_UP_PROMPT}"
HINT_ROW = "to browse and edit queued messages"

ANSI_PATTERN = re.compile("\x1b\\[[0-9;]*[A-Za-z]")


def strip_ansi(text):
    return ANSI_PATTERN.sub("", text)


def normalize_sgr_boundaries(frame):
    """Canonicalize tmux's row-boundary reset placement (the same rules
    visual_parity applies before its byte-diffs): the trailing fg/bg reset
    attaches to the end of the styled row or the start of the next one
    depending on timing, and both describe identical cells."""
    frame = re.sub("\x1b\\[39m\n", "\n", frame)
    frame = re.sub("\n\x1b\\[39m(?= )", "\n", frame)
    frame = re.sub("\x1b\\[49m\n", "\n", frame)
    frame = re.sub("\n\x1b\\[49m(?= )", "\n", frame)
    return frame


def strip_rows(frame):
    """The queue-strip rows of one frame, ANSI-stripped, in render order."""
    return [
        line.strip()
        for line in strip_ansi(frame).split("\n")
        if any(marker in line for marker in ("Steering: ", "Follow-up: ", HINT_ROW))
    ]


def styled_rows(frame, markers):
    """The rows of one frame containing any marker, ANSI bytes intact."""
    frame = normalize_sgr_boundaries(frame)
    return [
        line
        for line in frame.split("\n")
        if any(marker in strip_ansi(line) for marker in markers)
    ]


def strip_rows_styled(frame):
    """The queue-strip rows with their ANSI styling, for byte-exact compare."""
    return styled_rows(
        frame, ("Steering: ", "Follow-up: ", HINT_ROW)
    )


def editor_rows_styled(frame):
    """The editor's typed-text row with its ANSI styling, for byte-exact
    compare (the prompt dock row holding the command)."""
    return styled_rows(frame, ("@docs/plan.md",))


def prepare_queue_sandbox(base):
    """The visual_parity fixture with the queue-specific faux script."""
    shared_cwd, script_path, sandboxes = vp.prepare_sandbox(base)
    script_path = os.path.join(base, "queue-faux-script.json")
    with open(script_path, "w") as f:
        json.dump(QUEUE_FAUX_SCRIPT, f, indent=2)
    return shared_cwd, script_path, sandboxes


def wait_plain(session, needle, timeout=30, gone=False):
    """Poll the pane's ANSI-stripped text until `needle` appears (or is gone)."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        plain = strip_ansi(vp.capture(session))
        if (needle not in plain) == gone:
            return
        time.sleep(0.3)
    raise RuntimeError(
        f"timed out waiting for {needle!r} to {'vanish' if gone else 'appear'}"
    )


def run_queue_session(binary, sandbox, shared_cwd, script_path, size, out_dir, prefix):
    """Drive one binary through the queue/editor states, capturing each frame."""
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
    # (e1) the editor highlight: type a slash command with argument tokens
    # without submitting, close the autocomplete, capture the editor row.
    vp.tmux("send-keys", "-t", session, EDITOR_COMMAND)
    wait_plain(session, "@docs/plan.md")
    vp.tmux("send-keys", "-t", session, "Escape")
    time.sleep(1.0)
    frames["e_editor_slash"] = vp.capture(session)
    # (e2) the cursor inside the command token suppresses its accent; the
    # argument tokens stay colored.
    for _ in range(len(EDITOR_COMMAND) - 2):
        vp.tmux("send-keys", "-t", session, "Left")
    time.sleep(1.0)
    frames["e_editor_cursor_in_command"] = vp.capture(session)
    # Clear the editor (TS escape-repeat: two presses clear the input).
    vp.tmux("send-keys", "-t", session, "Escape")
    time.sleep(0.4)
    vp.tmux("send-keys", "-t", session, "Escape")
    wait_plain(session, "@docs/plan.md", gone=True, timeout=15)

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
    time.sleep(0.3)
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

    # (s) the slash-command strip: a second slow turn, with slash-command and
    # token-bearing prompts parked behind it; captured before delivery.
    vp.tmux("send-keys", "-t", session, SECOND_PROMPT)
    vp.tmux("send-keys", "-t", session, "Enter")
    vp.wait_for(session, "even more slowly", timeout=60)
    for prompt in SLASH_STEERING_PROMPTS:
        vp.tmux("send-keys", "-t", session, prompt)
        vp.tmux("send-keys", "-t", session, "Enter")
    # The accent styling splits the row mid-text, so wait on the
    # ANSI-stripped pane.
    for prompt in SLASH_STEERING_PROMPTS:
        wait_plain(session, f"Steering: {prompt}")
    time.sleep(0.3)
    frames["q_slash_strip"] = vp.capture(session)

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


def require_markers(frame, markers, state):
    """Scenario guard: every expected row must be on screen when captured,
    so a too-late capture (delivered before the frame) cannot false-pass."""
    plain = strip_ansi(frame)
    missing = [marker for marker in markers if marker not in plain]
    if missing:
        raise RuntimeError(
            f"scenario drift in {state}: expected rows not on screen: {missing}"
        )


def compare(name, ts_rows, rust_rows):
    if ts_rows == rust_rows:
        print(f"PASS {name}: {len(ts_rows)} row(s) match")
        for row in ts_rows:
            print(f"  | {row}")
        return None
    print(f"FAIL {name}")
    print(
        "\n".join(
            difflib.unified_diff(
                [str(r) for r in ts_rows], [str(r) for r in rust_rows],
                fromfile="ts", tofile="rust", lineterm="", n=1,
            )
        )
    )
    return name


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
        # Scenario guards: the strip states must show every parked row when
        # captured (a frame after delivery would compare empty rows).
        require_markers(
            ts_frames["q_queue_strip"],
            [STEERING_ROW, FOLLOW_UP_ROW, HINT_ROW],
            "q_queue_strip (ts)",
        )
        require_markers(
            ts_frames["q_slash_strip"],
            [f"Steering: {prompt}" for prompt in SLASH_STEERING_PROMPTS] + [HINT_ROW],
            "q_slash_strip (ts)",
        )
        failures = []
        # Strip rows compare ANSI byte-exact: the prompt-highlight styling
        # (dim base, accent command segment, colored tokens) is the surface
        # under test.
        failures.append(
            compare(
                f"q_queue_strip-{args.size}",
                strip_rows_styled(ts_frames["q_queue_strip"]),
                strip_rows_styled(rust_frames["q_queue_strip"]),
            )
        )
        failures.append(
            compare(
                f"q_slash_strip-{args.size}",
                strip_rows_styled(ts_frames["q_slash_strip"]),
                strip_rows_styled(rust_frames["q_slash_strip"]),
            )
        )
        # The editor's typed-text row compares byte-exact in both cursor
        # positions.
        for state in ("e_editor_slash", "e_editor_cursor_in_command"):
            failures.append(
                compare(
                    f"{state}-{args.size}",
                    editor_rows_styled(ts_frames[state]),
                    editor_rows_styled(rust_frames[state]),
                )
            )
        # The browse header and drained strip keep the plain-row compare.
        for state in ("q_browse_header", "q_drained"):
            name = f"{state}-{args.size}"
            ts_rows = strip_rows(ts_frames[state])
            rust_rows = strip_rows(rust_frames[state])
            if ts_rows == rust_rows:
                print(f"PASS {name}: {len(ts_rows)} strip row(s) match")
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
        if any(failures):
            failed = [f for f in failures if f]
            print(f"{len(failed)} state(s) differ; captures in {out_dir}")
            return 1
        print(f"all queue/editor states match; captures in {out_dir}")
        return 0
    finally:
        if not args.keep:
            shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
