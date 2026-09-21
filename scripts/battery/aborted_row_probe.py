
"""Ground-truth probe for the aborted-turn row (lane aborted-row):
what each side's daemon broadcasts + persists when a turn is aborted
mid-provider-wait (plain `abort`), and what the compact abort shows."""
import json, sys, time, shutil
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import batterylib as B

RUN = Path("/tmp/aborted-row-probe")

def make_side(name: str, binary: str) -> B.Side:
    root = RUN / name
    if root.exists():
        shutil.rmtree(root)
    root.mkdir(parents=True)
    agent = root / "agent"
    work = root / "work"
    work.mkdir(parents=True)
    agent.mkdir(parents=True)
    mock = B.MockProvider(root, [])
    mock.set_responses([{"text": "statusline filler"}])
    mock.start()
    tmpdir = Path("/tmp") / f"abrt-probe-{name}"
    if tmpdir.exists():
        shutil.rmtree(tmpdir)
    tmpdir.mkdir(parents=True)
    side = B.Side(name=name, binary=binary, root=root, agent_dir=agent,
                  work_dir=work, daemon_socket=root / "daemon.sock", mock=mock)
    side.env = B.scrubbed_env(agent, tmpdir, {"PRIME_DISABLE_AUTO_COMPACTION": "1"})
    side.write_models_json()
    return side

def probe(side: B.Side, abort_command: dict, key: str) -> None:
    side.mock.set_responses(
        [{"text": "statusline filler"}],
        queues=[{
            "name": "aborted-row-probe",
            "matchModels": ["mock-1"],
            "responses": [{"text": "held reply", "delayMs": 15000}],
        }],
    )
    side.start_daemon()
    wire = B.Wire(side.daemon_socket)
    create = wire.request("c1", {
        "type": "create", "name": f"aborted-row-probe-{key}",
        "config": {
            "cwd": str(side.work_dir),
            "sessionDir": str(side.agent_dir / "sessions"),
            "provider": "prime-inference",
            "model": "mock-1",
            "executionMode": "print",
        },
    }, timeout=120)
    session_id = create.get("data", {}).get("activeSessionId") or create.get("data", {}).get("id")
    attacher = B.Wire(side.daemon_socket)
    attacher.request("a1", {"type": "attach", "activeSessionId": session_id}, timeout=60)
    attacher.events.clear()
    prompt = wire.request("p1", {
        "type": "prompt", "activeSessionId": session_id,
        "message": "held turn for the abort probe",
    }, timeout=60)
    time.sleep(0.8)  # mid-provider-wait (the 15s hold)
    abort = wire.request("ab1", dict(abort_command, activeSessionId=session_id), timeout=60)
    idle = wire.request("w1", {"type": "wait_for_idle", "activeSessionId": session_id}, timeout=120)
    attacher.drain(2.0)
    rows = []
    for frame in attacher.events:
        ev = frame.get("event") or {}
        if ev.get("type") in ("message_start", "message_end", "message_update", "turn_end", "agent_end"):
            m = ev.get("message") or {}
            rows.append({
                "wire": ev.get("type"),
                "role": m.get("role"),
                "stopReason": m.get("stopReason"),
                "errorMessage": m.get("errorMessage"),
                "text": (m.get("content") if isinstance(m.get("content"), str) else
                         "".join(p.get("text", "") for p in (m.get("content") or []) if isinstance(p, dict))),
            })
    (side.root / f"{key}-wire-events.json").write_text(json.dumps(rows, indent=1))
    (side.root / f"{key}-raw-frames.json").write_text(json.dumps(attacher.events, indent=1))
    # the durable store
    store_rows = []
    for path in side.session_files():
        for line in path.read_text().splitlines():
            try:
                entry = json.loads(line)
            except Exception:
                continue
            if entry.get("type") == "message":
                m = entry.get("fields", {}).get("message") or entry.get("message") or {}
                store_rows.append({
                    "role": m.get("role"),
                    "stopReason": m.get("stopReason"),
                    "errorMessage": m.get("errorMessage"),
                })
    (side.root / f"{key}-store-rows.json").write_text(json.dumps(store_rows, indent=1))
    print(f"[{side.name}/{key}] abort={abort.get('success')} idle={idle.get('success')}")
    print(f"[{side.name}/{key}] wire: {json.dumps(rows)[:600]}")
    print(f"[{side.name}/{key}] store: {json.dumps([r for r in store_rows if r.get('role') == 'assistant'])[:600]}")
    attacher.close()
    wire.close()
    side.stop_daemon()

def main():
    ts_bin = "prime-agent"
    rust_bin = "/home/ubuntu/lane-worktrees/aborted-row/target/release/prime-agent"
    cases = [
        ("abort", {"type": "abort"}),
        ("compact", {"type": "compact"}),
        ("kill", {"type": "kill"}),
        ("abort_and_clear_queue", {"type": "abort_and_clear_queue"}),
    ]
    for name, binary in (("ts", ts_bin), ("rust", rust_bin)):
        for key, command in cases:
            side = make_side(f"{name}-{key}", binary)
            try:
                probe(side, command, key)
            finally:
                side.stop_daemon()

if __name__ == "__main__":
    main()
