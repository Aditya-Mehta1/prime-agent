"""Fixture tests for the pr-triage skill: parsing, ranking, screening, output.

All network access is stubbed at the `gh` boundary, so these run offline with
stdlib unittest only (mirrors the repo's prime-agent-runtime test style).
"""

from __future__ import annotations

import asyncio
import json
import sys
import unittest
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from pr_triage import pr_triage as m  # noqa: E402

FIXTURES = Path(__file__).parent / "fixtures"
NOW = datetime(2026, 9, 13, 12, 0, 0, tzinfo=timezone.utc)
REPO = "acme/widgets"


def run(coro):
    return asyncio.run(coro)


def load_fixture(name):
    return json.loads((FIXTURES / name).read_text())


def queue_nodes(fixture):
    return fixture["data"]["repository"]["pullRequests"]["nodes"]


class TokenizeTests(unittest.TestCase):
    def test_splits_punctuation_and_dashes(self):
        self.assertEqual(
            m._tokenize("Fix(coding-agent): Transcripts"),
            {"fix", "coding", "agent", "transcript"},
        )

    def test_strips_long_plurals(self):
        self.assertIn("file", m._tokenize("files"))
        self.assertIn("step", m._tokenize("steps"))
        self.assertEqual(m._tokenize("ts"), {"ts"})  # short words keep the plural

    def test_title_similarity(self):
        self.assertEqual(m._title_similarity("Fix heartbeat listing", "Fix heartbeat listing"), 1.0)
        self.assertEqual(m._title_similarity("Fix heartbeat listing", "Refine theme palette"), 0.0)
        # Inflection alone should not hide a near-duplicate title.
        self.assertGreater(
            m._title_similarity("bound heartbeat listings", "bound heartbeat listing"),
            0.8,
        )


class BotLabelTests(unittest.TestCase):
    def test_known_bots_with_and_without_suffix(self):
        self.assertEqual(m._bot_label("cursor[bot]"), "cursor")
        self.assertEqual(m._bot_label("cursor"), "cursor")
        self.assertEqual(m._bot_label("macroscopeapp"), "macroscope")
        self.assertEqual(m._bot_label("chatgpt-codex-connector[bot]"), "codex")
        self.assertEqual(m._bot_label("github-actions[bot]"), None)
        self.assertEqual(m._bot_label("sethkarten"), None)
        self.assertEqual(m._bot_label(None), None)


class RepoUrlTests(unittest.TestCase):
    def test_https_and_ssh_and_suffixes(self):
        self.assertEqual(
            m._repo_from_url("https://github.com/PrimeIntellect-ai/prime-agent.git"),
            "PrimeIntellect-ai/prime-agent",
        )
        self.assertEqual(
            m._repo_from_url("git@github.com:PrimeIntellect-ai/prime-agent.git"),
            "PrimeIntellect-ai/prime-agent",
        )
        self.assertEqual(
            m._repo_from_url("https://github.com/OWNER/NAME/"),
            "OWNER/NAME",
        )

    def test_non_github_url(self):
        self.assertEqual(m._repo_from_url("https://gitlab.com/acme/widgets"), "")
        self.assertEqual(m._repo_from_url(""), "")


class OverlapStatsTests(unittest.TestCase):
    def test_overlap_stats(self):
        shared, jaccard, candidate_share = m._overlap_stats(
            ["a.ts", "b.ts", "c.ts"], ["a.ts", "b.ts", "d.ts", "e.ts"]
        )
        self.assertEqual(shared, 2)
        self.assertAlmostEqual(jaccard, 0.4)
        self.assertAlmostEqual(candidate_share, 2 / 3)
        self.assertEqual(m._overlap_stats([], ["a.ts"]), (0, 0.0, 0.0))
        self.assertEqual(m._overlap_stats(["a.ts"], []), (0, 0.0, 0.0))

    def test_flag_rules(self):
        # Jaccard alone flags.
        self.assertTrue(m._is_overlapping(shared=3, jaccard=0.4, candidate_share=0.75))
        # Low Jaccard but the PR already touches most candidate files: flag.
        self.assertTrue(m._is_overlapping(shared=2, jaccard=0.1, candidate_share=0.67))
        # One shared file of many is normal and must not flag.
        self.assertFalse(m._is_overlapping(shared=1, jaccard=0.1, candidate_share=0.3))
        # Two shared files that are most of the candidate set, but not enough.
        self.assertFalse(m._is_overlapping(shared=2, jaccard=0.1, candidate_share=0.5))


class ShortlistScoreTests(unittest.TestCase):
    def setUp(self):
        self.df = Counter({"fix": 13, "coding": 13, "agent": 14, "heartbeat": 2})
        self.candidate = m._tokenize("fix(coding-agent): bound heartbeat listing and skip client-owned launches")

    def test_boilerplate_only_match_is_not_shortlisted(self):
        boilerplate = m._tokenize("fix(coding-agent): flaky browser smoke test")
        score, rare_shared = m._shortlist_score(self.candidate, boilerplate, self.df)
        self.assertLess(score, m.TITLE_SCORE_THRESHOLD)
        self.assertFalse(rare_shared)

    def test_rare_topic_word_shortlists(self):
        sibling = m._tokenize("fix(coding-agent): prevent heartbeat catalog timeouts")
        score, rare_shared = m._shortlist_score(self.candidate, sibling, self.df)
        self.assertTrue(rare_shared)
        self.assertGreater(score, 0.1)


class PercentileTests(unittest.TestCase):
    def test_median_and_tail(self):
        values = [1.25, 3.17, 24.08, 29.13, 45.08]
        self.assertAlmostEqual(m._percentile(sorted(values), 0.5), 24.08)
        self.assertAlmostEqual(m._percentile(sorted(values), 0.9), 45.08)
        self.assertEqual(m._percentile([], 0.5), 0.0)


class TruncateTests(unittest.TestCase):
    def test_short_text_unchanged(self):
        self.assertEqual(m._truncate("abc", 100), "abc")

    def test_long_text_truncated_with_marker(self):
        out = m._truncate("x" * 500, 100)
        self.assertLessEqual(len(out), 100)
        self.assertIn("[output truncated, 500 chars total]", out)


def graphql_side_effect(responses):
    """Return an async gh stub answering graphql calls with the given pages."""
    calls: list[tuple] = []

    async def fake_gh(*args, payload=None):
        calls.append((args, payload))
        return json.dumps(responses[len([c for c in calls if c[0][1] == "graphql"]) - 1])

    return fake_gh, calls


class RunDigestTests(unittest.TestCase):
    def run_with_fixture(self, fixture_name, **kwargs):
        fixture = load_fixture(fixture_name)
        fake_gh, calls = graphql_side_effect([fixture])

        def fake_now():
            return NOW

        with patch.object(m, "gh", side_effect=fake_gh), patch.object(m, "_now", fake_now):
            digest = run(m.run(repo=REPO, **kwargs))
        return digest, calls

    def test_digest_shape_and_ranking(self):
        digest, _ = self.run_with_fixture("queue_response.json")
        self.assertIn(f"# PR queue digest — {REPO}", digest)
        self.assertIn("6 open pull requests (1 draft excluded)", digest)
        self.assertIn("merge state: 2 DIRTY, 2 CLEAN, 1 BLOCKED", digest)
        self.assertIn("review decision: 4 REVIEW_REQUIRED, 1 CHANGES_REQUESTED", digest)
        self.assertIn("- age: median 24.1d, p90 45.1d, oldest 45.1d (#2131)", digest)
        self.assertIn("most open PRs per author: xeophon 2, sethkarten 2, snimu 1", digest)
        # Queue-wide bot-thread totals come from unresolved threads only.
        self.assertIn("unresolved bot threads across the queue: cursor 3, codex 2, macroscope 1", digest)

        # Review-ready is CLEAN, oldest first.
        ready_index = digest.index("## Review-ready (CLEAN, oldest first) — 2")
        row_2115 = digest.index("| #2115 |", ready_index)
        row_2120 = digest.index("| #2120 |", ready_index)
        self.assertLess(row_2115, row_2120)
        self.assertIn("READY", digest[row_2115:digest.index("\n", row_2115)])
        self.assertIn("ATTENTION: 2 unresolved bot threads", digest[row_2120:digest.index("\n", row_2120)])
        self.assertIn("cursor 1, macroscope 1", digest[row_2120:digest.index("\n", row_2120)])

        # Attention lists are risk-ranked: conflicts, staleness, bot threads.
        self.assertIn("conflicts with main (2): #2131, #2101", digest)
        # Stale covers every PR with no human review newer than stale_days;
        # the author's own bot-free PRs fall back to their creation date.
        self.assertIn("no human touch for >14d (3): #2115 (29d), #2131 (29d), #2101 (24d)", digest)
        self.assertIn(
            "unresolved bot threads (3): #2131 [cursor 2, codex 1], #2120 [cursor 1, macroscope 1], #2101 [codex 1]",
            digest,
        )
        self.assertIn(f"- #2115: https://github.com/{REPO}/pull/2115", digest)

    def test_include_drafts_counts_them(self):
        digest, _ = self.run_with_fixture("queue_response.json", include_drafts=True)
        self.assertIn("6 open pull requests\n", digest)
        self.assertIn("merge state: 3 CLEAN, 2 DIRTY, 1 BLOCKED", digest)
        # Drafts are counted but never listed as review-ready.
        self.assertIn("## Review-ready (CLEAN, oldest first) — 2", digest)

    def test_digest_truncation(self):
        digest, _ = self.run_with_fixture("queue_response.json", max_output=200)
        self.assertLessEqual(len(digest), 200)
        self.assertIn("[output truncated", digest)


class RunPaginationTests(unittest.TestCase):
    def test_two_pages_and_total(self):
        pages = [load_fixture("queue_page1.json"), load_fixture("queue_page2.json")]
        fake_gh, calls = graphql_side_effect(pages)

        def fake_now():
            return NOW

        with patch.object(m, "gh", side_effect=fake_gh), patch.object(m, "_now", fake_now):
            digest = run(m.run(repo=REPO, limit=6))
        graphql_calls = [c for c in calls if c[0][1] == "graphql"]
        self.assertEqual(len(graphql_calls), 2)
        # Page 2 continues from the page-1 cursor.
        self.assertEqual(graphql_calls[1][1]["variables"]["after"], "PAGE1")
        self.assertIn("126 open pull requests (1 draft excluded); fetched 6 of 126", digest)
        # PRs from both pages are ranked together, oldest first.
        ready_index = digest.index("## Review-ready (CLEAN, oldest first) — 2")
        self.assertLess(digest.index("| #2115 |", ready_index), digest.index("| #2120 |", ready_index))


class RunErrorTests(unittest.TestCase):
    def test_missing_gh_returns_setup_guidance(self):
        async def missing_gh(*args, payload=None):
            raise FileNotFoundError("gh")

        with patch.object(m, "gh", side_effect=missing_gh):
            out = run(m.run(repo=REPO))
        self.assertIn("needs the GitHub CLI", out)
        self.assertIn("https://cli.github.com", out)

    def test_gh_failure_returns_error_text(self):
        async def failing_gh(*args, payload=None):
            raise RuntimeError("gh api graphql failed: bad credentials")

        with patch.object(m, "gh", side_effect=failing_gh):
            out = run(m.run(repo=REPO))
        self.assertIn(f"PR triage failed for {REPO}", out)
        self.assertIn("bad credentials", out)

    def test_missing_repo_returns_guidance(self):
        async def infer_empty():
            return ""

        with patch.object(m, "_infer_repo", side_effect=infer_empty):
            out = run(m.run(repo=""))
        self.assertIn("needs a repository", out)
        self.assertIn('pass repo="OWNER/NAME"', out)


class CheckOverlapsTests(unittest.TestCase):
    CANDIDATE_TITLE = "fix(coding-agent): bound heartbeat listing and skip client-owned launches"
    CANDIDATE_FILES = [
        "packages/agent/src/daemon-supervisor.ts",
        "packages/agent/test/daemon-supervisor.test.ts",
        "packages/agent/src/agent-session.ts",
    ]

    def make_gh(self):
        """Stub gh: graphql returns the fixture titles; files map from fixtures."""
        titles = load_fixture("titles_response.json")
        calls: list[tuple] = []

        async def fake_gh(*args, payload=None):
            calls.append(args)
            if args[1] == "graphql":
                return json.dumps(titles)
            # args[1] is "repos/{repo}/pulls/{n}/files?per_page=100&page=1"
            number = int(args[1].rsplit("/", 2)[1])
            name = {2140: "files_2140.json", 2160: "files_2160.json"}.get(number)
            return json.dumps(load_fixture(name) if name else [])

        return fake_gh, calls

    def file_fetch_numbers(self, calls):
        numbers = []
        for args in calls:
            if args[1] != "graphql":
                numbers.append(int(args[1].rsplit("/", 2)[1]))
        return numbers

    def run_overlaps(self, *, deep=False, title=CANDIDATE_TITLE, files=None, repo=REPO):
        fake_gh, calls = self.make_gh()
        with patch.object(m, "gh", side_effect=fake_gh):
            candidate_files = self.CANDIDATE_FILES if files is None else files
            report = run(m.check_overlaps(title, candidate_files, repo=repo, deep=deep))
        return report, calls

    def test_flags_duplicate_with_shared_files(self):
        report, calls = self.run_overlaps()
        self.assertIn(f'# Duplicate pre-flight — "{self.CANDIDATE_TITLE}"', report)
        self.assertIn("Checked 14 open pull requests in acme/widgets; file-overlap fetch on 2", report)
        self.assertIn("## Likely duplicate work", report)
        self.assertIn(
            '- #2140 "fix(coding-agent): prevent heartbeat catalog timeouts" — '
            "shares 3 of the candidate's files (Jaccard 0.75), title similarity 0.29",
            report,
        )
        self.assertIn(f"https://github.com/{REPO}/pull/2140", report)
        self.assertIn("Verdict: DUPLICATE RISK — read #2140 before starting", report)

        # Only title-screened PRs get a file fetch: the two heartbeat PRs.
        self.assertEqual(self.file_fetch_numbers(calls), [2140, 2160])
        # Boilerplate-only titles never reach the file endpoint.
        self.assertNotIn("2141", report)

    def test_deep_checks_every_open_pr(self):
        report, calls = self.run_overlaps(deep=True)
        fetched = self.file_fetch_numbers(calls)
        self.assertEqual(len(fetched), 14)
        self.assertIn("file-overlap fetch on 14 (every open PR)", report)
        # Only the real overlap is flagged even when everything is checked.
        self.assertIn("Verdict: DUPLICATE RISK — read #2140", report)
        self.assertNotIn("#2141", report)

    def test_clear_when_no_overlap(self):
        report, _ = self.run_overlaps(files=["packages/tui/src/main.ts"])
        self.assertIn("Verdict: CLEAR — no title-screened open PR overlaps the planned files.", report)
        self.assertNotIn("Likely duplicate work", report)

    def test_empty_files_returns_guidance(self):
        report, calls = self.run_overlaps(files=[])
        self.assertIn("needs the planned changed files", report)
        self.assertEqual(calls, [])

    def test_missing_repo_returns_guidance(self):
        async def infer_empty():
            return ""

        fake_gh, _ = self.make_gh()
        with patch.object(m, "_infer_repo", side_effect=infer_empty), patch.object(m, "gh", side_effect=fake_gh):
            report = run(m.check_overlaps(self.CANDIDATE_TITLE, self.CANDIDATE_FILES, repo=""))
        self.assertIn("needs a repository", report)

    def test_missing_gh_returns_setup_guidance(self):
        async def missing_gh(*args, payload=None):
            raise FileNotFoundError("gh")

        with patch.object(m, "gh", side_effect=missing_gh):
            report = run(m.check_overlaps(self.CANDIDATE_TITLE, self.CANDIDATE_FILES, repo=REPO))
        self.assertIn("needs the GitHub CLI", report)


class ParseNodeTests(unittest.TestCase):
    def test_pending_reviews_and_thread_authors(self):
        nodes = queue_nodes(load_fixture("queue_response.json"))
        by_number = {int(n["number"]): n for n in nodes}
        pr_2125 = m._pr_from_node(by_number[2125])
        # A PENDING review request has no timestamp and must not break parsing.
        self.assertEqual(pr_2125.last_human_at, pr_2125.created_at)
        pr_2131 = m._pr_from_node(by_number[2131])
        self.assertEqual(pr_2131.unresolved, 3)
        self.assertEqual(pr_2131.unresolved_bots["cursor"], 2)
        self.assertEqual(pr_2131.unresolved_bots["codex"], 1)
        # The human CHANGES_REQUESTED review sets the last human touch.
        self.assertEqual(pr_2131.last_human_at, m._parse_ts("2026-08-15T10:00:00Z"))


if __name__ == "__main__":
    unittest.main()
