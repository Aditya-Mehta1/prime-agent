#!/usr/bin/env python3
"""Interactive TUI visual-parity verifier: frame-diff the Rust interactive
UI against the installed TS prime-agent binary in tmux.

Drives both binaries to the same defined states (fresh start, one turn with a
tool call, thinking visible via Ctrl+O, the working spinner mid-turn, and
the idle frame after the turn) at 120x36 and 220x50, captures the rendered
panes with escape sequences, normalizes volatile content, and reports
per-state frame diffs. Exit code is non-zero when any state differs.

The TS side is driven through the faux provider registered by a sandbox
extension (the harness equivalent of the test suite's registerFauxProvider
fixtures); the Rust side runs the real agent engine over the same scripted
faux provider through `PRIME_AGENT_FAUX_SCRIPT` ({"engine": "faux", ...}).

tmux rules: default socket only (`env -u TMUX`), vplane-* session names,
no kill-server; sessions are killed individually at the end.
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
SIZES = [("120", "36"), ("220", "50")]

# The single turn both binaries run: thinking, a text block, an ipython tool
# call, then the final answer. Content is identical on both sides so the
# frames compare content-for-content.
FAUX_SCRIPT = {
    "engine": "faux",
    "modelId": "faux-1",
    "modelName": "Faux Model",
    "reasoning": False,
    "contextWindow": 128000,
    "tokensPerSecond": 18,
    "responses": [
        {
            "content": [
                {
                    "type": "thinking",
                    "thinking": "The user wants a quick check. I will run a short Python cell in the persistent kernel and report the result.",
                },
                {"type": "text", "text": "Let me run a quick check."},
                {
                    "type": "toolCall",
                    "name": "ipython",
                    "id": "toolu_visual01",
                    "arguments": {"code": "print('visual parity ok')"},
                },
            ]
        },
        {"content": [{"type": "text", "text": "The check printed the expected marker. Anything else?"}]},
    ],
}

# The TS extension registers the faux provider and scripts its responses from
# PRIME_AGENT_FAUX_SCRIPT (mirrors the test-suite registerFauxProvider fixture).
TS_FAUX_EXTENSION = '// Visual-parity driver extension: registers the faux provider with scripted\n// responses read from PRIME_AGENT_FAUX_SCRIPT (same harness contract the Rust\n// rewrite uses). Verification harness only; never installed for real users.\nimport {\n\tregisterFauxProvider,\n\tfauxAssistantMessage,\n\tfauxText,\n\tfauxThinking,\n\tfauxToolCall,\n\tgetApiProvider,\n} from "@earendil-works/pi-ai";\nimport { readFileSync } from "node:fs";\n\nexport default function registerVisualFaux(pi) {\n\tconst scriptPath = process.env.PRIME_AGENT_FAUX_SCRIPT;\n\tconst script = scriptPath ? JSON.parse(readFileSync(scriptPath, "utf8")) : {};\n\tconst provider = script.provider || "faux";\n\tconst modelId = script.modelId || "faux-1";\n\tconst faux = registerFauxProvider({\n\t\tprovider,\n\t\tapi: "faux",\n\t\tmodels: [\n\t\t\t{\n\t\t\t\tid: modelId,\n\t\t\t\tname: script.modelName || "Faux Model",\n\t\t\t\treasoning: script.reasoning ?? false,\n\t\t\t\tcontextWindow: script.contextWindow ?? 128000,\n\t\t\t},\n\t\t],\n\t\ttokensPerSecond: script.tokensPerSecond,\n\t});\n\tconst responses = (script.responses || []).map((entry) => {\n\t\tlet content;\n\t\tif (typeof entry === "string") {\n\t\t\tcontent = [fauxText(entry)];\n\t\t} else if (Array.isArray(entry.content)) {\n\t\t\tcontent = entry.content.map((block) => {\n\t\t\t\tif (block.type === "thinking") return fauxThinking(block.thinking);\n\t\t\t\tif (block.type === "toolCall") return fauxToolCall(block.name, block.arguments, { id: block.id });\n\t\t\t\treturn fauxText(block.text);\n\t\t\t});\n\t\t} else {\n\t\t\tcontent = [fauxText(entry.text || "")];\n\t\t}\n\t\tconst stopReason =\n\t\t\tentry.stopReason ||\n\t\t\t(content.some((block) => block.type === "toolCall") ? "toolUse" : "stop");\n\t\treturn fauxAssistantMessage(content, { stopReason });\n\t});\n\tfaux.setResponses(responses);\n\tconst apiProvider = getApiProvider(faux.api);\n\tif (!apiProvider) {\n\t\tthrow new Error("Faux API provider was not registered");\n\t}\n\tpi.registerProvider(provider, {\n\t\tapi: faux.api,\n\t\tapiKey: "faux-key",\n\t\tbaseUrl: faux.getModel().baseUrl,\n\t\tstreamSimple: apiProvider.streamSimple,\n\t\tmodels: faux.models.map((model) => ({\n\t\t\tapi: model.api,\n\t\t\tbaseUrl: model.baseUrl,\n\t\t\tcontextWindow: model.contextWindow,\n\t\t\tcost: model.cost,\n\t\t\tid: model.id,\n\t\t\tinput: model.input,\n\t\t\tmaxTokens: model.maxTokens,\n\t\t\tname: model.name,\n\t\t\treasoning: model.reasoning,\n\t\t})),\n\t});\n}\n'

PROMPT = "Run a quick check."

# Spinner + working-icon frames (chat.rs LOADER_FRAMES / WORKING_ICON_FRAMES).
SPINNER_CHARS = "\u280b\u2819\u2839\u2838\u283c\u2834\u2826\u2827\u2807\u280f\u25f4\u25f7\u25f6\u25f5\u25cb\u25f8\u25fb\u25fc"


def find_runtime_package_dir():
    """Locate the installed TS release directory (ships prime-agent-runtime)."""
    releases = os.path.expanduser("~/.local/share/prime-agent/releases")
    if not os.path.isdir(releases):
        raise SystemExit(
            "cannot find the prime-agent-runtime sidecar; set PI_PACKAGE_DIR "
            "to a directory containing prime-agent-runtime/"
        )
    candidates = [
        entry
        for entry in sorted(os.listdir(releases))
        if os.path.isdir(
            os.path.join(releases, entry, "prime-agent-runtime")
        )
    ]
    if not candidates:
        raise SystemExit(
            "no release with prime-agent-runtime/ under " + releases
        )
    return os.path.join(releases, candidates[-1])


STATES = [
    ("a_fresh_start", "fresh splash with model and cwd lines"),
    ("b_turn_with_tool", "idle after a turn containing a tool-call card"),
    ("c_thinking_visible", "conversation detail (Ctrl+O): thinking block visible"),
    ("d_spinner", "working loader mid-turn"),
]


def tmux(*args, check=True):
    """Run a tmux command on the default socket (never inside TMUX)."""
    result = subprocess.run(
        ["env", "-u", "TMUX", "tmux", *args],
        capture_output=True,
        text=True,
    )
    if check and result.returncode != 0:
        raise RuntimeError(f"tmux {' '.join(args)} failed: {result.stderr}")
    return result.stdout


def capture(session, escape=True):
    flag = "-e" if escape else "-p"
    return tmux("capture-pane", flag, "-p", "-t", session)


def normalize(frame, root):
    """Normalize volatile content so stable chrome compares equal."""
    frame = frame.replace(root, "<SANDBOX>")
    # Product versions differ between the binaries.
    frame = re.sub(r"v\d+\.\d+\.\d+", "vX.X.X", frame)
    # Session ids (12-hex display ids).
    frame = re.sub(r"\b[0-9a-f]{12}\b", "<SID>", frame)
    # Durations ("2ms", "1.2s", "0.0s").
    frame = re.sub(r"\b\d+(\.\d+)?(ms|s)\b", "<T>", frame)
    # Context usage in the tray: "6.1k (5%)", "72 (0%)".
    frame = re.sub(r"\d+(\.\d+)?[kM]? \(\d+%\)", "<TOK> (<PCT>)", frame)
    # Loader token counts and elapsed seconds.
    frame = re.sub(r"[\u2193\u2191] [\d.kM]+ tokens", "<DIR> <TOK> tokens", frame)
    frame = re.sub(r"\b\d+s\b", "<S>", frame)
    # Spinner and working-icon animation frames.
    spinners = "".join("\u280b\u2819\u2839\u2838\u283c\u2834\u2826\u2827\u2807\u280f")
    frame = re.sub("[" + spinners + "]", "<SPIN>", frame)
    pulses = "".join("\u25f4\u25f7\u25f6\u25f5\u25cb\u25f8\u25fb\u25fc")
    frame = re.sub("[" + pulses + "]", "<PULSE>", frame)
    # tmux places the trailing foreground-reset (\x1b[39m) differently for
    # identical screens: at the end of the row whose styled text just ended,
    # or before the next row's default margin. Both describe default-colored
    # cells, so drop boundary resets before comparing.
    frame = re.sub("\x1b\[39m\n", "\n", frame)
    frame = re.sub("\n\x1b\[39m(?= )", "\n", frame)
    return frame


def diff_lines(left, right):
    return "\n".join(
        difflib.unified_diff(
            left.split("\n"), right.split("\n"),
            fromfile="ts", tofile="rust", lineterm="", n=1,
        )
    )


def prepare_sandbox(base):
    """Create the per-binary sandboxes and the shared cwd fixture."""
    shared_cwd = os.path.join(base, "shared-cwd")
    os.makedirs(shared_cwd, exist_ok=True)
    with open(os.path.join(shared_cwd, "alpha.txt"), "w") as f:
        f.write("alpha\n")
    with open(os.path.join(shared_cwd, "beta.md"), "w") as f:
        f.write("# beta\n")

    script_path = os.path.join(base, "faux-script.json")
    with open(script_path, "w") as f:
        json.dump(FAUX_SCRIPT, f, indent=2)

    sandboxes = {}
    for binary in ("ts", "rust"):
        home = os.path.join(base, binary, "home")
        agent = os.path.join(base, binary, "agent")
        os.makedirs(home, exist_ok=True)
        os.makedirs(os.path.join(agent, "extensions"), exist_ok=True)
        os.makedirs(os.path.join(agent, "sessions"), exist_ok=True)
        with open(os.path.join(agent, "settings.json"), "w") as f:
            json.dump({"onboardingCompleted": True}, f)
        sandboxes[binary] = {"home": home, "agent": agent}
    with open(
        os.path.join(sandboxes["ts"]["agent"], "extensions", "visual-faux.js"), "w"
    ) as f:
        f.write(TS_FAUX_EXTENSION)
    return shared_cwd, script_path, sandboxes


def wait_for(session, needle, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        pane = capture(session, escape=False)
        if needle in pane:
            return
        time.sleep(0.3)
    raise TimeoutError(f"session {session} never showed {needle!r}")


def run_session(binary, sandbox, shared_cwd, script_path, size, out_dir):
    """Drive one binary through the defined states, capturing each frame."""
    width, height = size
    session = f"vplane-vp-{binary}-{width}x{height}"
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
        rust = os.environ.get(
            "PA_RUST_BINARY",
            os.path.join(
                os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "prime-agent"
            ),
        )
        # The Rust kernel needs the prime-agent-runtime sidecar (the TS
        # release ships it next to its binary; the Rust build tree does not).
        package_dir = os.environ.get("PI_PACKAGE_DIR") or find_runtime_package_dir()
        command = (
            f"PI_PACKAGE_DIR={package_dir} "
            f"{rust} --daemon-socket {sandbox['agent']}/daemon.sock "
            f"--model {TS_SCRIPT_MODEL}"
        )
    tmux("send-keys", "-t", session, f"{env} {command}", "Enter")

    frames = {}
    # (a) fresh start: wait for the splash + editor to settle.
    wait_for(session, "Collapsed mode", timeout=40)
    time.sleep(1.0)
    frames["a_fresh_start"] = capture(session)

    # Submit the turn.
    tmux("send-keys", "-t", session, PROMPT)
    tmux("send-keys", "-t", session, "Enter")

    # (d) spinner: the thinking block streams first, so both binaries show
    # the loader with the Thinking activity; poll for it rather than a fixed
    # delay (kernel startup timing varies).
    deadline = time.time() + 60
    while time.time() < deadline:
        pane = capture(session, escape=False)
        if re.search(r"\u00b7 Thinking \u00b7", pane):
            break
        time.sleep(0.1)
    frames["d_spinner"] = capture(session)

    # (b) idle after the turn: the final answer rendered AND the loader row
    # is gone (the spinner line disappears once the turn ends).
    deadline = time.time() + 120
    while time.time() < deadline:
        pane = capture(session, escape=False)
        if "Anything else?" in pane and not re.search(SPINNER_CHARS, pane):
            break
        time.sleep(0.3)
    time.sleep(1.0)
    frames["b_turn_with_tool"] = capture(session)

    # (c) thinking visible: Ctrl+O toggles conversation detail.
    tmux("send-keys", "-t", session, "C-o")
    try:
        wait_for(session, "Details mode", timeout=10)
    except TimeoutError:
        pass
    time.sleep(1.0)
    frames["c_thinking_visible"] = capture(session)

    # Exit: ctrl+c aborts a running turn, a second press exits when idle.
    tmux("send-keys", "-t", session, "C-c")
    time.sleep(0.5)
    tmux("send-keys", "-t", session, "C-c")
    time.sleep(1.0)
    tmux("kill-session", "-t", session, check=False)

    os.makedirs(out_dir, exist_ok=True)
    for state, frame in frames.items():
        with open(os.path.join(out_dir, f"{binary}-{state}-{width}x{height}.txt"), "w") as f:
            f.write(frame)
    return frames


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sizes", default=",".join(f"{w}x{h}" for w, h in SIZES))
    parser.add_argument("--keep", action="store_true", help="keep the sandbox dir")
    parser.add_argument(
        "--out", default=None, help="captures directory (default: a fresh temp dir)"
    )
    parser.add_argument(
        "--only", default=None, help="run a single binary (ts|rust) for capture shakedown"
    )
    args = parser.parse_args()
    sizes = []
    for entry in args.sizes.split(","):
        width, height = entry.split("x")
        sizes.append((width, height))

    base = tempfile.mkdtemp(prefix="visual-parity-sandbox-")
    out_dir = args.out or tempfile.mkdtemp(prefix="visual-parity-captures-")
    shared_cwd, script_path, sandboxes = prepare_sandbox(base)
    failures = []
    try:
        if args.only:
            run_session(args.only, sandboxes[args.only], shared_cwd, script_path, sizes[0], out_dir)
            print(f"captures for {args.only} in {out_dir}")
            return 0
        for size in sizes:
            ts_frames = run_session("ts", sandboxes["ts"], shared_cwd, script_path, size, out_dir)
            rust_frames = run_session("rust", sandboxes["rust"], shared_cwd, script_path, size, out_dir)
            for state, _ in STATES:
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
        if not args.keep:
            shutil.rmtree(base, ignore_errors=True)
    if failures:
        print(f"{len(failures)} state(s) differ; captures in {out_dir}")
        return 1
    print(f"all states match; captures in {out_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
