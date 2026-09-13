"""Pure scorer for the swarm-fanout eval.

The scorer consumes a fixture manifest plus a recorded outcome dict and
applies the orchestration rubric. It never runs the agent; the runner
records the outcome and calls in, so every rule below is unit-testable
without a model or network.

Rubric (all four are required for a resolved run):
  - coverage: every shard's answer appears in combined-index.md and
    matches the machine-computed expected value from the fixture.
  - delegation evidence: at least one live depth-1 spawn edge per shard,
    each with a distinct childId and distinct name in the RLM ledger, and
    at least one sub-* child session dir per shard holding a session
    file. A parent that answers everything itself fails here.
  - dedup: no shard's worker name is spawned twice while another child
    for that shard is still live, and total depth-1 spawns stay within
    one retry per shard (2x the shard count).
  - receipt completeness: every depth-1 child (live or deleted) has a
    parent-visible reply or an explicit child failure/terminal notice in
    the parent transcript. A silently dropped child fails here.

Efficiency (tokens, turns, wall time) is informational until a
single-agent baseline datapoint exists.
"""

from __future__ import annotations

import json
import re
from collections import defaultdict

LEDGER_OPS = {"meta", "spawn", "rename", "delete"}
NOTICE_CUSTOM_TYPES = {"rlm_child_terminal_notice", "rlm_child_failure"}
REPLY_CUSTOM_TYPE = "agent_message"

# One "- <shard>: <answer>" bullet per line; the shard name may not
# contain a colon, the answer is everything after the first colon.
ANSWER_LINE = re.compile(r"^\s*[-*]\s+(.+?):\s*(.*?)\s*$")


def score_fixture(fixture: dict, outcome: dict) -> dict:
    """Apply the swarm-fanout rubric to one recorded outcome."""
    records, malformed = parse_ledger(outcome.get("ledger_text", ""))
    edges = replay_edges(records)
    answers = parse_answers(outcome.get("artifact_text", ""))
    coverage = score_coverage(fixture, answers)
    delegation = score_delegation(fixture, edges, outcome.get("child_session_dirs", []))
    dedup = score_dedup(fixture, records)
    receipts = score_receipts(edges, outcome.get("parent_transcript_text", ""))
    usage = outcome.get("usage") or {}
    resolved = (
        coverage["coverage"] and delegation["delegation_evidence"] and dedup["dedup"] and receipts["receipts"]
    )
    return {
        "fixture": fixture.get("name"),
        "resolved": resolved,
        "coverage": coverage["coverage"],
        "missing_shards": coverage["missing_shards"],
        "wrong_answers": coverage["wrong_answers"],
        "delegation_evidence": delegation["delegation_evidence"],
        "live_depth1_edges": delegation["live_depth1_edges"],
        "distinct_worker_names": delegation["distinct_worker_names"],
        "child_session_dirs": delegation["child_session_dirs"],
        "dedup": dedup["dedup"],
        "duplicate_spawns": dedup["duplicate_spawns"],
        "total_spawns": dedup["total_spawns"],
        "spawn_budget": dedup["spawn_budget"],
        "receipts": receipts["receipts"],
        "missing_receipts": receipts["missing_receipts"],
        "ledger_malformed_lines": malformed,
        "tokens_used": int(usage.get("tokens", 0)),
        "turns": int(usage.get("turns", 0)),
        "wall_time_s": float(outcome.get("wall_time_s", 0.0)),
    }


def parse_ledger(ledger_text: str) -> tuple[list[dict], int]:
    """Parse ledger JSONL, skipping blank and malformed lines.

    The product ledger fails closed on malformed lines; the scorer is a
    passive reader of a possibly torn artifact, so malformed lines are
    skipped and counted in the result for diagnosis.
    """
    records: list[dict] = []
    malformed = 0
    for line in ledger_text.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        try:
            record = json.loads(stripped)
        except ValueError:
            malformed += 1
            continue
        if isinstance(record, dict) and record.get("op") in LEDGER_OPS:
            records.append(record)
        else:
            malformed += 1
    return records, malformed


def replay_edges(records: list[dict]) -> dict[str, dict]:
    """Replay ledger records into edges, last-writer-wins per childId."""
    edges: dict[str, dict] = {}
    for record in records:
        op = record.get("op")
        child_id = record.get("childId")
        if not isinstance(child_id, str):
            continue
        if op == "spawn":
            edges[child_id] = {
                "childId": child_id,
                "parent": record.get("parent"),
                "child": record.get("child"),
                "depth": record.get("depth"),
                "name": record.get("name"),
                "deleted": None,
            }
        elif op == "rename":
            edge = edges.get(child_id)
            if edge is not None and isinstance(record.get("name"), str):
                edge["name"] = record["name"]
        elif op == "delete":
            edge = edges.get(child_id)
            if edge is not None:
                edge["deleted"] = record.get("reason")
    return edges


def parse_answers(artifact_text: str) -> dict[str, str]:
    """Map shard file name to answer from the combined-index bullets.

    Later duplicates of the same shard line are ignored: coverage needs
    one correct answer per shard, and a wrong duplicate of a correct line
    is still a fabricated-answer smell worth surfacing via the extra-line
    count only.
    """
    answers: dict[str, str] = {}
    extra_lines = 0
    for line in artifact_text.splitlines():
        match = ANSWER_LINE.match(line)
        if match is None:
            continue
        shard, answer = match.group(1), match.group(2)
        if shard in answers:
            extra_lines += 1
        answers[shard] = answer
    return answers


def score_coverage(fixture: dict, answers: dict[str, str]) -> dict:
    """Every shard's answer must match the machine-computed expected value."""
    missing: list[str] = []
    wrong: dict[str, dict] = {}
    for shard in fixture.get("shards", []):
        file = shard["file"]
        expected = str(shard["expected"])
        found = answers.get(file)
        if found is None:
            missing.append(file)
        elif found != expected:
            wrong[file] = {"expected": expected, "found": found}
    return {
        "coverage": not missing and not wrong,
        "missing_shards": missing,
        "wrong_answers": wrong,
    }


def score_delegation(fixture: dict, edges: dict[str, dict], child_session_dirs: list[str]) -> dict:
    """Spawn evidence must exist per shard: ledger edges plus child dirs.

    A parent that answers every shard without spawning one child per
    shard fails delegation even though coverage may pass - the no-spawn
    cheat is the blind-answer analog of swe-fix-loop's blind patch.
    """
    n_shards = len(fixture.get("shards", []))
    live_depth1 = [edge for edge in edges.values() if edge.get("depth") == 1 and edge.get("deleted") is None]
    distinct_child_ids = {edge["childId"] for edge in live_depth1}
    distinct_names = {edge.get("name") for edge in live_depth1 if edge.get("name")}
    passed = (
        len(distinct_child_ids) >= n_shards
        and len(distinct_names) >= n_shards
        and len(child_session_dirs) >= n_shards
    )
    return {
        "delegation_evidence": passed,
        "live_depth1_edges": len(distinct_child_ids),
        "distinct_worker_names": len(distinct_names),
        "child_session_dirs": len(child_session_dirs),
    }


def score_dedup(fixture: dict, records: list[dict]) -> dict:
    """No shard may be worked by two live children; one retry per shard.

    The replay mirrors the daemon's ledger semantics: a spawn adds the
    childId to its name's live set, a delete removes it. A spawn for a
    shard's worker name while another child already holds that name is
    duplicate work. Total depth-1 spawns may not exceed two per shard,
    which is the retry budget the task prompt grants.
    """
    n_shards = len(fixture.get("shards", []))
    shard_workers = {shard["worker"] for shard in fixture.get("shards", [])}
    live_by_name: dict[str, set[str]] = defaultdict(set)
    duplicates: list[dict] = []
    total_spawns = 0
    for record in records:
        op = record.get("op")
        child_id = record.get("childId")
        if not isinstance(child_id, str):
            continue
        if op == "spawn" and record.get("depth") == 1:
            total_spawns += 1
            name = record.get("name")
            if name in shard_workers and live_by_name.get(name):
                duplicates.append({"name": name, "childId": child_id})
            live_by_name[name].add(child_id)
        elif op == "delete":
            for child_ids in live_by_name.values():
                child_ids.discard(child_id)
        elif op == "rename":
            for child_ids in live_by_name.values():
                child_ids.discard(child_id)
            if isinstance(record.get("name"), str):
                live_by_name[record["name"]].add(child_id)
    budget = 2 * n_shards
    return {
        "dedup": not duplicates and total_spawns <= budget,
        "duplicate_spawns": duplicates,
        "total_spawns": total_spawns,
        "spawn_budget": budget,
    }


def score_receipts(edges: dict[str, dict], parent_transcript_text: str) -> dict:
    """Every depth-1 child must be accounted for in the parent transcript.

    A receipt is a child reply (an agent_message custom record naming the
    child's session id) or an explicit failure/terminal notice record
    naming the childId. Deleted children are still checked: deleting a
    silently failed child hides the drop, it does not receipt it.
    """
    replies: set[str] = set()
    notices: set[str] = set()
    for entry in _transcript_records(parent_transcript_text):
        if entry.get("customType") == REPLY_CUSTOM_TYPE:
            details = entry.get("details") or {}
            if details.get("fromRelationship") not in (None, "child"):
                continue
            sender = details.get("from") or {}
            session_id = sender.get("sessionId")
            if isinstance(session_id, str) and session_id:
                replies.add(session_id)
        elif entry.get("customType") in NOTICE_CUSTOM_TYPES:
            details = entry.get("details") or {}
            child_id = details.get("childId")
            if isinstance(child_id, str) and child_id:
                notices.add(child_id)
    missing: list[dict] = []
    for edge in edges.values():
        if edge.get("depth") != 1:
            continue
        child_session_id = _session_id_from_file(edge.get("child"))
        if child_session_id in replies or edge["childId"] in notices:
            continue
        missing.append({"childId": edge["childId"], "name": edge.get("name")})
    return {"receipts": not missing, "missing_receipts": missing}


def summarize_usage(session_text: str) -> dict:
    """Sum assistant tokens and assistant turns from the session JSONL.

    In --mode json each completed assistant message is emitted once on
    message_end; turn_end repeats the same message, so only message_end
    events are counted.
    """
    tokens = 0
    turns = 0
    for line in session_text.splitlines():
        try:
            entry = json.loads(line)
        except ValueError:
            continue
        if not isinstance(entry, dict) or entry.get("type") != "message_end":
            continue
        message = entry.get("message")
        if not isinstance(message, dict) or message.get("role") != "assistant":
            continue
        usage = message.get("usage") or {}
        total = usage.get("totalTokens", 0)
        if not isinstance(total, int) or total <= 0:
            continue
        tokens += total
        turns += 1
    return {"tokens": tokens, "turns": turns}


def _transcript_records(transcript_text: str):
    """Yield parsed custom_message records from a session JSONL."""
    for line in transcript_text.splitlines():
        try:
            entry = json.loads(line)
        except ValueError:
            continue
        if isinstance(entry, dict) and entry.get("type") == "custom_message":
            yield entry


def _session_id_from_file(session_file: object) -> str | None:
    """The ledger child path is a <session-id>.jsonl file; extract the id."""
    if not isinstance(session_file, str):
        return None
    name = session_file.rsplit("/", 1)[-1]
    return name[:-6] if name.endswith(".jsonl") else name
