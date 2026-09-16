from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from rlm import bash
from rlm.bash import (
    BASH_DESTRUCTIVE_GIT_BYPASS_ENV,
    DestructiveGitRefusalError,
    is_destructive_git_discard_command,
)

# The package re-exports the bash() function under the same name, so reach the
# module through sys.modules for internals.
bash_module = sys.modules["rlm.bash"]


def _run_git(cwd: str, *args: str) -> None:
    # HOME=cwd keeps user-level git config out of the test repositories.
    subprocess.run(
        ["git", *args],
        cwd=cwd,
        check=True,
        capture_output=True,
        env={**os.environ, "GIT_CONFIG_NOSYSTEM": "1", "HOME": cwd},
    )


def _init_dirty_git_repo(root: str) -> None:
    """Create a git repository with one committed file plus two uncommitted changes."""
    Path(root).mkdir(parents=True, exist_ok=True)
    _run_git(root, "init", "-q")
    Path(root, "tracked.txt").write_text("committed\n")
    _run_git(root, "add", "tracked.txt")
    _run_git(root, "commit", "-q", "-m", "init")
    Path(root, "tracked.txt").write_text("modified\n")
    Path(root, "untracked.txt").write_text("uncommitted\n")


# Vectors ported from packages/coding-agent/test/bash-destructive-git-guard.test.ts;
# the kernel guard must keep the same command taxonomy as the coding-agent tool.
MATCHING_COMMANDS = [
    "git checkout -- .",
    "git checkout .",
    "git checkout HEAD -- .",
    "git restore .",
    "git restore --source=HEAD~1 .",
    "git clean -f",
    "git clean -fd",
    "git clean -fdx",
    "git clean --force",
    "git reset --hard",
    "git reset --hard HEAD~1",
    "git checkout -b tmp 2>/dev/null; git checkout -- .",
    "git checkout main && git reset --hard",
    "echo start\ngit clean -fd",
    "npm test & git clean -fd &",
    "git checkout :/",
    "git checkout -- :/",
    "git checkout HEAD -- :/",
    "git restore :/",
    "git restore -s@ .",
    "git restore -s@ :/",
    "git restore --source=HEAD :/",
    "git restore -s HEAD~1 :/",
    "git restore -- .",
    "git checkout -- ./",
    "git checkout ./",
    "git restore ./",
    "git -C sub reset --hard",
    "git --git-dir=sub/.git reset --hard",
    "git reset -q --hard",
    "git reset --no-refresh --hard",
    "git -C repo -C nested reset --hard",
    "GIT_DIR=sub/.git git reset --hard",
    "GIT_DIR=sub/.git GIT_WORK_TREE=sub git reset --hard",
    "git checkout -f -- .",
    "git checkout --theirs -- .",
    "git checkout -m .",
    "git checkout --conflict=diff3 .",
    "git checkout HEAD .",
    "git checkout HEAD~1 -- .",
    "git checkout origin/main .",
    "git checkout -f main",
    "git checkout --force main",
    "git clean -f -- -n",
]

NON_MATCHING_COMMANDS = [
    "git status",
    "git log --oneline",
    "git checkout -b new-branch",
    "git checkout main",
    "git checkout -m main",
    "git checkout -b newbranch .",
    "git checkout -- single-file.txt",
    "echo 'git reset --hard'",
    'git commit -m "git reset --hard"',
    'echo "git clean -fd"',
    "echo preparing # git reset --hard",
    "git checkout ./nested",
    "git restore --staged .",
    "git restore --staged :/",
    "git restore single-file.txt",
    "git clean -n",
    "git clean -n -f .",
    "git clean --dry-run",
    "git clean -d",
    "git reset",
    "git reset --soft HEAD~1",
    "git stash",
    "git add .",
    "echo hello world",
    "npm run check",
]


class DestructiveGitDetectionTest(unittest.TestCase):
    def test_matches_destructive_discards(self):
        for command in MATCHING_COMMANDS:
            with self.subTest(command=command):
                self.assertTrue(is_destructive_git_discard_command(command))

    def test_does_not_match_other_commands(self):
        for command in NON_MATCHING_COMMANDS:
            with self.subTest(command=command):
                self.assertFalse(is_destructive_git_discard_command(command))


class DestructiveGitGuardTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self._prev_cwd = os.getcwd()
        os.environ.pop(BASH_DESTRUCTIVE_GIT_BYPASS_ENV, None)
        os.environ.pop("PRIME_AGENT_BASH_COMMAND_PREFIX", None)
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        # Restore cwd before the temp dir disappears (cleanups run LIFO).
        self.addCleanup(os.chdir, self._prev_cwd)
        self.test_dir = temp.name

    def _init_dirty_repo(self) -> None:
        _init_dirty_git_repo(self.test_dir)
        os.chdir(self.test_dir)

    def _tracked(self, *parts: str) -> Path:
        return Path(self.test_dir, *parts)

    async def test_refuses_destructive_discards_on_dirty_tree(self):
        for index, command in enumerate([
            "git checkout -- .",
            "git checkout .",
            "git clean -fd",
            "git reset --hard",
            "git restore .",
        ]):
            with self.subTest(command=command):
                repo = str(self._tracked(f"repo-{index}"))
                _init_dirty_git_repo(repo)
                os.chdir(repo)
                with self.assertRaises(DestructiveGitRefusalError) as caught:
                    bash(command)
                self.assertIn("Refusing to run this destructive git command", str(caught.exception))
                self.assertEqual(Path(repo, "tracked.txt").read_text(), "modified\n")
                self.assertTrue(Path(repo, "untracked.txt").exists())

    async def test_refusal_lists_dirty_paths_and_both_bypasses(self):
        self._init_dirty_repo()
        with self.assertRaises(DestructiveGitRefusalError) as caught:
            bash("git checkout -- .")
        message = str(caught.exception)
        self.assertIn("2 uncommitted change(s)", message)
        self.assertIn("tracked.txt", message)
        self.assertIn("untracked.txt", message)
        self.assertIn("Commit, stash, or stage your work first.", message)
        self.assertIn("allow_destructive_git=True", message)
        self.assertIn(BASH_DESTRUCTIVE_GIT_BYPASS_ENV, message)

    async def test_elides_long_dirty_path_lists(self):
        self._init_dirty_repo()
        for i in range(12):
            self._tracked(f"extra-{i}.txt").write_text("x\n")
        with self.assertRaises(DestructiveGitRefusalError) as caught:
            bash("git checkout -- .")
        self.assertIn("... and 4 more", str(caught.exception))

    async def test_runs_discard_when_tree_is_clean(self):
        self._init_dirty_repo()
        _run_git(self.test_dir, "add", "-A")
        _run_git(self.test_dir, "commit", "-q", "-m", "second")
        result = await bash("git checkout -- .")
        self.assertEqual(result.exit_code, 0)

    async def test_bypass_kwarg_runs_discard(self):
        self._init_dirty_repo()
        result = await bash("git reset --hard", allow_destructive_git=True)
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(self._tracked("tracked.txt").read_text(), "committed\n")

    async def test_bypass_env_var_runs_discard(self):
        self._init_dirty_repo()
        with mock.patch.dict(os.environ, {BASH_DESTRUCTIVE_GIT_BYPASS_ENV: "1"}):
            result = await bash("git reset --hard")
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(self._tracked("tracked.txt").read_text(), "committed\n")

    async def test_bypass_env_var_zero_still_refuses(self):
        self._init_dirty_repo()
        with mock.patch.dict(os.environ, {BASH_DESTRUCTIVE_GIT_BYPASS_ENV: "0"}):
            with self.assertRaises(DestructiveGitRefusalError):
                bash("git reset --hard")
        self.assertEqual(self._tracked("tracked.txt").read_text(), "modified\n")

    async def test_fails_open_outside_a_git_repository(self):
        os.chdir(self.test_dir)
        result = await bash("git checkout -- .")
        self.assertNotEqual(result.exit_code, 0)

    async def test_non_discard_commands_are_untouched_on_a_dirty_tree(self):
        self._init_dirty_repo()
        result = await bash("git status")
        self.assertEqual(result.exit_code, 0)
        self.assertIn("tracked.txt", result.output)
        result = await bash("git log --oneline")
        self.assertEqual(result.exit_code, 0)
        # Quoted data must not trigger the guard end to end either.
        result = await bash("echo 'git reset --hard'")
        self.assertEqual(result.exit_code, 0)
        self.assertIn("git reset --hard", result.output)
        self.assertEqual(self._tracked("tracked.txt").read_text(), "modified\n")

    async def test_probe_runs_only_for_discard_commands(self):
        self._init_dirty_repo()
        probe = mock.Mock(return_value=[" M tracked.txt"])
        with mock.patch.object(bash_module, "_probe_uncommitted_changes", probe):
            result = await bash("echo hi")
            self.assertEqual(result.exit_code, 0)
            result = await bash("git status")
            self.assertEqual(result.exit_code, 0)
            probe.assert_not_called()
            with self.assertRaises(DestructiveGitRefusalError):
                bash("git checkout -- .")
        probe.assert_called_once_with(
            "git status --porcelain --untracked-files=all",
            os.path.realpath(self.test_dir),
        )

    async def test_fails_open_when_the_probe_fails(self):
        self._init_dirty_repo()
        with mock.patch.object(bash_module, "_probe_uncommitted_changes", return_value=None):
            result = await bash("git checkout -- .")
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(self._tracked("tracked.txt").read_text(), "committed\n")

    async def test_refuses_cd_relocation_into_dirty_nested_repository(self):
        _init_dirty_git_repo(str(self._tracked("sub")))
        self._init_dirty_repo()
        with self.assertRaises(DestructiveGitRefusalError) as caught:
            bash("cd sub && git reset --hard")
        self.assertIn("Refusing to run this destructive git command", str(caught.exception))
        self.assertIn("tracked.txt", str(caught.exception))
        self.assertEqual(self._tracked("sub", "tracked.txt").read_text(), "modified\n")

    async def test_refuses_git_c_relocation_into_dirty_nested_repository(self):
        _init_dirty_git_repo(str(self._tracked("sub")))
        self._init_dirty_repo()
        with self.assertRaises(DestructiveGitRefusalError) as caught:
            bash("git -C sub reset --hard")
        self.assertIn("tracked.txt", str(caught.exception))
        self.assertEqual(self._tracked("sub", "tracked.txt").read_text(), "modified\n")

    async def test_allows_relocated_discard_when_target_is_clean(self):
        _init_dirty_git_repo(str(self._tracked("sub")))
        _run_git(str(self._tracked("sub")), "add", "-A")
        _run_git(str(self._tracked("sub")), "commit", "-q", "-m", "second")
        self._init_dirty_repo()
        result = await bash("cd sub && git reset --hard")
        self.assertEqual(result.exit_code, 0)

    async def test_multi_discard_probes_every_target_repository(self):
        _init_dirty_git_repo(str(self._tracked("sub")))
        self._init_dirty_repo()
        with self.assertRaises(DestructiveGitRefusalError):
            bash("git checkout -- . && cd sub && git reset --hard")

    async def test_refuses_relocations_it_cannot_replay_safely(self):
        self._init_dirty_repo()
        for command in [
            "cd $(pwd)/sub && git reset --hard",
            "git --git-dir=sub/.git reset --hard",
            "cd sub || git reset --hard",
            "pushd sub && git reset --hard",
            'git -C "sub" reset --hard',
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveGitRefusalError) as caught:
                    bash(command)
                self.assertIn("changes directory (or repository) first", str(caught.exception))

    async def test_clean_fx_lists_ignored_files_it_would_delete(self):
        self._init_dirty_repo()
        _run_git(self.test_dir, "add", "-A")
        _run_git(self.test_dir, "commit", "-q", "-m", "second")
        self._tracked(".gitignore").write_text("ignored.txt\n")
        _run_git(self.test_dir, "add", ".gitignore")
        _run_git(self.test_dir, "commit", "-q", "-m", "gitignore")
        self._tracked("ignored.txt").write_text("generated\n")
        with self.assertRaises(DestructiveGitRefusalError) as caught:
            bash("git clean -fx")
        message = str(caught.exception)
        self.assertIn("uncommitted or ignored file(s)", message)
        self.assertIn("ignored.txt", message)
        self.assertTrue(self._tracked("ignored.txt").exists())

    async def test_allows_clean_f_when_only_ignored_files_exist(self):
        self._init_dirty_repo()
        _run_git(self.test_dir, "add", "-A")
        _run_git(self.test_dir, "commit", "-q", "-m", "second")
        self._tracked(".gitignore").write_text("ignored.txt\n")
        _run_git(self.test_dir, "add", ".gitignore")
        _run_git(self.test_dir, "commit", "-q", "-m", "gitignore")
        self._tracked("ignored.txt").write_text("generated\n")
        result = await bash("git clean -f")
        self.assertEqual(result.exit_code, 0)
        self.assertTrue(self._tracked("ignored.txt").exists())

    async def test_detects_untracked_files_despite_status_showuntrackedfiles_no(self):
        self._init_dirty_repo()
        _run_git(self.test_dir, "add", "-A")
        _run_git(self.test_dir, "commit", "-q", "-m", "second")
        self._tracked("fresh-untracked.txt").write_text("new\n")
        _run_git(self.test_dir, "config", "status.showUntrackedFiles", "no")
        with self.assertRaises(DestructiveGitRefusalError) as caught:
            bash("git clean -fd")
        self.assertIn("fresh-untracked.txt", str(caught.exception))
        self.assertTrue(self._tracked("fresh-untracked.txt").exists())

    async def test_command_prefix_is_replayed_in_the_probe(self):
        self._init_dirty_repo()
        probe = mock.Mock(return_value=[" M tracked.txt"])
        with (
            mock.patch.dict(
                os.environ, {"PRIME_AGENT_BASH_COMMAND_PREFIX": "export GUARD_TEST_VAR=1"}
            ),
            mock.patch.object(bash_module, "_probe_uncommitted_changes", probe),
        ):
            with self.assertRaises(DestructiveGitRefusalError):
                bash("git checkout -- .")
        probe.assert_called_once_with(
            "export GUARD_TEST_VAR=1\ngit status --porcelain --untracked-files=all",
            os.path.realpath(self.test_dir),
        )

    async def test_discard_inside_command_prefix_is_refused(self):
        os.chdir(self.test_dir)
        probe = mock.Mock()
        with (
            mock.patch.dict(
                os.environ, {"PRIME_AGENT_BASH_COMMAND_PREFIX": "git checkout -- ."}
            ),
            mock.patch.object(bash_module, "_probe_uncommitted_changes", probe),
        ):
            with self.assertRaises(DestructiveGitRefusalError) as caught:
                bash("git status")
        self.assertIn("changes directory (or repository) first", str(caught.exception))
        probe.assert_not_called()


if __name__ == "__main__":
    unittest.main()
