"""PR triage skill implementation.

Summarizes a GitHub repository's open pull-request queue and pre-flights
planned work against open PRs, using the `gh` CLI (the same interface the
repo's own scripts use). All network access goes through :func:`gh`; the
parsing, ranking, and formatting helpers are pure so tests can cover them
with fixture JSON alone.
"""

from __future__ import annotations

import asyncio
import json
import re
import subprocess
from collections import Counter
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any

# Bots whose unresolved review threads gate merge-readiness. GraphQL and REST
# report App actors with and without a "[bot]" suffix (cursor vs cursor[bot]),
# so the suffix is stripped before matching.
KNOWN_BOTS = {
    "cursor": "cursor",
    "macroscopeapp": "macroscope",
    "chatgpt-codex-connector": "codex",
}

# Title screen for check_overlaps: shortlist an open PR when its df-weighted
# title score against the candidate reaches this, or when they share a rare
# token. Boilerplate-heavy titles score near zero; the real duplicate pairs
# seen in the wild (heartbeat listing, transcript restore, harness store) all
# shared rare topic words AND changed files, so the screen catches them while
# keeping the file-overlap fetch count small.
TITLE_SCORE_THRESHOLD = 0.35
RARE_DF = 10
MAX_SHORTLIST = 40

# Overlap is flagged when the Jaccard index of the changed-file sets is at
# least this, or when the PR already touches most of the candidate's files.
JACCARD_THRESHOLD = 0.3
CANDIDATE_SHARE_THRESHOLD = 0.6

GRAPHQL_PAGE_SIZE = 100
TITLES_FETCH_CAP = 500  # check_overlaps screens every open PR; cap guards huge queues.
FILES_PAGE_SIZE = 100
FILES_PAGE_CAP = 3  # REST pagination cap: 300 files per PR is plenty for triage.

QUEUE_QUERY = """query($owner: String!, $name: String!, $first: Int, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequests(states: OPEN, first: $first, after: $after) {
      totalCount
      pageInfo { hasNextPage endCursor }
      nodes {
        number
        title
        author { login }
        createdAt
        isDraft
        additions
        deletions
        changedFiles
        mergeStateStatus
        reviewDecision
        reviewThreads(first: 50) { pageInfo { hasNextPage } nodes { isResolved comments(first: 1) { nodes { author { login } } } } }
        reviews(last: 30) { nodes { author { login __typename } state submittedAt } }
      }
    }
  }
}"""

TITLES_QUERY = """query($owner: String!, $name: String!, $first: Int, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequests(states: OPEN, first: $first, after: $after) {
      totalCount
      pageInfo { hasNextPage endCursor }
      nodes { number title author { login } changedFiles isDraft }
    }
  }
}"""

GH_TIMEOUT_SECONDS = 60


async def gh(*args: str, payload: dict[str, Any] | None = None) -> str:
    """Run one `gh` CLI call and return stdout; raise RuntimeError on failure."""
    try:
        result = await asyncio.to_thread(
            subprocess.run,
            ["gh", *args],
            input=json.dumps(payload) if payload is not None else None,
            text=True,
            capture_output=True,
            check=False,
            timeout=GH_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired as e:
        raise RuntimeError(f"gh {' '.join(args[:2])} timed out after {GH_TIMEOUT_SECONDS}s") from e
    if result.returncode:
        detail = (result.stderr or result.stdout or "").strip()
        raise RuntimeError(f"gh {' '.join(args[:2])} failed: {detail}")
    return result.stdout


def _now() -> datetime:
    return datetime.now(timezone.utc)


def _parse_ts(raw: str) -> datetime:
    """Parse a GitHub ISO-8601 timestamp (Z suffix) into an aware datetime."""
    parsed = datetime.fromisoformat(raw.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed


def _bot_label(login: str | None) -> str | None:
    """Map a review-thread author to a known bot label, or None for humans."""
    if not login:
        return None
    stripped = re.sub(r"\[bot\]$", "", login)
    return KNOWN_BOTS.get(stripped)


def _is_bot(login: str | None, author_type: str | None = None) -> bool:
    """True for any bot account, whether or not the login carries a suffix.

    GraphQL reports App and machine actors (cursor, dependabot, ...) with a
    BARE login and `__typename: "Bot"`, while REST appends "[bot]"; rely on the
    actor type when present and fall back to login matching (KNOWN_BOTS after
    suffix stripping, or a "[bot]" suffix) for data without the type.
    """
    if author_type == "Bot":
        return True
    if not login:
        return False
    return _bot_label(login) is not None or login.endswith("[bot]")


def _tokenize(text: str) -> set[str]:
    """Lowercase word tokens; dashes and non-alphanumerics are separators.

    Long words drop a trailing plural so "transcripts" matches "transcript";
    real near-duplicate PR titles often differ only by inflection.
    """
    tokens = set()
    for token in re.split(r"[^a-z0-9]+", text.lower()):
        if not token:
            continue
        if len(token) >= 4 and token.endswith("s"):
            token = token[:-1]
        tokens.add(token)
    return tokens


def _title_similarity(a: str, b: str) -> float:
    """Jaccard similarity of title token sets (1.0 for identical titles)."""
    tokens_a = _tokenize(a)
    tokens_b = _tokenize(b)
    if not tokens_a or not tokens_b:
        return 0.0
    return len(tokens_a & tokens_b) / len(tokens_a | tokens_b)


def _shortlist_score(
    candidate_tokens: set[str], pr_tokens: set[str], doc_freq: Counter[str]
) -> tuple[float, bool]:
    """Screen score of an open PR title against the candidate title.

    Weighs each shared token by inverse document frequency across the open-PR
    title corpus, so boilerplate prefixes ("fix(coding-agent)") count for
    almost nothing while rare topic words (heartbeat, transcripts, harness)
    dominate. Returns the weighted share of the candidate title plus whether
    any rare token (df <= RARE_DF) is shared; either shortlists the PR.
    """
    if not candidate_tokens or not pr_tokens:
        return 0.0, False
    shared = candidate_tokens & pr_tokens
    if not shared:
        return 0.0, False
    candidate_weight = sum(1.0 / (2 + doc_freq.get(token, 0)) for token in candidate_tokens)
    if candidate_weight <= 0:
        return 0.0, False
    score = sum(1.0 / (2 + doc_freq.get(token, 0)) for token in shared) / candidate_weight
    return score, any(doc_freq.get(token, 0) <= RARE_DF for token in shared)


def _overlap_stats(candidate_files: list[str], pr_files: list[str]) -> tuple[int, float, float]:
    """Return (shared count, Jaccard index, candidate share) for two file sets."""
    candidate = set(candidate_files)
    other = set(pr_files)
    if not candidate or not other:
        return 0, 0.0, 0.0
    shared = candidate & other
    jaccard = len(shared) / len(candidate | other)
    candidate_share = len(shared) / len(candidate)
    return len(shared), jaccard, candidate_share


def _is_overlapping(shared: int, jaccard: float, candidate_share: float) -> bool:
    """Flag an open PR as likely-duplicate overlap for the candidate change."""
    if jaccard >= JACCARD_THRESHOLD:
        return True
    return shared >= 2 and candidate_share >= CANDIDATE_SHARE_THRESHOLD


def _percentile(sorted_values: list[float], fraction: float) -> float:
    if not sorted_values:
        return 0.0
    index = min(len(sorted_values) - 1, int(round(fraction * (len(sorted_values) - 1))))
    return sorted_values[index]


@dataclass
class PR:
    """One open pull request with the fields the digest needs."""

    number: int
    title: str
    author: str
    created_at: datetime
    is_draft: bool
    additions: int
    deletions: int
    changed_files: int
    merge_state: str
    review_decision: str
    unresolved_bots: Counter[str] = field(default_factory=Counter)
    last_human_at: datetime | None = None
    threads_truncated: bool = False

    @property
    def unresolved(self) -> int:
        return sum(self.unresolved_bots.values())

    @property
    def size(self) -> str:
        return f"+{self.additions}/-{self.deletions}"

    def age_days_at(self, now: datetime) -> float:
        return (now - self.created_at).total_seconds() / 86400

    def human_idle_days_at(self, now: datetime) -> float | None:
        if self.last_human_at is None:
            return None
        return (now - self.last_human_at).total_seconds() / 86400

    def url(self, repo: str) -> str:
        return f"https://github.com/{repo}/pull/{self.number}"


def _pr_from_node(node: dict[str, Any]) -> PR:
    """Build a PR from a GraphQL queue-query node (see QUEUE_QUERY)."""
    created = _parse_ts(node["createdAt"])
    last_human = created
    for review in node.get("reviews", {}).get("nodes", []):
        # PENDING entries are requested-but-unsubmitted reviews; they have no timestamp.
        if review.get("state") == "PENDING" or not review.get("submittedAt"):
            continue
        author = review.get("author") or {}
        if _is_bot(author.get("login"), author.get("__typename")):
            continue
        submitted = _parse_ts(review["submittedAt"])
        if submitted > last_human:
            last_human = submitted

    bots: Counter[str] = Counter()
    threads = node.get("reviewThreads", {})
    threads_truncated = bool(threads.get("pageInfo", {}).get("hasNextPage"))
    for thread in threads.get("nodes", []):
        if thread.get("isResolved"):
            continue
        comments = thread.get("comments", {}).get("nodes", [])
        author = (comments[0].get("author") or {}) if comments else {}
        label = _bot_label(author.get("login"))
        if label:
            bots[label] += 1

    return PR(
        number=int(node["number"]),
        title=str(node.get("title") or ""),
        author=str((node.get("author") or {}).get("login") or "unknown"),
        created_at=created,
        is_draft=bool(node.get("isDraft")),
        additions=int(node.get("additions") or 0),
        deletions=int(node.get("deletions") or 0),
        changed_files=int(node.get("changedFiles") or 0),
        merge_state=str(node.get("mergeStateStatus") or "UNKNOWN"),
        review_decision=str(node.get("reviewDecision") or "PENDING"),
        unresolved_bots=bots,
        last_human_at=last_human,
        threads_truncated=threads_truncated,
    )


def _split_repo(repo: str) -> tuple[str, str]:
    owner, _, name = repo.partition("/")
    if not owner or not name:
        raise RuntimeError(f"invalid repo {repo!r}; expected OWNER/NAME")
    return owner, name


async def _graphql_pages(
    query: str, repo: str, limit: int
) -> tuple[list[dict[str, Any]], int, bool]:
    """Paginate a GraphQL pullRequests query (QUEUE_QUERY or TITLES_QUERY).

    Fetches until `limit` nodes or the queue ends. Returns the raw nodes, the
    queue's totalCount, and whether the queue still had more pages at the cap
    (so callers can mark verdicts as covering only the fetched subset).
    """
    owner, name = _split_repo(repo)
    nodes: list[dict[str, Any]] = []
    total: int | None = None
    cursor: str | None = None
    truncated = False
    while len(nodes) < limit:
        payload = {
            "query": query,
            "variables": {"owner": owner, "name": name, "first": min(GRAPHQL_PAGE_SIZE, limit - len(nodes)), "after": cursor},
        }
        response = json.loads(await gh("api", "graphql", "--input", "-", payload=payload))
        if response.get("errors") or not response.get("data", {}).get("repository"):
            raise RuntimeError(f"GraphQL query failed: {json.dumps(response.get('errors'))}")
        connection = response["data"]["repository"]["pullRequests"]
        total = int(connection.get("totalCount") or 0) if total is None else total
        nodes.extend(connection["nodes"])
        if not connection["pageInfo"]["hasNextPage"]:
            break
        cursor = connection["pageInfo"]["endCursor"]
    else:
        truncated = True
    return nodes, total or 0, truncated


async def _fetch_queue(repo: str, limit: int) -> tuple[list[PR], int]:
    nodes, total, _truncated = await _graphql_pages(QUEUE_QUERY, repo, limit)
    return [_pr_from_node(node) for node in nodes], total


async def _fetch_pr_files(repo: str, number: int) -> tuple[list[str], bool]:
    """Fetch a PR's changed-file paths via REST, up to FILES_PAGE_CAP pages.

    Returns the paths and whether the PR has more files than the cap, so the
    caller can mark the overlap check as truncated.
    """
    filenames: list[str] = []
    truncated = False
    for page in range(1, FILES_PAGE_CAP + 1):
        text = await gh("api", f"repos/{repo}/pulls/{number}/files?per_page={FILES_PAGE_SIZE}&page={page}")
        entries = json.loads(text)
        if not isinstance(entries, list):
            raise RuntimeError(f"unexpected files response for PR #{number}")
        filenames.extend(str(entry.get("filename") or "") for entry in entries if entry.get("filename"))
        if len(entries) < FILES_PAGE_SIZE:
            break
        truncated = page >= FILES_PAGE_CAP
    return filenames, truncated


def _format_bot_threads(bots: Counter[str]) -> str:
    parts = [f"{label} {count}" for label, count in sorted(bots.items(), key=lambda kv: (-kv[1], kv[0]))]
    return ", ".join(parts) if parts else "0"


def _verdict(pr: PR) -> tuple[str, list[str]]:
    """READY verdict per the merge-ready policy; returns verdict plus reasons."""
    reasons: list[str] = []
    if pr.merge_state != "CLEAN":
        reasons.append(pr.merge_state.lower() if pr.merge_state != "UNKNOWN" else "merge state unknown")
    if pr.unresolved:
        reasons.append(f"{pr.unresolved} unresolved bot thread{'s' if pr.unresolved > 1 else ''}")
    if pr.threads_truncated:
        # Thread list truncated at the 50-fetch cap: cannot prove zero
        # unresolved threads, so READY is withheld.
        reasons.append("thread list truncated (50+ threads)")
    if pr.review_decision == "CHANGES_REQUESTED":
        reasons.append("changes requested")
    # READY per the merge-ready policy: CLEAN, no unresolved bot threads, no
    # changes requested. Staleness is tracked separately in the attention
    # list — an old CLEAN PR is ready, just overdue.
    return ("READY", []) if not reasons else ("ATTENTION", reasons)


def _truncate(text: str, max_output: int) -> str:
    if len(text) <= max_output:
        return text
    total = len(text)
    marker = f"\n\n... [output truncated, {total} chars total] ...\n"
    half = max(0, (max_output - len(marker)) // 2)
    truncated = text[:half] + marker + text[len(text) - half :]
    return truncated if len(truncated) <= max_output else truncated[:max_output]


def _format_digest(prs: list[PR], repo: str, *, total: int, include_drafts: bool, stale_days: float, now: datetime) -> str:
    """Format the queue digest markdown from parsed PRs (pure)."""
    shown = [pr for pr in prs if include_drafts or not pr.is_draft]
    drafts = len(prs) - len(shown)
    by_state = Counter(pr.merge_state for pr in shown)
    by_decision = Counter(pr.review_decision for pr in shown)
    by_author = Counter(pr.author for pr in shown)
    bot_totals: Counter[str] = Counter()
    for pr in shown:
        bot_totals.update(pr.unresolved_bots)

    ages = sorted(pr.age_days_at(now) for pr in shown)
    oldest = max(shown, key=lambda pr: pr.age_days_at(now)) if shown else None

    lines: list[str] = [f"# PR queue digest — {repo}", ""]
    count_note = f"{total} open pull requests"
    if drafts:
        count_note += f" ({drafts} draft{'s' if drafts > 1 else ''} excluded)"
    if len(prs) < total:
        count_note += f"; fetched {len(prs)} of {total} — pass a higher limit for the full queue"
    lines += [count_note, ""]

    lines.append("## Queue shape")
    state_parts = [f"{count} {state}" for state, count in by_state.most_common()]
    lines.append(f"- merge state: {', '.join(state_parts) or 'none'}")
    decision_parts = [f"{count} {decision}" for decision, count in by_decision.most_common()]
    lines.append(f"- review decision: {', '.join(decision_parts) or 'none'}")
    if shown:
        median = _percentile(ages, 0.5)
        p90 = _percentile(ages, 0.9)
        lines.append(
            f"- age: median {median:.1f}d, p90 {p90:.1f}d"
            + (f", oldest {ages[-1]:.1f}d (#{oldest.number})" if oldest else "")
        )
    author_parts = [f"{author} {count}" for author, count in by_author.most_common(5)]
    lines.append(f"- most open PRs per author: {', '.join(author_parts) or 'none'}")
    bot_parts = [f"{label} {count}" for label, count in bot_totals.most_common()]
    lines.append(f"- unresolved bot threads across the queue: {', '.join(bot_parts) or 'none'}")
    lines.append("")

    ready = [pr for pr in shown if pr.merge_state == "CLEAN" and not pr.is_draft]
    ready.sort(key=lambda pr: pr.age_days_at(now), reverse=True)
    lines.append(f"## Review-ready (CLEAN, oldest first) — {len(ready)}")
    if ready:
        lines.append("| # | age | size | files | threads | last human | verdict | title |")
        lines.append("|---|-----|------|-------|---------|------------|---------|-------|")
        for pr in ready:
            verdict, reasons = _verdict(pr)
            idle = pr.human_idle_days_at(now)
            idle_text = f"{idle:.1f}d" if idle is not None else "?"
            thread_text = f"{pr.unresolved} ({_format_bot_threads(pr.unresolved_bots)})" if pr.unresolved else "0"
            verdict_text = verdict if verdict == "READY" else f"{verdict}: {'; '.join(reasons)}"
            title = pr.title if len(pr.title) <= 60 else pr.title[:57] + "..."
            lines.append(
                f"| #{pr.number} | {pr.age_days_at(now):.1f}d | {pr.size} | {pr.changed_files} | "
                f"{thread_text} | {idle_text} | {verdict_text} | {title} |"
            )
    else:
        lines.append("_No CLEAN pull requests — the queue is blocked or conflicted._")
    lines.append("")

    conflicted = sorted((pr for pr in shown if pr.merge_state == "DIRTY"), key=lambda pr: pr.age_days_at(now), reverse=True)
    threaded = sorted((pr for pr in shown if pr.unresolved), key=lambda pr: (-pr.unresolved, pr.age_days_at(now)))
    stale = [pr for pr in shown if (idle := pr.human_idle_days_at(now)) is not None and idle > stale_days]
    stale.sort(key=lambda pr: pr.human_idle_days_at(now) or 0, reverse=True)

    lines.append("## Needs attention")
    if conflicted:
        summary = ", ".join(f"#{pr.number}" for pr in conflicted[:12])
        lines.append(f"- conflicts with main ({len(conflicted)}): {summary}")
    if stale:
        summary = ", ".join(f"#{pr.number} ({pr.human_idle_days_at(now):.0f}d)" for pr in stale[:12])
        lines.append(f"- no human touch for >{stale_days:.0f}d ({len(stale)}): {summary}")
    if threaded:
        summary = ", ".join(
            f"#{pr.number} [{_format_bot_threads(pr.unresolved_bots)}]" for pr in threaded[:12]
        )
        lines.append(f"- unresolved bot threads ({len(threaded)}): {summary}")
    if not (conflicted or stale or threaded):
        lines.append("- nothing queued: no conflicts, stale PRs, or open bot threads")
    lines.append("")

    links = [f"- #{pr.number}: {pr.url(repo)}" for pr in ready[:15]]
    if links:
        lines.append("## Links")
        lines.extend(links)
        lines.append("")

    return "\n".join(lines).rstrip() + "\n"


async def run(
    repo: str = "",
    *,
    limit: int = 100,
    include_drafts: bool = False,
    stale_days: float = 14,
    max_output: int = 16000,
) -> str:
    """Summarize a GitHub repository's open pull-request queue for review triage.

    Uses the authenticated `gh` CLI. The digest counts PRs by merge state and
    review decision, reports age percentiles and per-author depth, and lists
    CLEAN PRs oldest-first with per-PR unresolved bot-thread counts (Cursor,
    Macroscope, Codex) and a READY verdict.

    Args:
        repo: Repository as OWNER/NAME. Defaults to the origin remote of the current directory.
        limit: Fetch at most this many open PRs (oldest may be cut off; raise it for the full queue).
        include_drafts: Include draft PRs in the digest.
        stale_days: Flag PRs with no human review activity for longer than this many days.
        max_output: Truncate the digest to at most this many characters.

    Returns:
        A markdown digest of the queue.
    """
    if not repo:
        repo = await _infer_repo()
        if not repo:
            return (
                "PR triage needs a repository: pass repo=\"OWNER/NAME\", or run inside a checkout with an origin remote.\n"
                "Example: await pr_triage(repo=\"PrimeIntellect-ai/prime-agent\")"
            )
    try:
        prs, total = await _fetch_queue(repo, max(1, limit))
    except FileNotFoundError:
        return (
            "PR triage needs the GitHub CLI (`gh`), which is not installed or not on PATH.\n"
            "Install it from https://cli.github.com and run `gh auth login`, then retry."
        )
    except RuntimeError as e:
        return f"PR triage failed for {repo}: {e}\nRun `gh auth status` and `gh api rate_limit` if this keeps failing."
    digest = _format_digest(prs, repo, total=total, include_drafts=include_drafts, stale_days=stale_days, now=_now())
    return _truncate(digest, max_output)


async def check_overlaps(
    title: str,
    files: list[str],
    *,
    repo: str = "",
    deep: bool = False,
    max_output: int = 8000,
) -> str:
    """Pre-flight a planned change against open pull requests for duplicate work.

    Fetches the repository's open-PR titles, screens them against the
    candidate title with document-frequency-weighted token overlap (boilerplate
    counts for almost nothing; one rare shared topic word shortlists a PR),
    fetches the shortlisted PRs' changed files, and flags PRs whose changed-file
    overlap suggests the work already exists.

    Args:
        title: Planned PR title (used for the similarity screen).
        files: Planned changed-file paths, e.g. ["packages/agent/src/daemon-supervisor.ts"].
        repo: Repository as OWNER/NAME. Defaults to the origin remote of the current directory.
        deep: File-check every open PR instead of screened ones (slower: one gh call per PR).
        max_output: Truncate the report to at most this many characters.

    Returns:
        A markdown report flagging likely duplicates, or a CLEAR verdict.
    """
    if not files:
        return "Duplicate pre-flight needs the planned changed files; pass files=[...] (paths the work will touch)."
    if not repo:
        repo = await _infer_repo()
        if not repo:
            return "Duplicate pre-flight needs a repository: pass repo=\"OWNER/NAME\", or run inside a checkout with an origin remote."
    try:
        nodes, total, queue_truncated = await _graphql_pages(TITLES_QUERY, repo, TITLES_FETCH_CAP)
        candidate_tokens = _tokenize(title)
        doc_freq: Counter[str] = Counter()
        pr_tokens: dict[int, set[str]] = {}
        titles: dict[int, str] = {}
        for node in nodes:
            number = int(node["number"])
            titles[number] = str(node.get("title") or "")
            tokens = _tokenize(titles[number])
            pr_tokens[number] = tokens
            doc_freq.update(tokens)
        scored: list[tuple[float, int, str]] = []
        for number, tokens in pr_tokens.items():
            score, rare_shared = _shortlist_score(candidate_tokens, tokens, doc_freq)
            if deep or score >= TITLE_SCORE_THRESHOLD or rare_shared:
                scored.append((score, number, titles[number]))
        scored.sort(key=lambda entry: (-entry[0], entry[1]))
        shortlist = scored if deep else scored[:MAX_SHORTLIST]
        flagged: list[dict[str, Any]] = []
        truncated_files: list[int] = []
        for score, number, pr_title in shortlist:
            pr_files, files_truncated = await _fetch_pr_files(repo, number)
            if files_truncated:
                truncated_files.append(number)
            shared, jaccard, candidate_share = _overlap_stats(files, pr_files)
            if not _is_overlapping(shared, jaccard, candidate_share):
                continue
            flagged.append(
                {
                    "number": number,
                    "title": pr_title,
                    "shared": shared,
                    "jaccard": jaccard,
                    "candidate_share": candidate_share,
                    "similarity": _title_similarity(title, pr_title),
                }
            )
    except FileNotFoundError:
        return (
            "PR triage needs the GitHub CLI (`gh`), which is not installed or not on PATH.\n"
            "Install it from https://cli.github.com and run `gh auth login`, then retry."
        )
    except RuntimeError as e:
        return f"Duplicate pre-flight failed for {repo}: {e}\nRun `gh auth status` and `gh api rate_limit` if this keeps failing."

    flagged.sort(key=lambda entry: (-entry["shared"], -entry["jaccard"]))
    scope = "every open PR" if deep else f"up to {MAX_SHORTLIST} title-screened open PRs (pass deep=True to check every open PR)"
    lines = [f'# Duplicate pre-flight — "{title}" ({len(files)} files)', ""]
    lines.append(f"Checked {len(nodes)} of {total} open pull requests in {repo}; file-overlap fetch on {len(shortlist)} ({scope}).")
    coverage_note: list[str] = []
    if queue_truncated:
        coverage_note.append(
            f"the queue was truncated at the {TITLES_FETCH_CAP}-PR cap ({total} open) — the verdict covers the fetched subset only"
        )
    if truncated_files:
        ids = ", ".join(f"#{number}" for number in truncated_files[:10])
        coverage_note.append(
            f"file lists capped at {FILES_PAGE_SIZE * FILES_PAGE_CAP} files on {ids} — overlap on those PRs may be understated"
        )
    lines.append("")
    if flagged:
        lines.append("## Likely duplicate work")
        for entry in flagged:
            lines.append(
                f"- #{entry['number']} \"{entry['title']}\" — shares {entry['shared']} of the candidate's files "
                f"(Jaccard {entry['jaccard']:.2f}), title similarity {entry['similarity']:.2f}"
            )
            lines.append(f"  https://github.com/{repo}/pull/{entry['number']}")
        lines.append("")
        ids = ", ".join(f"#{entry['number']}" for entry in flagged)
        lines.append(f"Verdict: DUPLICATE RISK — read {ids} before starting; if the work is still needed, state the differences.")
    else:
        if deep:
            lines.append("Verdict: CLEAR — no open PR overlaps the planned files above the thresholds.")
        else:
            lines.append("Verdict: CLEAR — no title-screened open PR overlaps the planned files.")
    if coverage_note:
        lines.append("")
        lines.append("Coverage: " + "; ".join(coverage_note) + ".")
    return _truncate("\n".join(lines).rstrip() + "\n", max_output)


async def _infer_repo() -> str:
    """Infer OWNER/NAME from the origin remote of the current directory."""
    try:
        result = await asyncio.to_thread(
            subprocess.run,
            ["git", "remote", "get-url", "origin"],
            text=True,
            capture_output=True,
            check=False,
            timeout=GH_TIMEOUT_SECONDS,
        )
    except (OSError, subprocess.TimeoutExpired):
        return ""
    if result.returncode:
        return ""
    return _repo_from_url(result.stdout)


def _repo_from_url(url: str) -> str:
    """Extract OWNER/NAME from an https or ssh GitHub remote URL."""
    match = re.search(r"github\.com[:/]([^/]+)/([^/#?]+?)(?:\.git)?/?$", url.strip())
    if not match:
        return ""
    return f"{match.group(1)}/{match.group(2)}"
