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
    (cron_list) are reported as expected diffs; the agent_messages_*
    selector-less arms landed with wave b7 and must match.

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
    ("no-selector command now matches TS (agent_messages_status)",
     {"type": "agent_messages_status"}),
    ("no-selector command now matches TS (agent_messages_pause)",
     {"type": "agent_messages_pause"}),
    ("no-selector command now matches TS (agent_messages_resume)",
     {"type": "agent_messages_resume"}),
    ("no-selector command: TS arm is a later wave (cron_list)",
     {"type": "cron_list"}),
    # Waves b2-b5 (worker commands): a bogus selector routes identically
    # on both supervisors - the exact TS refusal before any worker sees
    # the command.
    ("b2 getter routes (get_connection_state)",
     {"type": "get_connection_state", "activeSessionId": "bogus-1"}),
    ("b2 getter routes (get_rlm_children)",
     {"type": "get_rlm_children", "activeSessionId": "bogus-1"}),
    ("b2 getter routes (get_context_tree)",
     {"type": "get_context_tree", "activeSessionId": "bogus-1"}),
    ("b2 getter routes (get_commands)",
     {"type": "get_commands", "activeSessionId": "bogus-1"}),
    ("b2 getter routes (get_resource_snapshot)",
     {"type": "get_resource_snapshot", "activeSessionId": "bogus-1"}),
    ("b2 getter routes (get_session_context)",
     {"type": "get_session_context", "activeSessionId": "bogus-1"}),
    ("b2 getter routes (get_system_prompt)",
     {"type": "get_system_prompt", "activeSessionId": "bogus-1"}),
    ("b2 getter routes (get_tool_definition)",
     {"type": "get_tool_definition", "activeSessionId": "bogus-1", "name": "bash"}),
    ("b2 getter routes (get_rlm_max_depth_status)",
     {"type": "get_rlm_max_depth_status", "activeSessionId": "bogus-1"}),
    ("b2 getter routes (get_model_catalog)",
     {"type": "get_model_catalog", "activeSessionId": "bogus-1"}),
    ("b2 getter routes (get_available_models)",
     {"type": "get_available_models", "activeSessionId": "bogus-1"}),
    ("b3 switch routes (cycle_model)",
     {"type": "cycle_model", "activeSessionId": "bogus-1"}),
    ("b3 switch routes (set_scoped_models)",
     {"type": "set_scoped_models", "activeSessionId": "bogus-1", "scopedModels": []}),
    ("b3 switch routes (cycle_thinking_level)",
     {"type": "cycle_thinking_level", "activeSessionId": "bogus-1"}),
    ("b3 switch routes (set_service_tier)",
     {"type": "set_service_tier", "activeSessionId": "bogus-1", "serviceTier": "priority"}),
    ("b3 switch routes (set_transport)",
     {"type": "set_transport", "activeSessionId": "bogus-1", "transport": "auto"}),
    ("b3 switch routes (set_steering_mode)",
     {"type": "set_steering_mode", "activeSessionId": "bogus-1", "mode": "all"}),
    ("b3 switch routes (set_follow_up_mode)",
     {"type": "set_follow_up_mode", "activeSessionId": "bogus-1", "mode": "all"}),
    ("b3 switch routes (set_auto_retry)",
     {"type": "set_auto_retry", "activeSessionId": "bogus-1", "enabled": True}),
    ("b3 switch routes (abort_retry)",
     {"type": "abort_retry", "activeSessionId": "bogus-1"}),
    ("b4 command routes (append_custom_message)",
     {"type": "append_custom_message", "activeSessionId": "bogus-1",
      "message": {"customType": "x", "content": "y", "display": True}}),
    ("b4 command routes (restore_next_turn)",
     {"type": "restore_next_turn", "activeSessionId": "bogus-1", "messages": []}),
    ("b4 command routes (restore_actions)",
     {"type": "restore_actions", "activeSessionId": "bogus-1",
      "snapshot": {"formatVersion": 1, "actions": []}}),
    ("b4 command routes (refine)",
     {"type": "refine", "activeSessionId": "bogus-1", "instructions": "tidy"}),
    ("b4 command routes (reload)",
     {"type": "reload", "activeSessionId": "bogus-1"}),
    ("b4 command routes (extension_ui_response)",
     {"type": "extension_ui_response", "activeSessionId": "bogus-1",
      "requestId": "ui-1", "response": {"confirmed": True}}),
    ("b5 bash routes (execute_bash)",
     {"type": "execute_bash", "activeSessionId": "bogus-1", "command": "echo hi"}),
    ("b5 bash routes (execute_bash_and_wait)",
     {"type": "execute_bash_and_wait", "activeSessionId": "bogus-1", "command": "echo hi"}),
    ("b5 bash routes (abort_bash)",
     {"type": "abort_bash", "activeSessionId": "bogus-1"}),
    # Waves b6-b9: every new command routes; a bogus selector refuses with
    # the exact TS error before any worker sees the command. The
    # selector-less agent_messages_* forms match the TS supervisor arms
    # (wave b7); the ownership/lifecycle commands (b9) answer the TS
    # unknown-session error, and retry_worker resolves before its arm.
    ("b6 rlm routes (cancel_rlm_child)",
     {"type": "cancel_rlm_child", "activeSessionId": "bogus-1", "childId": "c"}),
    ("b6 rlm routes (delete_rlm_subagent)",
     {"type": "delete_rlm_subagent", "activeSessionId": "bogus-1", "childId": "c"}),
    ("b6 rlm routes (set_rlm_max_depth)",
     {"type": "set_rlm_max_depth", "activeSessionId": "bogus-1", "maxDepth": 3}),
    ("b6 rlm routes (get_rlm_max_depth_status)",
     {"type": "get_rlm_max_depth_status", "activeSessionId": "bogus-1"}),
    ("b7 clear routes (agent_messages_clear)",
     {"type": "agent_messages_clear", "activeSessionId": "bogus-1"}),
    ("b8 pause routes (acquire_session_input_pause)",
     {"type": "acquire_session_input_pause", "activeSessionId": "bogus-1", "leaseKey": "k"}),
    ("b8 pause routes (release_session_input_pause)",
     {"type": "release_session_input_pause", "activeSessionId": "bogus-1", "pauseId": "p"}),
    ("b9 ownership routes (complete_owned_session)",
     {"type": "complete_owned_session", "activeSessionId": "bogus-1"}),
    ("b9 ownership routes (promote_owned_session)",
     {"type": "promote_owned_session", "activeSessionId": "bogus-1"}),
    ("b9 navigation routes (new_session)",
     {"type": "new_session", "activeSessionId": "bogus-1"}),
    ("b9 navigation routes (switch_session)",
     {"type": "switch_session", "activeSessionId": "bogus-1", "sessionPath": "/tmp/x.jsonl"}),
    ("b9 navigation routes (import_jsonl)",
     {"type": "import_jsonl", "activeSessionId": "bogus-1", "inputPath": "/tmp/in.jsonl"}),
    ("b9 navigation routes (export_html)",
     {"type": "export_html", "activeSessionId": "bogus-1", "outputPath": "/tmp/out.html"}),
    ("b9 navigation routes (export_jsonl)",
     {"type": "export_jsonl", "activeSessionId": "bogus-1", "outputPath": "/tmp/out.jsonl"}),
    ("b9 admission routes (cancel_prompt_admission)",
     {"type": "cancel_prompt_admission", "activeSessionId": "bogus-1", "admissionId": "a1"}),
    ("b9 recovery routes (retry_worker)",
     {"type": "retry_worker", "activeSessionId": "bogus-1"}),
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
