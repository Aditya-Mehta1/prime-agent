#!/usr/bin/env python3
"""Daemon protocol-breadth wire parity (roadmap item 7, wave-1 verifier).

Runs the installed TS `prime-agent` supervisor and the Rust `pa-daemon`
side by side on isolated temp HOME/TMPDIR state, sends the same command
envelopes to both, and byte-compares the response shapes (dynamic fields
normalized away). The lane contract (docs/protocol-breadth-audit.md):

  - an unknown command type fails with the exact TS error string and the
    offending type echoed in the response `command` field;
  - a previously-rejected TS command type (resume_queue,
    mutate_queued_message, get_connection_state, ...) routes exactly like
    TS: same `Unknown active session: <id>` refusal for a bogus selector;
  - selector-less commands whose TS supervisor arms are later waves
    (agent_messages_status, cron_list) are reported as expected diffs.

Usage: python3 scripts/protocol_breadth_parity.py [path-to-pa-daemon]
(the default is target/debug/pa-daemon relative to the repo root).
"""
import json, os, socket, subprocess, sys, tempfile, time, uuid

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TS = "prime-agent"
RS = sys.argv[1] if len(sys.argv) > 1 else os.path.join(REPO, "target", "debug", "pa-daemon")
SCRUB = [
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER",
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_TOKEN",
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_ACTIVE_SESSION_ID",
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_INSTANCE_ID",
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_RECOVERY_JOURNAL",
    "PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_SOCKET",
    "PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL",
    "PRIME_AGENT_INTERNAL_SESSION_LEASES",
    "PRIME_AGENT_INTERNAL_SESSION_LEASE_OWNER_ID",
]

def _start(binary, sock, agent_dir):
    env = {k: v for k, v in os.environ.items() if k not in SCRUB}
    # Fresh HOME keeps the daemon off shared box state (supervisor-owner
    # registry, daemon sockets), like the parity-battery sandboxes.
    env["HOME"] = agent_dir + "-home"
    os.makedirs(env["HOME"], exist_ok=True)
    env["PRIME_AGENT_CODING_AGENT_DIR"] = agent_dir
    env["PRIME_AGENT_DISABLE_ANALYTICS"] = "1"
    # Isolated TMPDIR keeps the default daemon-socket dir (and the legacy
    # supervisor registry under it) away from other lanes' daemons on the
    # shared box; the ownership check refuses to start otherwise.
    env["TMPDIR"] = agent_dir + "-tmp"
    os.makedirs(env["TMPDIR"], exist_ok=True)
    # Per-user global supervisor registry: point it at the temp home so
    # the ownership check cannot see other lanes' daemons on this box.
    env["PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_REGISTRY_DIR"] = (
        env["HOME"] + "/.prime/supervisor-owners"
    )
    child = subprocess.Popen(
        [binary, "supervisor", "--socket", sock, "--agent-dir", agent_dir],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env,
    )
    deadline = time.time() + 15
    while time.time() < deadline:
        if os.path.exists(sock) or os.path.exists(os.path.join(
            env.get("TMPDIR", ""), "prime-agent-1000", "daemon.sock")):
            return child
        if child.poll() is not None:
            raise SystemExit(f"{binary} exited with {child.returncode}")
        time.sleep(0.05)
    raise SystemExit(f"{binary} socket never appeared")

def start(binary, sock, agent_dir):
    """Start one supervisor; returns (child, socket_path).

    The TS supervisor binds its default socket under $TMPDIR
    (prime-agent-1000/daemon.sock) even with --socket given; the Rust
    supervisor honors --socket. Both get an isolated TMPDIR so no two
    daemons in this run (or on the box) collide.
    """
    child = _start(binary, sock, agent_dir)
    ts_default = os.path.join(agent_dir + "-tmp", "prime-agent-1000", "daemon.sock")
    return child, (ts_default if binary == TS else sock)

def connect(sock):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(10)
    s.connect(sock)
    f = s.makefile("rw")
    hello = json.loads(f.readline())
    return s, f, hello

def send(f, cid, command):
    if command.get("id") == "bare":
        # A bare command (no envelope): the TS supervisor rejects it with
        # the protocol error under command:"parse".
        env = command
    else:
        env = {"type": "command", "id": cid,
               "protocol": {"name": "prime-agent.daemon", "version": 7},
               "command": command}
    f.write(json.dumps(env) + "\n")
    f.flush()
    while True:
        line = f.readline()
        if not line:
            raise SystemExit("connection closed")
        value = json.loads(line)
        if value.get("type") == "response" and value.get("id") == cid:
            return value
        # A bare command's failure still carries the client's own id.

def norm(response):
    """Normalize dynamic fields; compare the wire shape."""
    data = response.pop("data", None)
    out = {
        "id": response.get("id"),
        "command": response.get("command"),
        "success": response.get("success"),
        "error": response.get("error"),
    }
    if response.get("errorInfo") is not None:
        out["errorInfo"] = response["errorInfo"]
    return out, data

CASES = [
    ("unknown type keeps the TS error",
     {"type": "bogus_command"}),
    ("bare command answers the TS parse failure",
     {"type": "list", "id": "bare"}),
    ("previously-rejected type now routes (resume_queue)",
     {"type": "resume_queue", "activeSessionId": "bogus-1"}),
    ("previously-rejected type now routes (mutate_queued_message)",
     {"type": "mutate_queued_message", "activeSessionId": "bogus-1",
      "lane": "steering", "index": 0, "expectedText": "x",
      "mutation": {"type": "delete"}}),
    ("previously-rejected type now routes (get_connection_state)",
     {"type": "get_connection_state", "activeSessionId": "bogus-1"}),
    ("no-selector command: TS arm is a later wave (agent_messages_status)",
     {"type": "agent_messages_status"}),
    ("no-selector command: TS arm is a later wave (cron_list)",
     {"type": "cron_list"}),
]

tmp = tempfile.mkdtemp(prefix="pa-parity-")
ts_sock = os.path.join(tmp, "ts.sock")
rs_sock = os.path.join(tmp, "rs.sock")
ts_child, ts_path = start(TS, ts_sock, os.path.join(tmp, "ts-agent"))
rs_child, rs_path = start(RS, rs_sock, os.path.join(tmp, "rs-agent"))
try:
    ts_f = connect(ts_path)[1]
    rs_f = connect(rs_path)[1]
    failures = 0
    for label, command in CASES:
        cid = "bare" if command.get("id") == "bare" else "c-" + uuid.uuid4().hex[:8]
        ts_resp = send(ts_f, cid, command)
        rs_resp = send(rs_f, cid, command)
        ts_norm, ts_data = norm(ts_resp)
        rs_norm, rs_data = norm(rs_resp)
        ok = ts_norm == rs_norm
        expected = "later wave" in label
        status = ("MATCH " if ok else ("EXPECTED-DIFF " if expected else "UNEXPECTED-DIFF "))
        print(f"{status} {label}")
        print(f"   TS: {json.dumps(ts_norm)}")
        print(f"   RS: {json.dumps(rs_norm)}")
        # An expected diff is the staged remainder (the TS supervisor arm
        # for the selector-less form lands with its breadth wave); it
        # must not fail the verifier, but it is always printed.
        if ts_norm != rs_norm and not expected:
            failures += 1
    print(f"\n{'ALL MATCH' if failures == 0 else f'{failures} UNEXPECTED DIFFS'}")
    sys.exit(1 if failures else 0)
finally:
    for child in (ts_child, rs_child):
        child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
