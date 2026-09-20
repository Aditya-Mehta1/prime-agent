#!/usr/bin/env python3
"""Print-mode json event-stream parity verifier: run the installed TS
prime-agent binary and the Rust rewrite in `--mode json` print runs over the
same scripted faux provider, normalize the volatile event fields, and diff
the full event sequences (the #211 parity-evidence pattern, headless: no
tmux, stdout json lines only).

Scenarios (each a fresh isolated HOME + agent dir per side; a scenario is
one or more sequential runs over the same shared session store):

  - stream: one prompt, one streamed text answer. Covers the session
    header row, the harness digest's message pair (TS commit-time
    injection, between turn_start and the user message pair), the
    message_update deltas with the slim assistantMessageEvent (no nested
    partial on the wire), and the run lifecycle order.
  - threshold: a small seed turn then a ~12k-token crossing turn over a
    16k context window with reserveTokens=1. Covers the threshold
    compaction arm's compaction_start/compaction_end pair (reason, result,
    willRetry=false) at the settled turn boundary. The compact-trigger
    auto-refine the compaction schedules stays off the stream on both
    sides: the disposal drain runs after the client's subscription is torn
    down (only the durable rows persist).
  - resume: two runs over one session. Run one ends above the reserve
    headroom with compaction disabled (nothing fires, either boundary);
    run two resumes with compaction enabled (`--continue`, its own daemon
    socket so the first run's worker never pins the session), and the
    pre-turn boundary (TS `_runPreTurnCompaction`) fires the threshold
    arm before the admitted prompt: the compaction_start/compaction_end
    pair precedes the prompt's turn events on both sides.
  - compact-refine: the overflow compact-and-retry arm whose retried turn
    settles, then the serialized checkpoint's compact-trigger auto-refine:
    the review gate approves and the refinement runs mid-run, streaming
    the refinement rows' message pairs and refine_complete (the #214
    residue: TS print mode auto-refines after compactions; probed against
    the TS binary).
  - compact-refine-decline: the same overflow shape with a declining
    review - no refinement rows, no refine events, identical streams.

The Rust run rides the harness digest row through the loop's prompt input
(the same TS design), so `agent_end.messages` includes it on both sides and
the comparison is full parity: any event difference fails.

Both sides run with RLM_DEPTH unset (root sessions are depth 0; the TS
daemon strips the env for its workers, the Rust print run is in-process).
The compaction scenarios that stay about the compaction arms (`threshold`,
`resume`) pin `autoRefine: {enabled: false}` in the sandbox settings so
the review rounds never enter their streams; the compact-refine scenarios
leave the gate at its enabled default - the compact-trigger review IS the
surface under test there (the threshold scenario scripts the drain's
decline, so its disposal round stays silent on both sides at the enabled
default). The settings gate is the product's own switch, so nothing else
differs.

Exit code is non-zero when any event sequence differs. Use --keep to keep
the sandbox trees, --out to pin captures.
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

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "battery"))
import ts_identity  # noqa: E402  (the shared PATH-binary identity guard)

TS_FAUX_EXTENSION = open(
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "ts_faux_extension.js"),
    encoding="utf-8",
).read()


def find_runtime_package_dir():
    releases = os.path.expanduser("~/.local/share/prime-agent/releases")
    if not os.path.isdir(releases):
        raise SystemExit("cannot find the prime-agent-runtime sidecar; set PI_PACKAGE_DIR")
    candidates = [
        entry
        for entry in sorted(os.listdir(releases))
        if os.path.isdir(os.path.join(releases, entry, "prime-agent-runtime"))
    ]
    if not candidates:
        raise SystemExit("no release with prime-agent-runtime/ under " + releases)
    return os.path.join(releases, candidates[-1])


def prepare_sandbox(base, binary, settings):
    home = os.path.join(base, binary, "home")
    agent = os.path.join(base, binary, "agent")
    os.makedirs(home, exist_ok=True)
    os.makedirs(os.path.join(agent, "sessions"), exist_ok=True)
    os.makedirs(os.path.join(agent, "extensions"), exist_ok=True)
    write_settings(agent, settings)
    if binary == "ts":
        # The TS binary runs its session in a daemon worker; the faux
        # provider is a sandbox extension.
        with open(os.path.join(agent, "extensions", "print-faux.js"), "w") as handle:
            handle.write(TS_FAUX_EXTENSION)
    return {"home": home, "agent": agent}


def write_settings(agent, settings):
    with open(os.path.join(agent, "settings.json"), "w") as handle:
        json.dump(settings, handle)


def run_scenario(binary, sandbox, script_path, prompts, cwd, extra_args, socket_path):
    """Run the binary in print json mode; return (exit, stdout, stderr)."""
    env = {key: value for key, value in os.environ.items()}
    # Root sessions are depth 0 on both sides; ambient harness env stays out.
    for leaked in ("RLM_DEPTH", "HOME", "PRIME_AGENT_CODING_AGENT_DIR", "PRIME_AGENT_FAUX_SCRIPT"):
        env.pop(leaked, None)
    env.update(
        {
            "HOME": sandbox["home"],
            "PRIME_AGENT_CODING_AGENT_DIR": sandbox["agent"],
            "PRIME_AGENT_FAUX_SCRIPT": script_path,
            "PRIME_AGENT_DISABLE_ANALYTICS": "1",
            "PRIME_DISABLE_VERSION_CHECK": "1",
        }
    )
    if binary == "ts":
        command = ["prime-agent", "--daemon-socket", socket_path, "--model", "faux-1", "--mode", "json"]
    else:
        rust = os.environ.get(
            "PA_RUST_BINARY",
            os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "prime-agent"),
        )
        env["PI_PACKAGE_DIR"] = os.environ.get("PI_PACKAGE_DIR") or find_runtime_package_dir()
        # The Rust print harness takes the faux script inline (the env var
        # is the JSON, not a path).
        with open(script_path) as handle:
            env["PRIME_AGENT_FAUX_SCRIPT"] = handle.read()
        command = [rust, "--daemon-socket", socket_path, "--model", "faux-1", "--mode", "json"]
    # The resume flags ride ahead of the prompts (both parsers accept
    # positionals only last).
    result = subprocess.run(
        command + extra_args + ["-p"] + prompts,
        capture_output=True,
        text=True,
        env=env,
        cwd=cwd,
        timeout=300,
    )
    return result.returncode, result.stdout, result.stderr


UUID_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")


def normalize(value, sandbox_root):
    """Normalize one parsed event (volatile fields -> placeholders)."""
    if isinstance(value, list):
        return [normalize(item, sandbox_root) for item in value]
    if not isinstance(value, dict):
        return value
    out = {}
    for key, item in value.items():
        out[key] = normalize_value(key, item, sandbox_root)
    return out


def normalize_value(key, item, sandbox_root):
    if isinstance(item, (int, float)) and not isinstance(item, bool):
        if key in ("timestamp", "tokensBefore", "tokens", "totalTokens", "input", "output", "cacheRead", "cacheWrite"):
            return "<N>"
        return item
    if isinstance(item, str):
        if key == "cwd" or sandbox_root in item:
            return "<SANDBOX>"
        if UUID_RE.match(item):
            return "<ID>"
        if key == "timestamp":
            return "<TS>"
        if key in ("created_at", "updated_at"):
            # Harness-entry timestamps (the refinement result's before/after
            # snapshots): volatile, normalized to a placeholder.
            return "<TS>"
        if key in ("firstKeptEntryId", "id", "refinementId"):
            return "<ID>"
        return item
    if isinstance(item, list):
        return [normalize_value(key, entry, sandbox_root) for entry in item]
    if isinstance(item, dict):
        return normalize(item, sandbox_root)
    return item


def normalize_events(stdout, sandbox_root):
    """Parse, normalize, and collapse the json event lines (consecutive
    message_update deltas of one type collapse to a single run marker -
    the chunk boundaries are a faux-provider artifact, not a wire
    contract)."""
    events = []
    for line in stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        event = normalize_event(json.loads(line), sandbox_root)
        previous = events[-1] if events else None
        if (
            isinstance(previous, dict)
            and previous.get("type") == "message_update"
            and event.get("type") == "message_update"
            and previous.get("assistantMessageEvent") == event.get("assistantMessageEvent")
        ):
            continue
        events.append(event)
    return float_checkpoint_agent_end(events)


def is_refinement_event(event):
    """One serialized-checkpoint refinement event (the compact-trigger
    auto-refine surface): the durable rows' message pairs or the
    refine_complete/refine_failed terminal events."""
    if event.get("type") in ("refine_complete", "refine_failed"):
        return True
    message = event.get("message", {})
    return event.get("type") in ("message_start", "message_end") and message.get(
        "customType"
    ) in ("refinement_outcome", "refinement_notice")


def float_checkpoint_agent_end(events):
    """Hoist the overflow-retry continuation run's trailing agent_end across
    the serialized checkpoint's refinement events (one narrow reordering):
    the TS checkpoint runs inside the agent loop's stop decision - refine
    events first, then the run's agent_end - while the Rust print boundary
    runs post-idle, after the run's agent_end. An agent_end that directly
    follows a turn_end and is followed ONLY by refinement events moves to
    the tail; every other position is untouched."""
    for index, event in enumerate(events):
        if (
            event.get("type") == "agent_end"
            and index > 0
            and events[index - 1].get("type") == "turn_end"
            and events[index + 1 :]
            and all(is_refinement_event(item) for item in events[index + 1 :])
        ):
            return events[:index] + events[index + 1 :] + [event]
    return events


def normalize_event(event, sandbox_root):
    event = normalize(event, sandbox_root)
    kind = event.get("type")
    if kind == "session":
        # Keep the identity row's shape only.
        return {"type": "session", "version": event.get("version")}
    if kind in ("message_start", "message_end", "message_update", "turn_end"):
        message = event.get("message", {})
        normalized = {
            "type": kind,
            "message": normalize_message(message, sandbox_root),
        }
        if kind == "message_update":
            # The delta chunking is a faux-provider artifact (token size
            # bounds differ per implementation); the wire contract is the
            # delta type and its content index - the accumulated partial
            # at an intermediate chunk boundary is not. The terminal
            # message_start/message_end events carry the content claim.
            delta = event.get("assistantMessageEvent", {})
            normalized["assistantMessageEvent"] = {
                "type": delta.get("type"),
                "contentIndex": delta.get("contentIndex"),
            }
            normalized["message"] = {"role": normalized["message"].get("role")}
        if kind == "turn_end":
            normalized["toolResults"] = event.get("toolResults", [])
        return normalized
    if kind == "agent_end":
        # Role-level comparison of the run's message list; content is
        # covered by the per-message events above.
        return {
            "type": "agent_end",
            "messages": [
                {"role": message.get("role"), "customType": message.get("customType")}
                for message in event.get("messages", [])
            ],
        }
    if kind == "compaction_end" and isinstance(event.get("result"), dict):
        result = dict(event["result"])
        result["tokensBefore"] = "<N>"
        result["firstKeptEntryId"] = "<ID>"
        # The derived file-operations block is extractor bookkeeping; the
        # parity claim is the summary fields.
        result.pop("details", None)
        event = dict(event)
        event["result"] = result
    return event


def normalize_message(message, sandbox_root):
    """One message's wire shape, volatile fields normalized."""
    if not isinstance(message, dict):
        return message
    out = dict(message)
    if out.get("role") == "custom" and out.get("customType") == "harness_digest":
        # The digest body: the composed harness state (same framing on both
        # sides; the ranked entries differ with wording, so compare the
        # frame shape only).
        content = out.get("content")
        out["content"] = (
            content[: content.index("\n")] + "\n<DIGEST>" if isinstance(content, str) and "\n" in content else "<DIGEST>"
        )
        details = out.get("details")
        if isinstance(details, dict):
            out["details"] = {"digest": "<DIGEST>"}
        return out
    # The assistant wire: drop the Rust-only diagnostics array, normalize
    # the provider registration id and the usage estimate.
    out.pop("diagnostics", None)
    if isinstance(out.get("api"), str):
        out["api"] = "<API>"
    if isinstance(out.get("usage"), dict):
        out["usage"] = "<USAGE>"
    return out


def diff_events(ts_events, rust_events):
    """Compare the normalized event sequences; returns the failure text."""
    if len(ts_events) != len(rust_events):
        return (
            f"event count differs: ts={len(ts_events)} rust={len(rust_events)}\n"
            + "\n".join(
                difflib.unified_diff(
                    [json.dumps(event, sort_keys=True) for event in ts_events],
                    [json.dumps(event, sort_keys=True) for event in rust_events],
                    fromfile="ts",
                    tofile="rust",
                    lineterm="",
                    n=1,
                )
            )
        )
    failures = []
    for index, (ts_event, rust_event) in enumerate(zip(ts_events, rust_events)):
        if ts_event == rust_event:
            continue
        failures.append(
            f"event {index} differs:\n  ts:   {json.dumps(ts_event, sort_keys=True)}\n"
            f"  rust: {json.dumps(rust_event, sort_keys=True)}\n"
        )
    return "".join(failures)


def faux_script(responses, context_window):
    return {
        "engine": "faux",
        "modelId": "faux-1",
        "modelName": "Faux Model",
        "reasoning": False,
        "contextWindow": context_window,
        "responses": responses,
    }


# One run of a scenario: its own settings (written into the shared agent
# dir), faux script (written to its own script file), prompt list, and
# extra CLI args (the resume run passes --continue). Every run gets its
# own daemon socket so a previous run's worker never pins the session.
SCENARIOS = {
    "stream": {
        "runs": [
            {
                "settings": {"onboardingCompleted": True},
                "script": faux_script(["a streamed parity answer"], 128000),
                "prompts": ["hello parity"],
                "args": [],
            }
        ]
    },
    "threshold": {
        "runs": [
            {
                # Both sides' small-prompt context estimates sit under the 13k
                # headroom (ts ~11.4k, rust ~8.3k - each side estimates its
                # own prompt) and the ~6k-token crossing turn pushes both
                # past it without overflowing the 20k window: the seed turn
                # must stay below, the crossing turn must cross, on both
                # sides.
                "settings": {
                    "onboardingCompleted": True,
                    "compaction": {"enabled": True, "reserveTokens": 7000, "keepRecentTokens": 10},
                },
                # The fourth response serves the disposal drain's auto-refine
                # review (the compaction scheduled the trigger; no further
                # turn consumed it): a decline, so the drain's surface is
                # silence on both sides - the compact-trigger round itself
                # stays off the stream (TS dispose runs it after the
                # subscription teardown).
                "script": faux_script(
                    [
                        "seed reply",
                        "crossing reply",
                        "the compaction summary",
                        '{"shouldRefine": false, "rationale": "one-off tool output"}',
                    ],
                    20000,
                ),
                "prompts": ["seed turn", "crossing turn " + "x" * 24000],
                "args": [],
            }
        ]
    },
    "resume": {
        "runs": [
            {
                # Run one: a big seed turn pushes the settled usage past the
                # 18k headroom on both sides (each side estimates its own
                # context: ts ~2x its serialized input via cacheWrite, rust
                # ~23k), the small trailing turns keep the cut tail small,
                # and compaction is disabled - neither boundary fires, so
                # the session persists above the headroom.
                "settings": {
                    "onboardingCompleted": True,
                    "compaction": {"enabled": False, "reserveTokens": 7000, "keepRecentTokens": 10},
                    "autoRefine": {"enabled": False},
                },
                "script": faux_script(["seed reply", "k2", "k3", "k4"], 25000),
                # The trailing turn texts are sized so the keep-recent walk
                # (10 tokens) absorbs its budget at a USER message ("turn
                # two"), not an assistant one: a cut inside a turn is a
                # split-turn compaction, which TS backs with a SECOND
                # summarizer call + a "**Turn Context (split turn):**"
                # suffix the Rust compaction core does not produce yet
                # (a pa-core gap owned by the compaction lane, out of this
                # scenario's claim).
                "prompts": [
                    "big seed turn " + "x" * 60000,
                    "turn two",
                    "turn three",
                    "turn four",
                ],
                "args": [],
            },
            {
                # Run two: resume (`--continue`, fresh daemon socket) with
                # compaction enabled - the pre-turn boundary fires the
                # threshold arm before the admitted prompt, and the
                # compacted context (summary + the small kept tail) sits
                # back under the headroom, so the resumed turn's settled
                # boundary stays quiet on both sides.
                "settings": {
                    "onboardingCompleted": True,
                    "compaction": {"enabled": True, "reserveTokens": 7000, "keepRecentTokens": 10},
                    "autoRefine": {"enabled": False},
                },
                "script": faux_script(
                    ["the compaction summary", "recovered after the resume"], 25000
                ),
                "prompts": ["next prompt"],
                "args": ["--continue"],
            },
        ]
    },
    "compact-refine": {
        "runs": [
            {
                # The overflow compact-and-retry (willRetry=true) whose
                # retried turn settles, then the serialized checkpoint's
                # compact-trigger auto-refine: the scripted review approves,
                # the scripted refinement plan applies one memory edit, and
                # the refine events stream mid-run on both sides. The
                # overflow response is scripted (the classifier reads the
                # error text), the way the #211/#214 unit harnesses do.
                "settings": {
                    "onboardingCompleted": True,
                    "compaction": {"enabled": True, "reserveTokens": 1, "keepRecentTokens": 10},
                },
                "script": faux_script(
                    [
                        "seed reply",
                        {"text": "", "stopReason": "error", "errorMessage": "prompt is too long: 213462 tokens > 200000 maximum"},
                        "the compaction summary",
                        "recovered reply",
                        '{"shouldRefine": true, "rationale": "the overflow recovery tactic is reusable", "instructions": "record the tactic"}',
                        '{"summary":"note it","rationale":"repeated","expectedOutcome":"recall","edits":[{"action":"create","kind":"memory","id":"m1","title":"Tactic","content":"Use tactic A"}]}',
                    ],
                    200000,
                ),
                "prompts": ["seed turn " + "x" * 48000, "overflow probe " + "x" * 48000],
                "args": [],
            }
        ]
    },
    "compact-refine-decline": {
        "runs": [
            {
                # The same overflow shape with a declining review: the
                # cooldown stamps, no refinement rows, no refine events -
                # identical streams.
                "settings": {
                    "onboardingCompleted": True,
                    "compaction": {"enabled": True, "reserveTokens": 1, "keepRecentTokens": 10},
                },
                "script": faux_script(
                    [
                        "seed reply",
                        {"text": "", "stopReason": "error", "errorMessage": "prompt is too long: 213462 tokens > 200000 maximum"},
                        "the compaction summary",
                        "recovered reply",
                        '{"shouldRefine": false, "rationale": "one-off tool output"}',
                    ],
                    200000,
                ),
                "prompts": ["seed turn " + "x" * 48000, "overflow probe " + "x" * 48000],
                "args": [],
            }
        ]
    },
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--keep", action="store_true")
    parser.add_argument("--out", default=None)
    parser.add_argument("--only", default=None, choices=["ts", "rust"])
    parser.add_argument("--scenario", default=None, choices=sorted(SCENARIOS))
    args = parser.parse_args()

    # Fail fast before any launch: a non-TS `prime-agent` on PATH plays a
    # Rust build as the "ts" side and reports false divergences.
    if args.only in (None, "ts"):
        ts_identity.assert_ts_side_is_the_ts_product()

    base = tempfile.mkdtemp(prefix="print-json-parity-")
    out_dir = args.out or tempfile.mkdtemp(prefix="print-json-captures-")
    os.makedirs(out_dir, exist_ok=True)
    failures = []
    for name, scenario in SCENARIOS.items():
        if args.scenario and name != args.scenario:
            continue
        print(f"== scenario {name}")
        shared_cwd = os.path.join(base, f"{name}-cwd")
        os.makedirs(shared_cwd, exist_ok=True)
        # captures[binary][run_index] = the normalized events of that run.
        captures = {"ts": [], "rust": []}
        for binary in ("ts", "rust"):
            if args.only and binary != args.only:
                continue
            sandbox = prepare_sandbox(os.path.join(base, name), binary, scenario["runs"][0]["settings"])
            for index, run in enumerate(scenario["runs"]):
                # A later run may change the settings (the resume scenario
                # enables compaction for run two); the agent dir - and its
                # session store - stays shared across the runs.
                if index > 0:
                    write_settings(sandbox["agent"], run["settings"])
                script_path = os.path.join(base, f"{name}-run{index}-faux-script.json")
                with open(script_path, "w") as handle:
                    json.dump(run["script"], handle)
                # One daemon socket per run: a previous run's worker must
                # not pin the session the next run resumes.
                socket_path = os.path.join(sandbox["agent"], f"daemon-run{index}.sock")
                code, stdout, stderr = run_scenario(
                    binary, sandbox, script_path, run["prompts"], shared_cwd, run["args"], socket_path
                )
                suffix = "" if index == 0 else f"-run{index}"
                with open(os.path.join(out_dir, f"{binary}-{name}{suffix}.jsonl"), "w") as handle:
                    handle.write(stdout)
                with open(os.path.join(out_dir, f"{binary}-{name}{suffix}.stderr"), "w") as handle:
                    handle.write(stderr)
                captures[binary].append(normalize_events(stdout, os.path.join(base, name)))
        if not (captures["ts"] and captures["rust"]):
            continue
        for index in range(len(scenario["runs"])):
            label = name if index == 0 else f"{name}/run{index}"
            diff = diff_events(captures["ts"][index], captures["rust"][index])
            if diff:
                failures.append(label)
                print(diff)
                with open(os.path.join(out_dir, f"{label.replace('/', '-')}-diff.txt"), "w") as handle:
                    handle.write(diff)
                print(f"FAIL {label} (captures in {out_dir})")
            else:
                print(f"PASS {label} ({len(captures['ts'][index])} events, both sides)")
    if not args.keep:
        shutil.rmtree(base, ignore_errors=True)
    if failures:
        print(f"parity failures: {', '.join(failures)}")
        return 1
    print("print json event parity: PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
