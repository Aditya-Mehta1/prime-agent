from __future__ import annotations

import asyncio
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from unittest import mock

from rlm import bash
from rlm.bash import BASH_FORCE_PUSH_BYPASS_ENV, ForcePushRefusalError

# The package re-exports the bash() function under the same name, so reach the
# module through sys.modules for internals.
bash_module = sys.modules["rlm.bash"]

# Every spawned command, probe, and repo operation in this suite carries an
# explicit timeout.
AWAIT_TIMEOUT = 10.0
GIT_TIMEOUT = 60
KERNEL_LAUNCH_TIMEOUT = 90


FORCE_PUSH_LEAF = "git push -f origin main"


def _sh_payload_chain(depth: int, leaf: str = FORCE_PUSH_LEAF) -> str:
    """`sh -c` nested `depth` times around `leaf`, each layer JSON-quoted.

    Building the layers this way consumes one shell escaping layer per level,
    which is what made the third level and deeper invisible to a payload scan
    that only re-read the raw source text."""
    command = leaf
    for _ in range(depth):
        command = "sh -c " + json.dumps(command)
    return command


def _alternating_payload_chain(
    depth: int, first: str = "sh", leaf: str = FORCE_PUSH_LEAF
) -> str:
    """`sh -c` and `eval` alternating `depth` times around `leaf`.

    The two re-parsers consume escaping differently (`eval` keeps its raw
    sources, `sh -c` resolves them through the word scan), so a chain that
    starts with `eval` is only reachable through the folded-value look."""
    command = leaf
    for layer in range(depth):
        kind = first if layer % 2 == 0 else ("eval" if first == "sh" else "sh")
        command = ("sh -c " if kind == "sh" else "eval ") + json.dumps(command)
    return command


def _prepare(command: str) -> str:
    """The guard's own normalization pipeline, for detection-vector tests."""
    resolved = bash_module._fp_mask_redirections(
        bash_module._fp_normalize_continuations(command)
    )
    normalized, _index_map = bash_module._fp_strip_escapes(resolved)
    return normalized


def _guarded_runs(command: str) -> list[bash_module._FpPushArgs]:
    """Every parsed git push invocation the guard considers a force push."""
    words = bash_module._fp_scan_words(_prepare(command))
    runs = []
    for run in bash_module._fp_find_git_push_runs(words):
        args = bash_module._fp_parse_push_args(run.tokens, run.push_index)
        if bash_module._fp_is_guarded_push(args):
            runs.append(args)
    return runs


# Vectors for the force-push detector: an invocation must carry a force flag
# (`--force`, `-f` bundled with other short options, a `+`-prefixed refspec)
# and not be a dry run. Quoted command words and quoted flags fold into their
# values, so they must match like the unquoted forms. An unquoted echo of the
# same text matches too: conservative in the safe direction, exactly like the
# other kernel bash guards.
FORCE_PUSH_MATCHING_COMMANDS = [
    "git push --force origin main",
    "git push -f origin main",
    "git push origin main -f",
    "git push -f origin main:main",
    "git push -f origin main:refs/heads/main",
    "git push -f origin refs/heads/main",
    "git push -f origin HEAD:main",
    "git push -f origin :main",
    "git push -f origin main:",
    "git push -f origin @{u}",
    "git push origin +main",
    "git push origin +main:main",
    "git push origin +feature",
    "git push --force",
    "git push -f",
    "git push -f origin",
    "git push -f --all",
    "git push --force --mirror origin",
    "git push -fv origin main",
    "git push -f origin main --",
    "git push --force --repo=origin main",
    "git push -f --delete origin main",
    "git push --force-with-lease -f origin main",
    "/usr/bin/git push -f origin main",
    '"git" push -f origin main',
    "git 'push' -f origin main",
    "\\git push -f origin main",
    "sudo git push -f origin main",
    "FOO=1 git push -f origin main",
    "git -C repo push -f origin main",
    "git -c foo.bar=1 push -f origin main",
    "git --git-dir=.git push -f origin main",
    "echo $(git push -f origin main)",
    "git push -f origin \\\nmain",
    "git push 2>/dev/null -f origin main",
    "git push -f origin main 2>/dev/null",
    "(git push -f origin main)",
    "{ git push -f origin main; }",
    "git push -f origin main # ship it",
    "git push -f origin main && echo done",
    "echo git push -f origin main",
    "echo main | xargs git push -f origin",
    "git push -f origin $BRANCH",
    "git push -f origin HEAD",
    # The shell joins a backslash-newline continuation into one token, decodes
    # ANSI-C (`$'...'`) escapes, and resolves `$"..."` as a double-quoted
    # string, so these reach git as the force pushes they spell out.
    "git push -f origin ma\\\nin",
    "git push -\\\nf origin main",
    "gi\\\nt push -f origin main",
    'git push -f origin "ma\\\nin"',
    "$'git' push -f origin main",
    "$'\\x67it' push -f origin main",
    "$'\\u0067it' push -f origin main",
    '$"git" push -f origin main',
    "git $'push' -f origin main",
    "git push -$'f' origin main",
    "git push $'--force' origin main",
    # The kernel also runs on case-insensitive filesystems, where `GIT` and
    # `/usr/bin/GIT` resolve to the `git` binary.
    "GIT push -f origin main",
    "Git.exe push -f origin main",
    "/usr/bin/GIT push -f origin main",
    # git rewrites argv with an inline alias body before it parses it.
    "git -c alias.p='push -f origin main' p",
    "git -c alias.a=p -c alias.p='push -f origin main' a",
    "git -c alias.p='push -f origin main' -C repo p",
    # An alias whose name shadows a builtin is never used by git, so the
    # builtin push must still be found.
    "git -c alias.push='status' push -f origin main",
]

# Vectors whose push argument the shell expands. An unquoted `$f` can expand
# into `-f`, and any dynamic refspec can be a `+`-refspec naming a protected
# branch, so the scanner cannot prove these are not force pushes: they must be
# treated as guarded rather than allowed as plain pushes.
FORCE_PUSH_UNRESOLVABLE_ARGUMENT_COMMANDS = [
    "f=-f; git push $f origin main",
    "f='-f origin'; git push $f",
    "git push $REMOTE origin main",
    "git push origin $BRANCH",
    "BRANCH=+main; git push origin $BRANCH",
    "git push origin 'main*'",
    "git push --repo=$REMOTE main",
    "git push --force-with-lease origin $BRANCH",
    # `-f` cancels the lease compare-and-swap: measured on git 2.55 over a
    # deliberately stale remote-tracking ref, a bare `--force-with-lease` is
    # rejected as `stale info` while `--force-with-lease -f`,
    # `--force-with-lease origin +main`, and `X=-f; git push --force-with-lease
    # origin $X` all rewrite main.
    "X=-f; git push --force-with-lease origin $X",
]

FORCE_PUSH_NON_MATCHING_COMMANDS = [
    "git push origin main",
    "git push",
    "git push origin",
    "git push -u origin main",
    "git push --all",
    "git push --mirror origin",
    "git push --tags",
    "git push origin --delete main",
    "git push --force-with-lease origin main",
    "git push --force-with-lease=main:expected origin main",
    "git push --force-if-includes origin main",
    "git push --force-with-lease --force-if-includes origin main",
    "git push -n origin main",
    "git push -f -n origin main",
    "git push -fn origin main",
    "git push -nf origin main",
    "git push --dry-run -f origin main",
    "git push -v -q origin main",
    "git checkout --force main",
    "git config push.default matching",
    "git status",
    "echo hello",
    "npm run check",
    "echo 'git push -f origin main'",
    'echo "git push -f origin main"',
    "# git push -f origin main",
    "git --exec-path push -f origin main",
    # Wrappers and quoting that do not carry a push stay inert.
    "git -c alias.s=status s",
    "git -c alias.co=checkout co",
    "git -c alias.push='status' push --dry-run -f origin main",
    "env -C . echo hi",
    "env -S 'git status'",
    "printf $'%s\\n' hi",
    'echo $"hello"',
    "echo $'tab\\there'",
]


class ForcePushDetectionTest(unittest.TestCase):
    def test_matches_force_pushes(self):
        for command in FORCE_PUSH_MATCHING_COMMANDS:
            with self.subTest(command=command):
                self.assertTrue(_guarded_runs(command))

    def test_matches_unresolvable_push_arguments(self):
        for command in FORCE_PUSH_UNRESOLVABLE_ARGUMENT_COMMANDS:
            with self.subTest(command=command):
                self.assertTrue(_guarded_runs(command))

    def test_does_not_match_other_commands(self):
        for command in FORCE_PUSH_NON_MATCHING_COMMANDS:
            with self.subTest(command=command):
                self.assertFalse(_guarded_runs(command))

    def test_force_with_lease_is_never_a_bare_force(self):
        args = bash_module._fp_parse_push_args(
            ["git", "push", "--force-with-lease=main:expected", "origin", "main"], 1
        )
        self.assertFalse(args.force)
        self.assertTrue(args.dry_run is False)


class ForcePushScannerFidelityTest(unittest.TestCase):
    """The scan must read the text the way the shell does."""

    def test_line_continuations_join_words(self):
        self.assertEqual(_prepare("git push -f origin ma\\\nin"), "git push -f origin main")
        self.assertEqual(_prepare("gi\\\nt push -\\\nf origin main"), "git push -f origin main")

    def test_ansi_c_words_decode_like_the_shell(self):
        for command, expected in [
            ("$'git'", "git"),
            ("$'\\x67it'", "git"),
            ("$'\\u0067it'", "git"),
            ("$'\\101BC'", "ABC"),
            ('$"git"', "git"),
            ("$'ma\\in'", "main"),
        ]:
            with self.subTest(command=command):
                words = bash_module._fp_scan_words(_prepare(command))
                self.assertEqual([word.value for word in words], [expected])

    def test_double_quoted_escape_does_not_end_the_string(self):
        command = 'echo "a \\" b"'
        stripped, _index_map = bash_module._fp_strip_escapes(command)
        self.assertEqual(stripped, command)
        words = bash_module._fp_scan_words(_prepare(command))
        self.assertEqual([word.value for word in words], ["echo", 'a " b'])

    def test_redirections_inside_double_quotes_stay_visible(self):
        # The quoted span is data: masking it as a redirection would change the
        # word the target check reads.
        command = 'git push -f origin " > x" main'
        self.assertIn('" > x"', _prepare(command))

    @unittest.skipUnless(
        hasattr(bash_module, "_fp_payload_hides_force_push"),
        "the payload walk was added with the force-push guard hardening; the"
        " same vectors are covered end to end by test_refuses_nested_payloads",
    )
    def test_deep_payload_chains_are_refused_by_the_depth_cap(self):
        # The payload walk counts two levels per nesting layer and refuses once
        # it passes _FP_MAX_PAYLOAD_DEPTH, so a chain it cannot follow is
        # refused rather than missed: a benign chain past the cap is refused.
        self.assertLessEqual(
            getattr(bash_module, "_FP_MAX_PAYLOAD_DEPTH", 10),
            10,
            "the payload depth cap must stay small enough to bound the walk",
        )
        for depth in (2, 3, 5):
            with self.subTest(depth=depth):
                self.assertFalse(
                    bash_module._fp_payload_hides_force_push(
                        _sh_payload_chain(depth, "git status")
                    )
                )
        for depth in (8, 12):
            with self.subTest(depth=depth):
                self.assertTrue(
                    bash_module._fp_payload_hides_force_push(
                        _sh_payload_chain(depth, "git status")
                    )
                )
                self.assertTrue(
                    bash_module._fp_payload_hides_force_push(_sh_payload_chain(depth))
                )

    def test_url_and_scp_remotes_are_not_refspecs(self):
        for first in [
            "https://example.invalid/x.git",
            "ssh://example.invalid/x.git",
            "git@github.com:org/repo.git",
            "example.invalid:org/repo.git",
            "C:\\repo",
        ]:
            with self.subTest(first=first):
                args = bash_module._fp_parse_push_args(["git", "push", "-f", first], 1)
                self.assertEqual(args.refspecs, [])
        # A refspec that merely carries a colon still names its target.
        args = bash_module._fp_parse_push_args(["git", "push", "-f", "origin", "main:main"], 1)
        self.assertEqual(args.refspecs, ["main:main"])
        args = bash_module._fp_parse_push_args(["git", "push", "-f", "main:main"], 1)
        self.assertEqual(args.refspecs, ["main:main"])



class ForcePushEvalPayloadTest(unittest.TestCase):
    def test_eval_payloads_hiding_force_pushes(self):
        for command in [
            "eval 'git push -f origin main'",
            'eval "git push -f origin main"',
            "eval 'git push --force'",
            "eval 'git push origin +main'",
            "eval 'cd repo && git push -f'",
            "eval 'echo x; git push -f origin main'",
            'eval \'eval "git push -f origin main"\'',
            # A payload holding another payload: the inner command only exists
            # after the outer one runs, so each scanner must consult the others.
            'eval \'sh -c "git push -f origin main"\'',
            "eval " + json.dumps(_sh_payload_chain(3)),
            _alternating_payload_chain(5, "eval"),
            _alternating_payload_chain(15, "eval"),
        ]:
            with self.subTest(command=command):
                self.assertTrue(
                    bash_module._fp_eval_payloads_hide_force_push(command)
                )

    def test_safe_eval_payloads_stay_unflagged(self):
        for command in [
            "eval 'git push --force-with-lease origin main'",
            "eval 'git push origin main'",
            "eval 'echo hi'",
            "eval \"echo 'git push -f origin main'\"",
            "eval 'git status'",
            "eval " + json.dumps(_sh_payload_chain(3, "git status")),
        ]:
            with self.subTest(command=command):
                self.assertFalse(
                    bash_module._fp_eval_payloads_hide_force_push(command)
                )


class ForcePushShellCPayloadTest(unittest.TestCase):
    def test_shell_c_payloads_hiding_force_pushes(self):
        for command in [
            "sh -c 'git push -f origin main'",
            "bash -c 'git push -f origin main'",
            "bash -lc 'git push --force origin main'",
            "sh -c 'cd repo && git push -f'",
            'sh -c "eval \'git push -f origin main\'"',
            'sh -c "env -S \'git push -f origin main\'"',
            # Deeper chains: every nesting level consumes one escaping layer,
            # so these are only reachable through the folded-value look.
            _sh_payload_chain(3),
            _sh_payload_chain(4),
            _sh_payload_chain(5),
            _alternating_payload_chain(5),
            _alternating_payload_chain(15),
        ]:
            with self.subTest(command=command):
                self.assertTrue(
                    bash_module._fp_shell_c_payloads_hide_force_push(command)
                )

    def test_safe_shell_c_payloads_stay_unflagged(self):
        for command in [
            "bash -c 'git push --force-with-lease origin main'",
            "bash -c 'echo hi'",
            'bash -c \'echo "git push -f origin main"\'',
            "bash -c 'git status'",
            'sh -c "eval \'echo hi\'"',
            "env -S 'sh -c \"git status\"'",
            # Nested chains the guard can still follow stay unflagged, and so
            # does a nested literal lease push.
            _sh_payload_chain(2, "git status"),
            _sh_payload_chain(3, "git status"),
            _sh_payload_chain(3, "git push --force-with-lease origin feature"),
        ]:
            with self.subTest(command=command):
                self.assertFalse(
                    bash_module._fp_shell_c_payloads_hide_force_push(command)
                )


@unittest.skipUnless(
    hasattr(bash_module, "_fp_env_payloads_hide_force_push"),
    "the env-payload scanner was added with the force-push guard hardening;"
    " its vectors are covered end to end by test_refuses_nested_payloads",
)
class ForcePushEnvPayloadTest(unittest.TestCase):
    """`env -S`/`--split-string` splits one word into the argv git receives."""

    def test_env_payloads_hiding_force_pushes(self):
        for command in [
            "env -S 'git push -f origin main'",
            "env --split-string 'git push -f origin main'",
            "env -iS'git push -f origin main'",
            "env --split-string='git push -f origin main'",
            # A payload holding another payload: the inner command exists only
            # after env runs, so the scanners have to consult each other.
            "env -S 'eval \"git push -f origin main\"'",
            "env -S 'sh -c \"git push -f origin main\"'",
            "env -S " + json.dumps(_sh_payload_chain(3)),
        ]:
            with self.subTest(command=command):
                self.assertTrue(
                    bash_module._fp_env_payloads_hide_force_push(command)
                )

    def test_env_payloads_without_a_force_push_stay_unflagged(self):
        for command in [
            "env -S 'git status'",
            "env -S 'echo hi'",
            "env -S 'git push --force-with-lease origin feature'",
            "env --split-string 'git status'",
            "env -C . git status",
            "env VERSION=1 git status",
            "echo env -S",
            "env -S " + json.dumps(_sh_payload_chain(2, "git status")),
        ]:
            with self.subTest(command=command):
                self.assertFalse(
                    bash_module._fp_env_payloads_hide_force_push(command)
                )


class ForcePushGuardSuite(unittest.IsolatedAsyncioTestCase):
    """End-to-end: bash() refuses before spawning anything."""

    def setUp(self):
        self._prev_cwd = os.getcwd()
        self._prev_env = dict(os.environ)
        os.environ.pop(BASH_FORCE_PUSH_BYPASS_ENV, None)
        os.environ.pop("PRIME_AGENT_BASH_COMMAND_PREFIX", None)
        # The launch-time bypass snapshot is a module attribute frozen at
        # import; pin it to "unset" so tests stay deterministic.
        frozen_patch = mock.patch.object(
            bash_module, "_FORCE_PUSH_BYPASS_AT_KERNEL_START", False
        )
        frozen_patch.start()
        self.addCleanup(frozen_patch.stop)
        late_warn_patch = mock.patch.object(
            bash_module, "_force_push_late_bypass_warned", False
        )
        late_warn_patch.start()
        self.addCleanup(late_warn_patch.stop)
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        # Restore cwd before the temp dir disappears (cleanups run LIFO).
        self.addCleanup(self._restore_env)
        self.addCleanup(os.chdir, self._prev_cwd)
        self.test_dir = Path(temp.name)
        os.chdir(self.test_dir)

    def _restore_env(self):
        os.environ.clear()
        os.environ.update(self._prev_env)

    def _git(self, *args: str, cwd: Path, check: bool = True) -> subprocess.CompletedProcess:
        completed = subprocess.run(
            ["git", *args],
            cwd=str(cwd),
            capture_output=True,
            text=True,
            timeout=GIT_TIMEOUT,
        )
        if check and completed.returncode != 0:
            raise AssertionError(f"git {' '.join(args)!r} failed: {completed.stderr}")
        return completed

    def _make_repo(self, name: str, branch: str = "feature") -> tuple[Path, Path]:
        """A local repo with a bare remote, main pushed, and `branch` checked
        out tracking its own name (upstream: <remote>/<branch>)."""
        repo = self.test_dir / name
        repo.mkdir()
        bare = self.test_dir / f"{name}-remote.git"
        self._git("init", "-q", "--bare", "-b", "main", str(bare), cwd=self.test_dir)
        self._git("init", "-q", "-b", "main", cwd=repo)
        self._git("config", "user.email", "guard@example.com", cwd=repo)
        self._git("config", "user.name", "Guard Test", cwd=repo)
        self._git("config", "commit.gpgsign", "false", cwd=repo)
        self._git("config", "tag.gpgsign", "false", cwd=repo)
        (repo / "file.txt").write_text("one\n")
        self._git("add", ".", cwd=repo)
        self._git("commit", "-q", "-m", "init", cwd=repo)
        self._git("remote", "add", "origin", str(bare), cwd=repo)
        self._git("push", "-q", "-u", "origin", "main", cwd=repo)
        if branch != "main":
            self._git("switch", "-c", branch, cwd=repo)
            self._git("push", "-q", "-u", "origin", branch, cwd=repo)
        return repo, bare

    def _diverge(self, repo: Path, bare: Path, branch: str) -> None:
        """Make local and remote `branch` histories diverge, so pushing needs force."""
        clone = self.test_dir / f"{repo.name}-clone"
        self._git("clone", "-q", "-b", branch, str(bare), str(clone), cwd=self.test_dir)
        self._git("config", "user.email", "guard@example.com", cwd=clone)
        self._git("config", "user.name", "Guard Test", cwd=clone)
        self._git("config", "commit.gpgsign", "false", cwd=clone)
        (clone / "file.txt").write_text("remote\n")
        self._git("commit", "-q", "-am", "remote change", cwd=clone)
        self._git("push", "-q", "origin", f"HEAD:refs/heads/{branch}", cwd=clone)
        (repo / "file.txt").write_text("local\n")
        self._git("commit", "-q", "-am", "local change", cwd=repo)

    async def _run(self, command: str, **kwargs) -> object:
        return await asyncio.wait_for(bash(command, **kwargs), AWAIT_TIMEOUT)

    async def _refused(self, command: str) -> str:
        try:
            handle = bash(command)
        except ForcePushRefusalError as refusal:
            return str(refusal)
        # Only reachable while the guard is broken: the command was already
        # spawned, so stop it before failing the test.
        handle.kill()
        self.fail(f"expected a force-push refusal for {command!r}")

    async def test_refuses_force_push_to_main_and_master(self):
        repo, _bare = self._make_repo("repo-main")
        os.chdir(repo)
        for command in [
            "git push -f origin main",
            "git push --force origin main",
            "git push -f origin main:main",
            "git push -f origin refs/heads/main",
            "git push -f origin HEAD:main",
            "git push origin +main",
            "git push -f origin master",
            "git push -f origin main:refs/heads/main",
            "echo ok; git push -f origin main",
        ]:
            with self.subTest(command=command):
                message = await self._refused(command)
                self.assertIn("Refusing to run this force-push command", message)
                self.assertIn("main", message)

    async def test_refuses_implicit_force_push_to_upstream(self):
        repo, _bare = self._make_repo("repo-upstream")
        os.chdir(repo)
        for command in ["git push -f", "git push --force", "git push -f origin"]:
            with self.subTest(command=command):
                message = await self._refused(command)
                self.assertIn("upstream", message)
                # The probe identified the real upstream: the bare remote.
                self.assertIn("origin/feature", message)
        # A second branch tracking main keeps the implicit vector live.
        self._git("switch", "main", cwd=repo)
        message = await self._refused("git push -f")
        self.assertIn("origin/main", message)

    async def test_force_with_lease_and_plain_pushes_allowed(self):
        repo, _bare = self._make_repo("repo-lease")
        os.chdir(repo)
        result = await self._run("git push --force-with-lease origin feature")
        self.assertEqual(result.exit_code, 0)
        result = await self._run("git push --force-with-lease=feature origin feature")
        self.assertEqual(result.exit_code, 0)
        result = await self._run("git push --force-if-includes --force-with-lease origin feature")
        self.assertEqual(result.exit_code, 0)
        result = await self._run("git push origin feature")
        self.assertEqual(result.exit_code, 0)
        result = await self._run("git push")
        self.assertEqual(result.exit_code, 0)
        result = await self._run("echo hi")
        self.assertEqual(result.exit_code, 0)

    async def test_force_push_to_own_feature_branch_allowed(self):
        repo, bare = self._make_repo("repo-feature")
        self._diverge(repo, bare, "feature")
        os.chdir(repo)
        # Without force the push is rejected by git itself; the guard allows it.
        plain = await self._run("git push origin feature")
        self.assertNotEqual(plain.exit_code, 0)
        for command in [
            "git push -f origin feature",
            "git push -f origin HEAD",
            "git push -f origin HEAD:refs/heads/feature",
            "git push origin +feature",
        ]:
            with self.subTest(command=command):
                result = await self._run(command)
                self.assertEqual(result.exit_code, 0, result.output)

    async def test_dry_run_force_pushes_allowed(self):
        repo, _bare = self._make_repo("repo-dry")
        os.chdir(repo)
        result = await self._run("git push --dry-run -f origin main")
        self.assertEqual(result.exit_code, 0)
        result = await self._run("git push -n --force origin main")
        self.assertEqual(result.exit_code, 0)

    async def test_refuses_wildcard_force_pushes(self):
        repo, _bare = self._make_repo("repo-wild")
        os.chdir(repo)
        for command in ["git push -f --all", "git push --force --mirror origin"]:
            with self.subTest(command=command):
                message = await self._refused(command)
                self.assertIn("every branch", message)

    async def test_refuses_xargs_fed_force_push(self):
        repo, _bare = self._make_repo("repo-xargs")
        os.chdir(repo)
        message = await self._refused("echo main | xargs git push -f origin")
        self.assertIn("xargs", message)

    async def test_refuses_unresolvable_refspecs(self):
        repo, _bare = self._make_repo("repo-unresolvable")
        os.chdir(repo)
        for command in ["git push -f origin $BRANCH", "git push -f origin 'main*'"]:
            with self.subTest(command=command):
                message = await self._refused(command)
                self.assertIn("cannot be verified statically", message)

    async def test_refuses_explicit_upstream_refspecs(self):
        repo, _bare = self._make_repo("repo-at-u")
        os.chdir(repo)
        for command in ["git push -f origin @{u}", "git push -f origin @{upstream}"]:
            with self.subTest(command=command):
                message = await self._refused(command)
                self.assertIn("upstream", message)

    async def test_head_target_refused_on_main_allowed_on_feature(self):
        repo, _bare = self._make_repo("repo-head", branch="main")
        os.chdir(repo)
        message = await self._refused("git push -f origin HEAD")
        self.assertIn("HEAD", message)
        self.assertIn("main", message)
        feature_repo, _fb = self._make_repo("repo-head-feature", branch="feature")
        os.chdir(feature_repo)
        result = await self._run("git push -f origin HEAD")
        self.assertEqual(result.exit_code, 0, result.output)

    async def test_kwarg_bypass_runs_the_force_push(self):
        repo, bare = self._make_repo("repo-kwarg")
        self._diverge(repo, bare, "feature")
        os.chdir(repo)
        before = self._git("ls-remote", str(bare), "refs/heads/feature", cwd=self.test_dir).stdout
        result = await self._run("git push -f origin feature", allow_force_push=True)
        self.assertEqual(result.exit_code, 0, result.output)
        after = self._git("ls-remote", str(bare), "refs/heads/feature", cwd=self.test_dir).stdout
        self.assertNotEqual(before, after)

    async def test_refusal_lists_both_bypasses_and_the_safe_alternative(self):
        repo, _bare = self._make_repo("repo-message")
        os.chdir(repo)
        message = await self._refused("git push -f origin main")
        self.assertIn("--force-with-lease", message)
        self.assertIn("allow_force_push", message)
        self.assertIn(BASH_FORCE_PUSH_BYPASS_ENV, message)

    async def test_warns_once_about_late_bypass(self):
        repo, _bare = self._make_repo("repo-warn")
        os.chdir(repo)
        stderr = io.StringIO()
        with (
            mock.patch.dict(os.environ, {BASH_FORCE_PUSH_BYPASS_ENV: "1"}),
            redirect_stderr(stderr),
        ):
            message = await self._refused("git push -f origin main")
        self.assertIn("Refusing to run this force-push command", message)
        warning = stderr.getvalue()
        self.assertIn(BASH_FORCE_PUSH_BYPASS_ENV, warning)
        self.assertIn("appeared after kernel start", warning)
        # The warning fires once, and a falsy mid-session value stays inert.
        second = io.StringIO()
        with (
            mock.patch.dict(os.environ, {BASH_FORCE_PUSH_BYPASS_ENV: "0"}),
            redirect_stderr(second),
        ):
            await self._refused("git push -f origin main")
        self.assertEqual(second.getvalue(), "")

    async def test_guard_probes_only_on_pattern_match(self):
        repo, _bare = self._make_repo("repo-probe")
        os.chdir(repo)
        with mock.patch.object(
            bash_module, "_fp_probe_upstream", wraps=bash_module._fp_probe_upstream
        ) as probe:
            result = await self._run("git push origin feature")
            self.assertEqual(result.exit_code, 0)
            result = await self._run("echo hi")
            self.assertEqual(result.exit_code, 0)
        self.assertEqual(probe.call_count, 0)

    async def test_cd_replay_targets_the_right_repo(self):
        repo_a, _ba = self._make_repo("repo-a")  # feature tracks origin/feature
        repo_b, _bb = self._make_repo("repo-b")  # feature tracks origin/feature
        message = await self._refused(f"cd {repo_b.name} && git push -f")
        # The probe must have run inside repo_b, identifying ITS upstream.
        self.assertIn("origin/feature", message)
        with mock.patch.object(bash_module, "_fp_probe_upstream", wraps=bash_module._fp_probe_upstream) as probe:
            await self._refused(f"cd {repo_b.name} && git push -f")
            self.assertEqual(probe.call_count, 1)
            cwd = probe.call_args[0][0]
            self.assertEqual(Path(cwd).resolve(), repo_b.resolve())

    async def test_unresolvable_relocations_refused(self):
        repo, _bare = self._make_repo("repo-reloc")
        for command in [
            "cd - && git push -f",
            f"GIT_DIR={repo}/.git git push -f",
            f"git -C {repo.name} push -f",
            f"cd {repo.name}; git push -f",
        ]:
            with self.subTest(command=command):
                message = await self._refused(command)
                self.assertIn("Refusing to run this force-push command", message)

    async def test_subshell_cd_chain_replayed(self):
        repo_b, _bb = self._make_repo("repo-sub")
        message = await self._refused(f"(cd {repo_b.name} && git push -f)")
        # The open subshell's cd is replayed, so the probe names repo_b's upstream.
        self.assertIn("origin/feature", message)
        # A group that opens and closes before the push never relocates it:
        # the probe runs at the workspace root, which is not a repository,
        # so git fails open and the push errors out.
        result = await self._run(f"(cd {repo_b.name}; echo ok) && git push -f || echo no-upstream")
        self.assertEqual(result.exit_code, 0, result.output)
        # A multi-statement subshell the resolver cannot replay stays
        # refused: conservative in the safe direction.
        with self.assertRaises(ForcePushRefusalError):
            bash(f"cd sub && (cd {repo_b.name}; ls; git push -f)")

    async def test_command_prefix_force_push_guarded(self):
        repo, _bare = self._make_repo("repo-prefix")
        with mock.patch.dict(
            os.environ, {"PRIME_AGENT_BASH_COMMAND_PREFIX": "git push -f origin main"}
        ):
            message = await self._refused("echo hi")
        self.assertIn("Refusing to run this force-push command", message)

    async def test_command_prefix_relocation_refused_for_implicit_force(self):
        repo, _bare = self._make_repo("repo-prefix-rel")
        os.chdir(repo)
        with mock.patch.dict(
            os.environ, {"PRIME_AGENT_BASH_COMMAND_PREFIX": "cd somewhere-else"}
        ):
            message = await self._refused("git push -f")
        self.assertIn("Refusing to run this force-push command", message)

    async def test_command_prefix_relocation_allows_explicit_targets(self):
        repo, _bare = self._make_repo("repo-prefix-ok")
        os.chdir(repo)
        with mock.patch.dict(
            os.environ, {"PRIME_AGENT_BASH_COMMAND_PREFIX": "cd somewhere-else"}
        ):
            result = await self._run("git push -f origin feature")
            self.assertEqual(result.exit_code, 0, result.output)


    async def test_refuses_implicit_force_push_without_upstream(self):
        repo, _bare = self._make_repo("repo-no-upstream")
        # push.default=current maps the implicit refspec onto origin/main, and
        # the branch has no upstream for the guard to probe.
        self._git("config", "push.default", "current", cwd=repo)
        self._git("branch", "--unset-upstream", cwd=repo)
        os.chdir(repo)
        for command in ["git push -f origin", "git push -f"]:
            with self.subTest(command=command):
                message = await self._refused(command)
                self.assertIn("push.default", message)

    async def test_refuses_url_and_scp_remote_force_pushes(self):
        repo, bare = self._make_repo("repo-url-remote")
        os.chdir(repo)
        # git reads the first positional as the repository, so a colon inside a
        # URL is not a refspec separator: the push is implicit and takes its
        # target from the current branch.
        for command in [
            f"git push -f file://{bare}",
            "git push -f https://example.invalid/x.git",
            "git push -f git@github.com:org/repo.git",
        ]:
            with self.subTest(command=command):
                message = await self._refused(command)
                self.assertIn("upstream", message)

    async def test_refuses_ansi_c_quoted_force_push(self):
        repo, _bare = self._make_repo("repo-ansi-c")
        os.chdir(repo)
        for command in [
            "$'git' push -f origin main",
            "$'\\x67it' push -f origin main",
            "$'\\u0067it' push -f origin main",
            '$"git" push -f origin main',
            "git $'push' -f origin main",
            "git push -$'f' origin main",
            "git push $'--force' origin main",
        ]:
            with self.subTest(command=command):
                message = await self._refused(command)
                self.assertIn("main", message)
        # An ANSI-C payload hides a whole command; it is refused outright.
        for command in [
            "sh -c $'git push -f origin main'",
            "eval $'git push -f origin main'",
        ]:
            with self.subTest(command=command):
                await self._refused(command)

    async def test_ansi_c_quoting_without_a_push_stays_allowed(self):
        repo, _bare = self._make_repo("repo-ansi-c-ok")
        os.chdir(repo)
        result = await self._run("printf $'%s\\n' hi")
        self.assertEqual(result.exit_code, 0, result.output)
        result = await self._run("echo $'tab\\there'")
        self.assertEqual(result.exit_code, 0, result.output)

    async def test_refuses_line_continuations_that_join_tokens(self):
        repo, _bare = self._make_repo("repo-continuation")
        os.chdir(repo)
        for command in [
            "git push -f origin ma\\\nin",
            "git push -\\\nf origin main",
            "gi\\\nt push -f origin main",
            'git push -f origin "ma\\\nin"',
        ]:
            with self.subTest(command=command):
                message = await self._refused(command)
                self.assertIn("main", message)
        # A path that merely looks like a continuation is not one.
        message = await self._refused("git push -f origin ma\\\nin")
        self.assertIn("main", message)

    async def test_refuses_dynamic_push_arguments(self):
        repo, _bare = self._make_repo("repo-dynamic")
        os.chdir(repo)
        for command in [
            "f=-f; git push $f origin main",
            "f='-f origin'; git push $f",
            "BRANCH=+main; git push origin $BRANCH",
            "git push origin $BRANCH",
            "git push --force-with-lease origin $BRANCH",
        ]:
            with self.subTest(command=command):
                message = await self._refused(command)
                self.assertIn("cannot be verified statically", message)

    async def test_refuses_aliased_force_push(self):
        repo, _bare = self._make_repo("repo-alias")
        os.chdir(repo)
        for command in [
            "git -c alias.p='push -f origin main' p",
            "git -c alias.a=p -c alias.p='push -f origin main' a",
            "git -c alias.p='push -f origin main' -C . p",
        ]:
            with self.subTest(command=command):
                message = await self._refused(command)
                self.assertIn("main", message)
        # A shell-alias body, a body from the environment, and a chain longer
        # than the guard follows are refused rather than guessed at.
        for command in [
            "git -c alias.p='!git push -f origin main' p",
            "git --config-env=alias.p=BODY p",
            "git -c alias.a=b -c alias.b=c -c alias.c=d -c alias.d=e"
            " -c alias.e=f -c alias.f=g -c alias.g=h -c alias.h=i -c alias.i=j"
            " -c alias.j=k -c alias.k='push -f origin main' a",
        ]:
            with self.subTest(command=command):
                message = await self._refused(command)
                self.assertIn("alias", message)

    async def test_inline_aliases_without_a_push_stay_allowed(self):
        repo, _bare = self._make_repo("repo-alias-ok")
        os.chdir(repo)
        result = await self._run("git -c alias.s=status s")
        self.assertEqual(result.exit_code, 0, result.output)
        result = await self._run("git -c alias.co=checkout co")
        self.assertEqual(result.exit_code, 0, result.output)
        # An alias that shadows the builtin name is never used by git, so the
        # builtin push is what runs and the guard still sees it.
        message = await self._refused("git -c alias.push='status' push -f origin main")
        self.assertIn("main", message)

    async def test_refuses_subcommands_git_may_resolve_through_an_alias(self):
        repo, bare = self._make_repo("repo-repo-alias")
        # A repo-local alias is invisible in the command text: `git p` looks
        # like an unknown subcommand, and git rewrites it into a force push.
        self._git("config", "alias.p", "push -f origin main", cwd=repo)
        os.chdir(repo)
        before = self._git("rev-parse", "main", cwd=bare).stdout
        message = await self._refused("git p")
        self.assertIn("alias", message)
        self.assertEqual(self._git("rev-parse", "main", cwd=bare).stdout, before)
        # A global alias is just as invisible.
        message = await self._refused("git -c alias.q='push -f origin main' q")
        self.assertIn("main", message)
        # Built-in subcommands are never resolved through an alias, so they
        # keep running.
        result = await self._run("git status")
        self.assertEqual(result.exit_code, 0, result.output)

    async def test_refuses_trailing_line_continuation(self):
        repo, _bare = self._make_repo("repo-trailing-backslash")
        os.chdir(repo)
        # The kernel appends the command to a script, so a trailing backslash
        # joins it with text the guard cannot see.
        message = await self._refused("git push -f origin main\\")
        self.assertIn("line continuation", message)
        # An escaped backslash is not a continuation.
        result = await self._run("git push -f origin main\\\\")
        self.assertNotEqual(result.exit_code, 0)  # git rejects the refspec

    async def test_refuses_case_insensitive_git_command_names(self):
        repo, _bare = self._make_repo("repo-case")
        os.chdir(repo)
        for command in [
            "GIT push -f origin main",
            "Git.EXE push -f origin main",
            "/usr/bin/GIT push -f origin main",
            "SUDO git push -f origin main",
        ]:
            with self.subTest(command=command):
                message = await self._refused(command)
                self.assertIn("main", message)

    async def test_refuses_env_relocation_and_split_string(self):
        repo, _bare = self._make_repo("repo-env")
        os.chdir(self.test_dir)  # not a repository: git fails the push itself
        message = await self._refused(f"env -C {repo} git push -f origin")
        self.assertIn("changes directory", message)
        message = await self._refused(f"env --chdir={repo} git push -f origin")
        self.assertIn("changes directory", message)
        message = await self._refused(f"env --chdir {repo} git push -f origin")
        self.assertIn("changes directory", message)
        for command in [
            "env -S 'git push -f origin main'",
            "env -iS'git push -f origin main'",
            "env --split-string='git push -f origin main'",
        ]:
            with self.subTest(command=command):
                message = await self._refused(command)
                self.assertIn("env -S", message)

    async def test_env_wrappers_without_a_hidden_push_stay_allowed(self):
        repo, _bare = self._make_repo("repo-env-ok")
        os.chdir(repo)
        result = await self._run("env -S 'git status'")
        self.assertEqual(result.exit_code, 0, result.output)
        result = await self._run("env -S 'git push --force-with-lease origin feature'")
        self.assertEqual(result.exit_code, 0, result.output)
        result = await self._run("env echo hi")
        self.assertEqual(result.exit_code, 0, result.output)

    async def test_force_push_allowlist_still_runs(self):
        repo, bare = self._make_repo("repo-allowlist")
        self._diverge(repo, bare, "feature")
        os.chdir(repo)
        for command in [
            "git push -f origin feature",
            "git push origin +feature",
            "git push --force-with-lease origin feature",
            "git push --force-if-includes --force-with-lease origin feature",
            "git push origin feature",
            "git push --tags",
        ]:
            with self.subTest(command=command):
                result = await self._run(command)
                self.assertEqual(result.exit_code, 0, result.output)


    async def test_refuses_nested_payloads(self):
        repo, _bare = self._make_repo("repo-nested-payload")
        os.chdir(repo)
        for command in [
            """env -S 'sh -c "git push -f origin main"'""",
            """eval 'sh -c "git push -f origin main"'""",
            """sh -c "eval 'git push -f origin main'" """,
            """env -S 'eval "git push -f origin main"'""",
            """sh -c "env -S 'sh -c \\"git push -f origin main\\"'" """,
            # Three or more re-parser layers, in both chain shapes, and an
            # `env -S` wrapping a three-layer chain.
            _sh_payload_chain(3),
            _sh_payload_chain(4),
            _sh_payload_chain(5),
            "env -S " + json.dumps(_sh_payload_chain(3)),
            _alternating_payload_chain(5, "sh"),
            _alternating_payload_chain(5, "eval"),
            _alternating_payload_chain(15, "eval"),
            # Past the depth cap the guard refuses rather than guess.
            _sh_payload_chain(8),
        ]:
            with self.subTest(command=command):
                await self._refused(command)

    async def test_nested_payloads_without_a_force_push_stay_allowed(self):
        repo, _bare = self._make_repo("repo-nested-payload-ok")
        os.chdir(repo)
        for command in [
            """env -S 'sh -c "git status"'""",
            """sh -c "eval 'echo hi'" """,
            """env -S 'sh -c "git push --force-with-lease origin feature"'""",
            _sh_payload_chain(2, "git status"),
            _sh_payload_chain(3, "git status"),
            "env -S " + json.dumps(_sh_payload_chain(2, "git status")),
            _sh_payload_chain(3, "git push --force-with-lease origin feature"),
            # An echo of the payload text is not a push, at any level the guard
            # can still follow.
            """echo "sh -c \\"git push -f origin main\\"" """,
        ]:
            with self.subTest(command=command):
                result = await self._run(command)
                self.assertEqual(result.exit_code, 0, result.output)

    async def test_refuses_implicit_force_push_on_detached_head(self):
        repo, bare = self._make_repo("repo-detached", branch="main")
        # push.default=matching ignores a detached HEAD and still force-pushes
        # every branch whose name exists on the remote, main included.
        self._git("config", "push.default", "matching", cwd=repo)
        self._diverge(repo, bare, "main")
        self._git("checkout", "--detach", cwd=repo)
        os.chdir(repo)
        for command in ["git push -f origin", "git push -f"]:
            with self.subTest(command=command):
                message = await self._refused(command)
                self.assertIn("push.default", message)
        # A detached HEAD is not a blanket refusal: an explicit non-protected
        # refspec still runs.
        self._git("branch", "feature", cwd=repo)
        result = await self._run("git push -f origin feature")
        self.assertEqual(result.exit_code, 0, result.output)

    async def test_alias_named_after_a_git_command_is_inert(self):
        repo, _bare = self._make_repo("repo-alias-inert")
        os.chdir(repo)
        # git runs its own command, never the alias, so these must not be
        # refused (and must really run).
        for command in [
            "git -c alias.status='push -f origin main' status --short",
            "git -c alias.log='push -f origin main' log --oneline -1",
            "git -c alias.submodule='push -f origin main' submodule status",
        ]:
            with self.subTest(command=command):
                result = await self._run(command)
                self.assertEqual(result.exit_code, 0, result.output)
        # `alias.push` is inert too: git runs the builtin `push` with no
        # arguments at all, so the command carries no force flag and the guard
        # allows it (git then does a plain implicit push).
        result = await self._run("git -c alias.push='push -f origin main' push")
        self.assertEqual(result.exit_code, 0, result.output)

    async def test_self_referencing_alias_is_not_reported_as_unresolvable(self):
        repo, _bare = self._make_repo("repo-alias-self")
        os.chdir(repo)
        # `alias.a=a` never expands (git refuses the loop), so the guard must
        # report the unknown subcommand, not an unresolvable alias.
        message = await self._refused("git -c alias.a=a a")
        self.assertIn(
            "outside the git command set this guard was calibrated against",
            message,
        )
        self.assertNotIn("defines a git alias", message)

class ForcePushGitCommandNameTest(unittest.TestCase):
    """A git subcommand git does not resolve itself is refused.

    The guard cannot see what an alias or an external `git-<name>` program
    runs, so a name outside git's own command table is refused even when it
    looks harmless. Calling the guard directly keeps these vectors spawn-free.
    """

    def setUp(self):
        frozen = mock.patch.object(
            bash_module, "_FORCE_PUSH_BYPASS_AT_KERNEL_START", False
        )
        frozen.start()
        self.addCleanup(frozen.stop)
        prefix = os.environ.pop("PRIME_AGENT_BASH_COMMAND_PREFIX", None)
        if prefix is not None:
            self.addCleanup(os.environ.__setitem__, "PRIME_AGENT_BASH_COMMAND_PREFIX", prefix)

    def _refusal(self, command: str) -> str | None:
        try:
            bash_module._guard_force_push(command, False)
        except ForcePushRefusalError as refusal:
            return str(refusal)
        return None

    def test_git_own_commands_run(self):
        for command in [
            "git submodule status",
            "git subtree --help",
            "git send-email --help",
            "git daemon --help",
            "git request-pull origin main",
            "git filter-branch --help",
            "git mergetool --help",
            "git merge-octopus --help",
            "git p4 --help",
            "git status",
            "git log --oneline -1",
            "git push --dry-run -f origin main",
        ]:
            with self.subTest(command=command):
                self.assertIsNone(self._refusal(command), command)

    def test_commands_newer_git_knows_are_refused(self):
        # history, repo, url-parse, format-rev, last-modified, and instaweb are
        # commands only in newer git than the baseline this set is calibrated
        # to (Apple git 2.50.1), so the guard refuses them by design rather than
        # trusting a name the running git might not have.
        for command in [
            "git history",
            "git repo",
            "git url-parse",
            "git format-rev",
            "git last-modified",
            "git instaweb",
            "git cvsserver --help",
        ]:
            with self.subTest(command=command):
                message = self._refusal(command)
                self.assertIsNotNone(message)
                self.assertIn(
            "outside the git command set this guard was calibrated against",
            message,
        )

    def test_names_git_does_not_own_are_refused(self):
        # `lfs` is an external `git-lfs` program with no external program on
        # PATH in every environment: `alias.lfs` hijacks it where git-lfs is
        # absent, so it stays refused.
        for command in ["git lfs version", "git p", "git co", "git st"]:
            with self.subTest(command=command):
                message = self._refusal(command)
                self.assertIsNotNone(message)
                self.assertIn(
            "outside the git command set this guard was calibrated against",
            message,
        )

    def test_refusal_points_at_the_real_subcommand_first(self):
        message = self._refusal("git lfs version")
        self.assertIsNotNone(message)
        self.assertLess(
            message.index("Spell out the real subcommand"),
            message.index("allow_force_push=True"),
        )


class ForcePushFrozenBypassTest(unittest.TestCase):
    """The bypass env var is frozen at kernel start, in a fresh kernel."""

    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.workspace = Path(temp.name) / "repo"
        self.workspace.mkdir()
        bare = Path(temp.name) / "remote.git"
        self._git("init", "-q", "--bare", "-b", "main", str(bare), cwd=Path(temp.name))
        self._git("init", "-q", "-b", "main", cwd=self.workspace)
        self._git("config", "user.email", "guard@example.com", cwd=self.workspace)
        self._git("config", "user.name", "Guard Test", cwd=self.workspace)
        self._git("config", "commit.gpgsign", "false", cwd=self.workspace)
        self._git("config", "tag.gpgsign", "false", cwd=self.workspace)
        (self.workspace / "file.txt").write_text("local\n")
        self._git("add", ".", cwd=self.workspace)
        self._git("commit", "-q", "-m", "init", cwd=self.workspace)
        self._git("remote", "add", "origin", str(bare), cwd=self.workspace)
        self._git("push", "-q", "-u", "origin", "main", cwd=self.workspace)
        # Diverge main so a force-push is a real rewrite.
        clone = Path(temp.name) / "clone"
        self._git("clone", "-q", str(bare), str(clone), cwd=Path(temp.name))
        self._git("config", "user.email", "guard@example.com", cwd=clone)
        self._git("config", "user.name", "Guard Test", cwd=clone)
        self._git("config", "commit.gpgsign", "false", cwd=clone)
        self._git("config", "tag.gpgsign", "false", cwd=clone)
        (clone / "file.txt").write_text("remote\n")
        self._git("commit", "-q", "-am", "remote change", cwd=clone)
        self._git("push", "-q", "origin", "main", cwd=clone)
        (self.workspace / "file.txt").write_text("local-diverged\n")
        self._git("commit", "-q", "-am", "local change", cwd=self.workspace)

    def _git(self, *args: str, cwd: Path) -> subprocess.CompletedProcess:
        completed = subprocess.run(
            ["git", *args],
            cwd=str(cwd),
            capture_output=True,
            text=True,
            timeout=GIT_TIMEOUT,
        )
        if completed.returncode != 0:
            raise AssertionError(f"git {' '.join(args)!r} failed: {completed.stderr}")
        return completed

    def _launch(
        self, extra_env: dict[str, str], body: str = "", command: str = "git push -f origin main"
    ) -> subprocess.CompletedProcess:
        """Start a fresh kernel (import-time env freeze) and run one push."""
        probe = (
            "import asyncio\n"
            "import os\n"
            "import sys\n"
            "from rlm import bash\n"
            f"{body}"
            "async def main():\n"
            "    result = await bash(sys.argv[1])\n"
            "    return result.exit_code\n"
            "raise SystemExit(asyncio.run(main()))\n"
        )
        env = dict(os.environ)
        env.pop(BASH_FORCE_PUSH_BYPASS_ENV, None)
        env.update(extra_env)
        return subprocess.run(
            [sys.executable, "-c", probe, command],
            cwd=str(self.workspace),
            env=env,
            capture_output=True,
            text=True,
            timeout=KERNEL_LAUNCH_TIMEOUT,
        )

    def test_launch_value_disables_the_guard_for_that_kernel(self):
        completed = self._launch({BASH_FORCE_PUSH_BYPASS_ENV: "1"})
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_falsy_launch_value_keeps_the_guard_armed(self):
        completed = self._launch({BASH_FORCE_PUSH_BYPASS_ENV: "0"})
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("Refusing to run", completed.stderr)

    def test_absent_launch_value_keeps_the_guard_armed(self):
        completed = self._launch({})
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("Refusing to run", completed.stderr)

    def test_mid_session_os_environ_write_does_not_unlock_a_fresh_kernel(self):
        body = (
            f"os.environ[{BASH_FORCE_PUSH_BYPASS_ENV!r}] = '1'\n"
        )
        completed = self._launch({}, body=body)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("Refusing to run", completed.stderr)
        self.assertIn("appeared after kernel start", completed.stderr)


if __name__ == "__main__":
    unittest.main()
