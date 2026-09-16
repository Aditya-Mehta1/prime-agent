from __future__ import annotations

import asyncio
import io
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
]


class ForcePushDetectionTest(unittest.TestCase):
    def test_matches_force_pushes(self):
        for command in FORCE_PUSH_MATCHING_COMMANDS:
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
        ]:
            with self.subTest(command=command):
                self.assertFalse(
                    bash_module._fp_shell_c_payloads_hide_force_push(command)
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
        with self.assertRaises(ForcePushRefusalError) as caught:
            bash(command)
        return str(caught.exception)

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
