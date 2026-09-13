"""Model-free self-tests for the swarm-fanout eval harness.

Validates fixture integrity (expected answers recomputed independently
from the shard data, task prompts naming every shard) and the scorer
rubric with synthetic ledgers, transcripts, and artifacts - including
the no-spawn, double-spawn, fabricated-answer, and silent-drop cheats.
The runner is exercised end to end with a stub agent binary that writes
a full fan-out artifact set. No agent, model, or network is invoked.
"""

from __future__ import annotations

import contextlib
import hashlib
import io
import json
import os
import shutil
import socket
import sys
import tempfile
import threading
import unittest
from pathlib import Path

HARNESS = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(HARNESS))
import runner  # noqa: E402
import scorer  # noqa: E402

FIXTURES = HARNESS / "fixtures"


def fixture_manifest(name: str) -> dict:
    return json.loads((FIXTURES / name / "fixture.json").read_text())


def recompute_answer(fixture_name: str, shard_file: str, question: str) -> int:
    """Recompute a shard's expected answer independently of the manifest."""
    shard_path = FIXTURES / fixture_name / "shards" / shard_file
    if fixture_name == "json-events":
        records = [json.loads(line) for line in shard_path.read_text().splitlines() if line.strip()]
        if question == 'How many records have action "deploy"?':
            return sum(1 for record in records if record["action"] == "deploy")
        if question == 'What is the total of the bytes field over all records with action "upload"?':
            return sum(record["bytes"] for record in records if record["action"] == "upload")
        if question == 'How many distinct users have at least one record with action "login"?':
            return len({record["user"] for record in records if record["action"] == "login"})
        if question == "What is the maximum value of the bytes field?":
            return max(record["bytes"] for record in records)
        if question == 'How many records have action "logout"?':
            return sum(1 for record in records if record["action"] == "logout")
        if question == "How many records have bytes greater than 500?":
            return sum(1 for record in records if record["bytes"] > 500)
        if question == 'What is the total of the bytes field over all records with action "deploy"?':
            return sum(record["bytes"] for record in records if record["action"] == "deploy")
        if question == "How many distinct users appear in the file?":
            return len({record["user"] for record in records})
    rows = [line.split(",") for line in shard_path.read_text().splitlines() if line.strip()]
    header, data = rows[0], rows[1:]
    columns = {name: index for index, name in enumerate(header)}
    if question == "What is the maximum value of the cpu column?":
        return max(int(row[columns["cpu"]]) for row in data)
    if question == "What is the total of the latency_ms column over rows with cpu >= 90?":
        return sum(int(row[columns["latency_ms"]]) for row in data if int(row[columns["cpu"]]) >= 90)
    if question == "How many rows have memory > 80?":
        return sum(1 for row in data if int(row[columns["memory"]]) > 80)
    if question == "What is the total of the latency_ms column over all rows?":
        return sum(int(row[columns["latency_ms"]]) for row in data)
    if question == "How many rows have cpu >= 90?":
        return sum(1 for row in data if int(row[columns["cpu"]]) >= 90)
    if question == "What is the total of the memory column over all rows?":
        return sum(int(row[columns["memory"]]) for row in data)
    if question == "What is the maximum memory among rows with latency_ms < 10?":
        return max(int(row[columns["memory"]]) for row in data if int(row[columns["latency_ms"]]) < 10)
    if question == "What is the total of the cpu column over rows with memory >= 50?":
        return sum(int(row[columns["cpu"]]) for row in data if int(row[columns["memory"]]) >= 50)
    raise AssertionError(f"unknown question: {question}")


# --- synthetic artifact builders -------------------------------------------


def spawn_line(child_id: str, name: str, child_file: str, parent_file: str, depth: int = 1) -> str:
    return json.dumps(
        {
            "v": 1,
            "op": "spawn",
            "at": "2026-09-12T00:00:00.000Z",
            "childId": child_id,
            "parent": parent_file,
            "child": child_file,
            "depth": depth,
            "name": name,
        }
    )


def delete_line(child_id: str, child_file: str, reason: str = "parent-teardown") -> str:
    return json.dumps(
        {
            "v": 1,
            "op": "delete",
            "at": "2026-09-12T00:00:01.000Z",
            "childId": child_id,
            "child": child_file,
            "reason": reason,
        }
    )


def rename_line(child_id: str, child_file: str, name: str) -> str:
    return json.dumps(
        {
            "v": 1,
            "op": "rename",
            "at": "2026-09-12T00:00:01.000Z",
            "childId": child_id,
            "child": child_file,
            "name": name,
        }
    )


def reply_line(child_session_id: str, session_name: str = "worker") -> str:
    """An agent_message custom record delivered into the parent transcript."""
    return json.dumps(
        {
            "type": "custom_message",
            "customType": "agent_message",
            "content": f"[agent-message from child:{session_name}]\n\n17",
            "display": True,
            "details": {
                "id": "agentmsg-1",
                "message": "17",
                "from": {
                    "activeSessionId": "active-1",
                    "sessionId": child_session_id,
                    "sessionName": session_name,
                    "runtimeKind": "subagent",
                },
                "fromRelationship": "child",
                "target": {"activeSessionId": "active-parent", "sessionId": "parent-id"},
            },
            "id": "entry-1",
            "parentId": None,
            "timestamp": "2026-09-12T00:00:02.000Z",
        }
    )


def notice_line(child_id: str, session_name: str, kind: str = "completed_without_reply") -> str:
    return json.dumps(
        {
            "type": "custom_message",
            "customType": "rlm_child_terminal_notice",
            "content": f"[child-exited: no-reply child:{session_name}]",
            "display": True,
            "details": {"kind": kind, "childId": child_id, "sessionName": session_name},
            "id": "entry-2",
            "parentId": "entry-1",
            "timestamp": "2026-09-12T00:00:03.000Z",
        }
    )


def failure_line(child_id: str, session_name: str) -> str:
    return json.dumps(
        {
            "type": "custom_message",
            "customType": "rlm_child_failure",
            "content": f"[child-failed child:{session_name}]\n\nboom",
            "display": True,
            "details": {"childId": child_id, "sessionName": session_name, "error": "boom"},
            "id": "entry-3",
            "parentId": "entry-2",
            "timestamp": "2026-09-12T00:00:04.000Z",
        }
    )


def full_ledger(fixture: dict, parent_file: str, artifacts_root: str) -> tuple[str, list[dict]]:
    """A well-formed per-shard fan-out ledger plus its child metadata."""
    lines = [
        json.dumps({"v": 1, "op": "meta", "at": "2026-09-12T00:00:00.000Z", "sessionsDir": "/tmp/sessions"})
    ]
    children = []
    for shard in fixture["shards"]:
        child_id = f"sub-{hashlib.sha256(shard['worker'].encode()).hexdigest()[:8]}"
        child_session = f"child-{hashlib.sha256(shard['file'].encode()).hexdigest()[:12]}"
        child_file = f"{artifacts_root}/parent-id/{child_id}/{child_session}.jsonl"
        lines.append(spawn_line(child_id, shard["worker"], child_file, parent_file))
        children.append(
            {"shard": shard, "childId": child_id, "session": child_session, "child_file": child_file}
        )
    return "\n".join(lines) + "\n", children


def full_transcript(children: list[dict]) -> str:
    """A parent transcript in which every listed child replied with its answer."""
    header = {"type": "session", "version": 3, "id": "parent-id", "timestamp": "2026-09-12T00:00:00.000Z"}
    lines = [json.dumps(header)]
    for child in children:
        lines.append(reply_line(child["session"], child["shard"]["worker"]))
    return "\n".join(lines) + "\n"


def full_index(fixture: dict, wrong: dict | None = None) -> str:
    wrong = wrong or {}
    lines = []
    for shard in fixture["shards"]:
        answer = wrong.get(shard["file"], str(shard["expected"]))
        lines.append(f"- {shard['file']}: {answer}")
    return "\n".join(lines) + "\n"


def ledger_spawn_records(ledger_text: str) -> list[dict]:
    return [
        json.loads(line)
        for line in ledger_text.splitlines()
        if line.strip() and json.loads(line).get("op") == "spawn"
    ]


def passing_outcome(fixture: dict) -> dict:
    ledger_text, children = full_ledger(fixture, "/tmp/sessions/parent-id.jsonl", "/tmp/artifacts")
    return {
        "ledger_text": ledger_text,
        "parent_transcript_text": full_transcript(children),
        "child_session_dirs": [child["childId"] for child in children],
        "artifact_text": full_index(fixture),
        "usage": {"tokens": 40_000, "turns": 12},
        "wall_time_s": 123.4,
    }


class FixtureIntegrity(unittest.TestCase):
    def test_json_events_expected_answers_recompute(self):
        manifest = fixture_manifest("json-events")
        for shard in manifest["shards"]:
            recomputed = recompute_answer("json-events", shard["file"], shard["question"])
            self.assertEqual(recomputed, shard["expected"], shard["file"])

    def test_csv_metrics_expected_answers_recompute(self):
        manifest = fixture_manifest("csv-metrics")
        for shard in manifest["shards"]:
            recomputed = recompute_answer("csv-metrics", shard["file"], shard["question"])
            self.assertEqual(recomputed, shard["expected"], shard["file"])

    def test_task_txt_names_every_shard_and_question(self):
        for name in ("json-events", "csv-metrics"):
            manifest = fixture_manifest(name)
            task = (FIXTURES / name / "task.txt").read_text()
            for shard in manifest["shards"]:
                self.assertIn(f"shards/{shard['file']}: {shard['question']}", task, name)

    def test_worker_names_and_artifact_are_wellformed(self):
        for name in ("json-events", "csv-metrics"):
            manifest = fixture_manifest(name)
            self.assertEqual(manifest["artifact"], "combined-index.md")
            self.assertEqual(len(manifest["shards"]), 8)
            workers = [shard["worker"] for shard in manifest["shards"]]
            self.assertEqual(len(set(workers)), len(workers), name)
            for shard in manifest["shards"]:
                stem = shard["file"].rsplit(".", 1)[0]
                self.assertEqual(shard["worker"], f"worker-{stem}")


class ScorerTests(unittest.TestCase):
    def setUp(self):
        self.fixture = fixture_manifest("json-events")

    def test_full_resolution(self):
        result = scorer.score_fixture(self.fixture, passing_outcome(self.fixture))
        self.assertTrue(result["resolved"])
        self.assertTrue(result["coverage"])
        self.assertTrue(result["delegation_evidence"])
        self.assertTrue(result["dedup"])
        self.assertTrue(result["receipts"])
        self.assertEqual(result["tokens_used"], 40_000)
        self.assertEqual(result["turns"], 12)
        self.assertEqual(result["wall_time_s"], 123.4)

    def test_no_spawn_blocks_resolution(self):
        # The parent answered everything itself: no ledger, no children.
        outcome = passing_outcome(self.fixture)
        outcome["ledger_text"] = ""
        outcome["parent_transcript_text"] = ""
        outcome["child_session_dirs"] = []
        result = scorer.score_fixture(self.fixture, outcome)
        self.assertTrue(result["coverage"])
        self.assertFalse(result["delegation_evidence"])
        self.assertFalse(result["resolved"])

    def test_fabricated_and_missing_answers_block_coverage(self):
        outcome = passing_outcome(self.fixture)
        wrong = {self.fixture["shards"][0]["file"]: "999999"}
        outcome["artifact_text"] = full_index(self.fixture, wrong)
        result = scorer.score_fixture(self.fixture, outcome)
        self.assertFalse(result["coverage"])
        self.assertEqual(len(result["wrong_answers"]), 1)
        # A dropped shard bullet is also a coverage failure.
        partial = full_index(self.fixture)
        outcome["artifact_text"] = "\n".join(partial.splitlines()[:-1]) + "\n"
        result = scorer.score_fixture(self.fixture, outcome)
        self.assertFalse(result["coverage"])
        self.assertEqual(len(result["missing_shards"]), 1)

    def test_delegation_requires_child_session_dirs(self):
        # Ledger edges exist but the child session dirs were never written.
        outcome = passing_outcome(self.fixture)
        outcome["child_session_dirs"] = []
        result = scorer.score_fixture(self.fixture, outcome)
        self.assertFalse(result["delegation_evidence"])
        self.assertFalse(result["resolved"])

    def test_delegation_requires_distinct_names(self):
        outcome = passing_outcome(self.fixture)
        # Rename every child to one name: eight live edges, one distinct
        # name, so the fan-out degenerated to a pile of duplicate workers.
        lines = outcome["ledger_text"].splitlines()
        renames = [
            rename_line(record["childId"], record["child"], "worker-same")
            for record in ledger_spawn_records(outcome["ledger_text"])
        ]
        outcome["ledger_text"] = "\n".join(lines + renames) + "\n"
        result = scorer.score_fixture(self.fixture, outcome)
        self.assertFalse(result["delegation_evidence"])
        self.assertEqual(result["distinct_worker_names"], 1)

    def test_double_spawn_blocks_dedup(self):
        outcome = passing_outcome(self.fixture)
        lines = outcome["ledger_text"].splitlines()
        first = ledger_spawn_records(outcome["ledger_text"])[0]
        duplicate = spawn_line("sub-duplicate00", first["name"], first["child"] + "2", first["parent"])
        outcome["ledger_text"] = "\n".join(lines + [duplicate]) + "\n"
        result = scorer.score_fixture(self.fixture, outcome)
        self.assertFalse(result["dedup"])
        self.assertEqual(len(result["duplicate_spawns"]), 1)
        self.assertEqual(result["total_spawns"], 9)

    def test_retry_after_delete_stays_within_dedup(self):
        # One child failed, was deleted, and a replacement with the same
        # worker name was spawned; the replacement also replied. This is
        # the retry the task prompt grants, and it resolves end to end.
        outcome = passing_outcome(self.fixture)
        lines = outcome["ledger_text"].splitlines()
        first = ledger_spawn_records(outcome["ledger_text"])[0]
        replacement_file = first["child"].replace(".jsonl", "-retry.jsonl")
        replacement = spawn_line("sub-retry00000", first["name"], replacement_file, first["parent"])
        outcome["ledger_text"] = (
            "\n".join(lines + [delete_line(first["childId"], first["child"]), replacement]) + "\n"
        )
        transcript = outcome["parent_transcript_text"].rstrip()
        replacement_session = replacement_file.rsplit("/", 1)[-1][:-6]
        outcome["parent_transcript_text"] = (
            transcript + "\n" + reply_line(replacement_session, first["name"]) + "\n"
        )
        result = scorer.score_fixture(self.fixture, outcome)
        self.assertTrue(result["dedup"])
        self.assertTrue(result["receipts"])
        self.assertEqual(result["total_spawns"], 9)
        self.assertTrue(result["resolved"])

    def test_spawn_budget_caps_total_spawns(self):
        # Eight shard workers plus nine helper spawns: seventeen depth-1
        # spawns exceed the one-retry-per-shard budget (2 x 8 = 16).
        outcome = passing_outcome(self.fixture)
        lines = outcome["ledger_text"].splitlines()
        first = ledger_spawn_records(outcome["ledger_text"])[0]
        helpers = []
        for index in range(9):
            helpers.append(
                spawn_line(
                    f"sub-helper{index:05d}",
                    f"helper-{index}",
                    f"/tmp/artifacts/parent-id/sub-helper{index:05d}/child{index}.jsonl",
                    first["parent"],
                )
            )
        outcome["ledger_text"] = "\n".join(lines + helpers) + "\n"
        result = scorer.score_fixture(self.fixture, outcome)
        self.assertEqual(result["total_spawns"], 17)
        self.assertFalse(result["dedup"])

    def test_silent_drop_blocks_receipts(self):
        outcome = passing_outcome(self.fixture)
        records = ledger_spawn_records(outcome["ledger_text"])
        children = [
            {
                "shard": {"worker": record["name"]},
                "session": record["child"].rsplit("/", 1)[-1][:-6],
            }
            for record in records
        ]
        # Every child replied except the last, which neither replied nor
        # left a notice: a dropped child must fail receipt completeness.
        outcome["parent_transcript_text"] = full_transcript(children[:-1])
        result = scorer.score_fixture(self.fixture, outcome)
        self.assertFalse(result["receipts"])
        self.assertEqual(len(result["missing_receipts"]), 1)

    def test_terminal_and_failure_notices_count_as_receipts(self):
        outcome = passing_outcome(self.fixture)
        records = ledger_spawn_records(outcome["ledger_text"])
        lines = [outcome["parent_transcript_text"].rstrip()]
        # Two children never replied; one left a completed-without-reply
        # notice, the other a failure notice. Both are accounted for.
        lines.append(notice_line(records[0]["childId"], records[0]["name"]))
        lines.append(failure_line(records[1]["childId"], records[1]["name"]))
        outcome["parent_transcript_text"] = "\n".join(lines) + "\n"
        result = scorer.score_fixture(self.fixture, outcome)
        self.assertTrue(result["receipts"])

    def test_deleted_child_without_notice_blocks_receipts(self):
        # Deleting a child that never replied hides the drop: the delete
        # record lands in the ledger, but no parent-visible receipt exists.
        # A deleted child that did reply stays receipted, so the drop
        # must be a child with neither reply nor notice.
        outcome = passing_outcome(self.fixture)
        records = ledger_spawn_records(outcome["ledger_text"])
        dropped = records[-1]
        kept = [
            {"shard": {"worker": record["name"]}, "session": record["child"].rsplit("/", 1)[-1][:-6]}
            for record in records[:-1]
        ]
        outcome["parent_transcript_text"] = full_transcript(kept)
        lines = outcome["ledger_text"].splitlines()
        outcome["ledger_text"] = "\n".join(lines + [delete_line(dropped["childId"], dropped["child"])]) + "\n"
        result = scorer.score_fixture(self.fixture, outcome)
        self.assertFalse(result["receipts"])
        self.assertEqual(len(result["missing_receipts"]), 1)
        self.assertFalse(result["delegation_evidence"])

    def test_receipt_requires_child_relationship(self):
        outcome = passing_outcome(self.fixture)
        # A sibling-labeled message carrying the child's session id is not
        # a receipt: the child's reply never actually arrived.
        outcome["parent_transcript_text"] = (
            "\n".join(
                line.replace('"fromRelationship": "child"', '"fromRelationship": "sibling"')
                for line in outcome["parent_transcript_text"].splitlines()
            )
            + "\n"
        )
        result = scorer.score_fixture(self.fixture, outcome)
        self.assertFalse(result["receipts"])

    def test_replay_edges_last_writer_wins(self):
        ledger = "\n".join(
            [
                spawn_line("sub-1", "worker-a", "/tmp/a.jsonl", "/tmp/p.jsonl"),
                spawn_line("sub-2", "worker-b", "/tmp/b.jsonl", "/tmp/p.jsonl"),
                rename_line("sub-2", "/tmp/b.jsonl", "worker-c"),
                delete_line("sub-1", "/tmp/a.jsonl"),
            ]
        )
        records, malformed = scorer.parse_ledger(ledger)
        self.assertEqual(malformed, 0)
        edges = scorer.replay_edges(records)
        self.assertEqual(set(edges), {"sub-1", "sub-2"})
        self.assertEqual(edges["sub-2"]["name"], "worker-c")
        self.assertIsNone(edges["sub-2"]["deleted"])
        self.assertEqual(edges["sub-1"]["deleted"], "parent-teardown")

    def test_parse_ledger_skips_malformed_lines(self):
        ledger = "{not json}\n" + spawn_line("sub-1", "worker-a", "/tmp/a.jsonl", "/tmp/p.jsonl") + "\n\n"
        records, malformed = scorer.parse_ledger(ledger)
        self.assertEqual(malformed, 1)
        self.assertEqual(len(records), 1)

    def test_summarize_usage_counts_each_assistant_message_once(self):
        message = {"role": "assistant", "usage": {"input": 60, "output": 40, "totalTokens": 100}}
        session = (
            json.dumps({"type": "message_end", "message": message})
            + "\n"
            + json.dumps({"type": "turn_end", "message": message, "toolResults": []})
        )
        self.assertEqual(scorer.summarize_usage(session), {"tokens": 100, "turns": 1})

    def test_summarize_usage_ignores_malformed_lines(self):
        session = "not json\n" + json.dumps({"type": "message_end"}) + "\n"
        self.assertEqual(scorer.summarize_usage(session), {"tokens": 0, "turns": 0})


# --- stub agent for end-to-end runner tests ---------------------------------
#
# The stub is a stand-in for the real CLI: it receives the same launch
# arguments the runner would give prime-agent, reads the isolated agent
# home from PRIME_AGENT_CODING_AGENT_DIR, and writes the exact artifact
# set a correct fan-out leaves behind (ledger, parent transcript, child
# session dirs, combined index, json-mode stdout events).

STUB_BODY = """\
#!/usr/bin/env python3
# Stub agent for the swarm-fanout eval self-tests.
import hashlib
import json
import os
import sys
from pathlib import Path

ANSWERS = __ANSWERS__
MODE = "__MODE__"
PARENT_ID = "019aaaaa-aaaa-4aaa-8aaa-00000000000a"


def assistant_event(tokens):
    usage = {"role": "assistant", "usage": {"input": 10, "output": tokens - 10, "totalTokens": tokens}}
    return json.dumps({"type": "message_end", "message": usage})


argv = sys.argv[1:]
# --mode json --daemon-socket SOCK --cwd REPO --session-dir SESSIONS --model M -- PROMPT
repo = Path(argv[5])
sessions_dir = Path(os.path.realpath(argv[7]))
agent_home = Path(os.environ["PRIME_AGENT_CODING_AGENT_DIR"])
parent_file = sessions_dir / (PARENT_ID + ".jsonl")

index_lines = [f"- {shard_file}: {answer}" for shard_file, answer in ANSWERS.items()]
(repo / "combined-index.md").write_text("\\n".join(index_lines) + "\\n")

if MODE == "artifact-only":
    # The no-spawn cheat: correct answers, zero delegation.
    print(assistant_event(1000))
    sys.exit(0)

artifacts_root = sessions_dir.parent / "session-artifacts"
ledger_lines = [
    json.dumps({"v": 1, "op": "meta", "at": "2026-09-12T00:00:00.000Z", "sessionsDir": str(sessions_dir)})
]
session_header = {
    "type": "session",
    "version": 3,
    "id": PARENT_ID,
    "timestamp": "2026-09-12T00:00:00.000Z",
    "cwd": str(repo),
}
transcript_lines = [json.dumps(session_header)]
for shard_file, answer in ANSWERS.items():
    stem = shard_file.rsplit(".", 1)[0]
    child_id = "sub-" + hashlib.sha256(stem.encode()).hexdigest()[:8]
    child_session = hashlib.sha256(("s" + shard_file).encode()).hexdigest()[:16]
    child_dir = artifacts_root / PARENT_ID / child_id
    child_dir.mkdir(parents=True, exist_ok=True)
    child_file = child_dir / (child_session + ".jsonl")
    child_header = {
        "type": "session",
        "version": 3,
        "id": child_session,
        "timestamp": "2026-09-12T00:00:00.000Z",
    }
    child_file.write_text(json.dumps(child_header) + "\\n")
    ledger_lines.append(
        json.dumps(
            {
                "v": 1,
                "op": "spawn",
                "at": "2026-09-12T00:00:00.500Z",
                "childId": child_id,
                "parent": str(parent_file),
                "child": str(child_file),
                "depth": 1,
                "name": "worker-" + stem,
            }
        )
    )
    transcript_lines.append(
        json.dumps(
            {
                "type": "custom_message",
                "customType": "agent_message",
                "content": "[agent-message from child:worker-" + stem + "]\\n\\n" + str(answer),
                "display": True,
                "details": {
                    "id": "agentmsg-" + child_session,
                    "message": str(answer),
                    "from": {
                        "activeSessionId": "active-" + child_session[:8],
                        "sessionId": child_session,
                        "sessionName": "worker-" + stem,
                        "runtimeKind": "subagent",
                    },
                    "fromRelationship": "child",
                    "target": {"activeSessionId": "active-parent", "sessionId": PARENT_ID},
                },
                "id": "entry-" + child_session[:8],
                "parentId": None,
                "timestamp": "2026-09-12T00:00:02.000Z",
            }
        )
    )

ledger_dir = agent_home / "rlm-ledger"
ledger_dir.mkdir(parents=True, exist_ok=True)
digest = hashlib.sha256(str(sessions_dir).encode("utf-8")).hexdigest()[:16]
(ledger_dir / (digest + ".jsonl")).write_text("\\n".join(ledger_lines) + "\\n")
parent_file.write_text("\\n".join(transcript_lines) + "\\n")
print(assistant_event(1000))
print(assistant_event(500))
sys.exit(0)
"""


def write_stub_agent(fixture: dict, mode: str = "full") -> Path:
    answers = {shard["file"]: shard["expected"] for shard in fixture["shards"]}
    script_dir = Path(tempfile.mkdtemp(prefix="swarm-fanout-stub-"))
    script = script_dir / "agent"
    body = STUB_BODY.replace("__ANSWERS__", repr(answers)).replace("__MODE__", mode)
    script.write_text(body)
    script.chmod(0o755)
    return script


class LedgerPathTests(unittest.TestCase):
    def test_ledger_hash_matches_product_computation(self):
        # rlmLedgerPath hashes the canonical sessions dir into the ledger
        # file name: sha256(canonical)[:16] under <agentDir>/rlm-ledger/.
        agent_home = Path("/tmp/agent-home")
        sessions_dir = "/tmp/sessions"
        candidates = runner.ledger_file_candidates(agent_home, sessions_dir)
        canonical = runner.canonicalize_dir_path(sessions_dir)
        digest = hashlib.sha256(canonical.encode("utf-8")).hexdigest()[:16]
        self.assertEqual(candidates[0], agent_home / "rlm-ledger" / f"{digest}.jsonl")

    def test_canonicalize_realpaths_existing_dirs(self):
        workdir = Path(tempfile.mkdtemp(prefix="swarm-fanout-canon-"))
        try:
            symlink = workdir / "linked"
            real = workdir / "real"
            real.mkdir()
            symlink.symlink_to(real, target_is_directory=True)
            self.assertEqual(runner.canonicalize_dir_path(str(symlink)), str(real.resolve()))
        finally:
            shutil.rmtree(workdir, ignore_errors=True)

    def test_canonicalize_missing_dir_stays_resolved(self):
        # rlmLedgerPath falls back to plain resolve when the dir does not
        # exist yet, without resolving ancestor symlinks.
        missing = str(Path(tempfile.gettempdir()) / "swarm-fanout-missing-dir")
        self.assertEqual(runner.canonicalize_dir_path(missing), os.path.abspath(missing))

    def test_ledger_candidates_cover_both_variants(self):
        agent_home = Path(tempfile.mkdtemp(prefix="swarm-fanout-home-"))
        try:
            workdir = Path(tempfile.mkdtemp(prefix="swarm-fanout-cand-"))
            try:
                real = workdir / "real"
                real.mkdir()
                symlinked = workdir / "link"
                symlinked.symlink_to(real, target_is_directory=True)
                candidates = runner.ledger_file_candidates(agent_home, str(symlinked))
                self.assertEqual(len(candidates), 2)
                canonical = hashlib.sha256(str(real.resolve()).encode("utf-8")).hexdigest()[:16]
                plain = hashlib.sha256(str(symlinked).encode("utf-8")).hexdigest()[:16]
                self.assertEqual(candidates[0].name, f"{canonical}.jsonl")
                self.assertEqual(candidates[1].name, f"{plain}.jsonl")
            finally:
                shutil.rmtree(workdir, ignore_errors=True)
        finally:
            shutil.rmtree(agent_home, ignore_errors=True)


class RunnerTests(unittest.TestCase):
    """The runner must emit a scored result in every launch failure mode."""

    def run_runner(self, argv: list[str]) -> tuple[int, dict]:
        captured = io.StringIO()
        with contextlib.redirect_stdout(captured):
            exit_code = runner.main(argv)
        return exit_code, json.loads(captured.getvalue())

    def test_stub_agent_full_fanout_resolves(self):
        fixture = fixture_manifest("json-events")
        stub = write_stub_agent(fixture, mode="full")
        argv = ["--fixture", str(FIXTURES / "json-events"), "--model", "test/fake", "--agent-bin", str(stub)]
        exit_code, result = self.run_runner(argv)
        self.assertEqual(exit_code, 0)
        self.assertTrue(result["resolved"])
        self.assertTrue(result["coverage"])
        self.assertTrue(result["delegation_evidence"])
        self.assertEqual(result["child_session_dirs"], 8)
        self.assertEqual(result["total_spawns"], 8)
        self.assertTrue(result["dedup"])
        self.assertTrue(result["receipts"])
        self.assertEqual(result["tokens_used"], 1500)
        self.assertEqual(result["turns"], 2)
        self.assertFalse(result["timed_out"])

    def test_stub_agent_without_spawns_scores_unresolved(self):
        # The artifact-only stub answers everything without spawning: the
        # no-spawn cheat must fail delegation evidence even though every
        # answer is correct.
        fixture = fixture_manifest("csv-metrics")
        stub = write_stub_agent(fixture, mode="artifact-only")
        argv = ["--fixture", str(FIXTURES / "csv-metrics"), "--model", "test/fake", "--agent-bin", str(stub)]
        exit_code, result = self.run_runner(argv)
        self.assertEqual(exit_code, 1)
        self.assertTrue(result["coverage"])
        self.assertFalse(result["delegation_evidence"])
        self.assertFalse(result["resolved"])

    def test_missing_agent_bin_scores_unresolved(self):
        argv = [
            "--fixture",
            str(FIXTURES / "json-events"),
            "--model",
            "test/fake",
            "--agent-bin",
            "/nonexistent-agent",
        ]
        exit_code, result = self.run_runner(argv)
        self.assertEqual(exit_code, 1)
        self.assertFalse(result["resolved"])
        self.assertTrue(result["agent_error"])

    def test_agent_timeout_still_scores(self):
        script_dir = Path(tempfile.mkdtemp(prefix="swarm-fanout-agent-"))
        script = script_dir / "agent"
        script.write_text("#!/bin/sh\nexec sleep 30\n")
        script.chmod(0o755)
        argv = [
            "--fixture",
            str(FIXTURES / "json-events"),
            "--model",
            "test/fake",
            "--agent-bin",
            str(script),
            "--timeout",
            "1",
        ]
        exit_code, result = self.run_runner(argv)
        self.assertEqual(exit_code, 1)
        self.assertFalse(result["resolved"])
        self.assertTrue(result["timed_out"])
        self.assertIsNone(result["exit_code"])

    def test_shutdown_agent_daemon_sends_shutdown_command(self):
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        socket_dir = Path(tempfile.mkdtemp(prefix="swarm-fanout-sock-"))
        socket_path = socket_dir / "daemon.sock"
        server.bind(str(socket_path))
        server.listen(1)
        received = []

        def serve() -> None:
            connection, _ = server.accept()
            received.append(connection.recv(1024).decode())
            connection.close()

        thread = threading.Thread(target=serve)
        thread.start()
        runner.shutdown_agent_daemon(socket_path)
        thread.join(timeout=5)
        server.close()
        self.assertEqual(len(received), 1)
        envelope = json.loads(received[0])
        self.assertEqual(envelope["type"], "command")
        self.assertEqual(envelope["command"]["type"], "shutdown")


if __name__ == "__main__":
    unittest.main()
