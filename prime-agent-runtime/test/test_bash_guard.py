from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from rlm import bash
from rlm.bash import (
    BASH_DESTRUCTIVE_GIT_BYPASS_ENV,
    BASH_DESTRUCTIVE_RM_BYPASS_ENV,
    DestructiveGitRefusalError,
    DestructiveRmRefusalError,
    is_destructive_git_discard_command,
    is_recursive_force_rm_command,
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
    # The isolated HOME hides any global identity, so configure one per repo
    # exactly like the coding-agent guard test does.
    _run_git(root, "config", "user.email", "test@example.com")
    _run_git(root, "config", "user.name", "Test")
    _run_git(root, "config", "commit.gpgsign", "false")
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
    "git reset 2>/dev/null --hard",
    "git reset 2> /dev/null --hard",
    "git reset 2>&1 --hard",
    "git 2>/dev/null reset --hard",
    "git restore 2>/dev/null .",
    "git clean -f 2>/dev/null",
    "git checkout 2>/dev/null -- .",
    "git restore --quiet .",
    "git restore -q .",
    "git restore --quiet --source=HEAD .",
    "g\\it reset --ha\\rd",
    "git res\\et --hard",
    "git reset &>/dev/null --hard",
    "git reset &> /dev/null --hard",
    "git reset &>>/dev/null --hard",
    "git reset >&/dev/null --hard",
    "{ cd sub && git reset --hard; }",
    "export GIT_DIR=sub/.git GIT_WORK_TREE=sub; git reset --hard",
    "GIT_DIR=sub/.git; git reset --hard",
    "git -Csub reset --hard",
    "git -cfoo.bar=1 reset --hard",
    "git reset \
--hard",
    "git checkout -- \
.",
    "git clean -f \
-d",
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
    "git status > status.txt",
    "git log --oneline > log.txt 2>/dev/null",
    "echo 2>/dev/null hi",
    "{ echo hi; }",
    "export FOO=1",
    "git restore --staged --quiet .",
    "echo \\# git reset --hard",
    "echo one \
 two",
    "git -Csub status",
    "# git reset --hard",
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


class EvalPayloadDetectionTest(unittest.TestCase):
    def test_eval_payloads_hiding_discards(self):
        for command in [
            "eval 'git reset --hard'",
            'eval "git clean -f"',
            "eval 'cd sub && git reset --hard'",
            "eval 'git checkout -- .'",
            'eval "git restore ."',
            "eval 'eval \"git reset --hard\"'",
            "GIT_DIR=sub/.git eval 'git reset --hard'",
            "eval 'git reset \\\n--hard'",
        ]:
            with self.subTest(command=command):
                self.assertTrue(bash_module._eval_payloads_hide_destructive_git(command))

    def test_safe_eval_payloads_stay_unflagged(self):
        for command in [
            "eval",
            "eval 'echo hi'",
            "eval 'git status'",
            "eval 'echo \"git reset --hard\"'",
            "eval \"echo 'git reset --hard'\"",
            "echo 'eval git reset --hard'",
            "npm run eval:suite",
        ]:
            with self.subTest(command=command):
                self.assertFalse(bash_module._eval_payloads_hide_destructive_git(command))


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

    async def test_refuses_eval_wrapped_discards(self):
        self._init_dirty_repo()
        for command in [
            "eval 'git reset --hard'",
            'eval "git clean -f"',
            "eval 'eval \"git reset --hard\"'",
            "eval 'cd sub && git reset --hard'",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveGitRefusalError) as caught:
                    bash(command)
                self.assertIn("wraps a git discard in eval", str(caught.exception))
                self.assertEqual(self._tracked("tracked.txt").read_text(), "modified\n")
                self.assertTrue(self._tracked("untracked.txt").exists())

    async def test_eval_refusal_honors_the_bypass_kwarg(self):
        self._init_dirty_repo()
        result = await bash("eval 'git reset --hard'", allow_destructive_git=True)
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(self._tracked("tracked.txt").read_text(), "committed\n")

    async def test_safe_eval_commands_still_run(self):
        self._init_dirty_repo()
        result = await bash("eval 'echo hi'")
        self.assertEqual(result.exit_code, 0)
        self.assertIn("hi", result.output)
        # Unquoting one level at a time must not mistake still-quoted data for
        # a payload command: this eval only prints the string.
        result = await bash("eval \"echo 'git reset --hard'\"")
        self.assertEqual(result.exit_code, 0)
        self.assertIn("git reset --hard", result.output)
        self.assertEqual(self._tracked("tracked.txt").read_text(), "modified\n")

    async def test_quoted_cd_relocations_are_replayed_in_the_probe(self):
        _init_dirty_git_repo(str(self._tracked("my repo")))
        self._init_dirty_repo()
        with self.assertRaises(DestructiveGitRefusalError) as caught:
            bash('cd "my repo" && git reset --hard')
        self.assertIn("tracked.txt", str(caught.exception))
        self.assertEqual(self._tracked("my repo", "tracked.txt").read_text(), "modified\n")

    async def test_allows_quoted_cd_discard_when_target_is_clean(self):
        _init_dirty_git_repo(str(self._tracked("my repo")))
        _run_git(str(self._tracked("my repo")), "add", "-A")
        _run_git(str(self._tracked("my repo")), "commit", "-q", "-m", "second")
        self._init_dirty_repo()
        result = await bash('cd "my repo" && git reset --hard')
        self.assertEqual(result.exit_code, 0)

    async def test_attached_dash_c_values_relocate_the_probe(self):
        _init_dirty_git_repo(str(self._tracked("sub")))
        # The parent tree stays clean: the probe must follow the attached
        # value, not probe the current directory.
        self._init_dirty_repo()
        _run_git(self.test_dir, "add", "-A")
        _run_git(self.test_dir, "commit", "-q", "-m", "second")
        with self.assertRaises(DestructiveGitRefusalError) as caught:
            bash("git -Csub reset --hard")
        self.assertIn("tracked.txt", str(caught.exception))
        self.assertEqual(self._tracked("sub", "tracked.txt").read_text(), "modified\n")

    async def test_attached_dash_c_probe_follows_the_value_not_the_parent_tree(self):
        # Stock git rejects attached short options itself ("unknown option:
        # -Csub"), so the form can never discard anything; the guard still
        # resolves the attached value the way the thread asks instead of
        # silently probing the parent tree.
        _init_dirty_git_repo(str(self._tracked("sub")))
        _run_git(str(self._tracked("sub")), "add", "-A")
        _run_git(str(self._tracked("sub")), "commit", "-q", "-m", "second")
        self._init_dirty_repo()  # the parent tree stays dirty
        result = await bash("git -Csub reset --hard")
        self.assertNotEqual(result.exit_code, 0)
        self.assertEqual(self._tracked("tracked.txt").read_text(), "modified\n")

    async def test_refuses_attached_and_bundled_relocations_it_cannot_replay(self):
        self._init_dirty_repo()
        for command in [
            "git -ccore.worktree=sub reset --hard",
            "git -ccore.bare=1 reset --hard",
            "git -pCsub reset --hard",
            "git -qC sub reset --hard",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveGitRefusalError) as caught:
                    bash(command)
                self.assertIn("changes directory (or repository) first", str(caught.exception))

    async def test_attached_benign_dash_c_configs_do_not_relocate(self):
        self._init_dirty_repo()
        with self.assertRaises(DestructiveGitRefusalError) as caught:
            bash("git -cfoo.bar=1 reset --hard")
        self.assertIn("uncommitted change(s)", str(caught.exception))
        self.assertNotIn("changes directory (or repository) first", str(caught.exception))

    async def test_refuses_discards_split_over_line_continuations(self):
        self._init_dirty_repo()
        for command in [
            "git reset \\\n--hard",
            "git checkout -- \\\n.",
            "git clean -f \\\n-d",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveGitRefusalError):
                    bash(command)
                self.assertEqual(self._tracked("tracked.txt").read_text(), "modified\n")
                self.assertTrue(self._tracked("untracked.txt").exists())

    async def test_line_continuations_in_safe_commands_still_run(self):
        self._init_dirty_repo()
        result = await bash("echo one \\\n two")
        self.assertEqual(result.exit_code, 0)
        self.assertIn("one", result.output)
        self.assertIn("two", result.output)

    async def test_comment_newline_still_ends_the_line_before_a_discard(self):
        # A backslash-newline inside a comment does not join lines: the
        # newline ends the comment and the next line runs for real.
        self._init_dirty_repo()
        with self.assertRaises(DestructiveGitRefusalError):
            bash("# safe \\\ngit reset --hard")
        self.assertEqual(self._tracked("tracked.txt").read_text(), "modified\n")

    async def test_refuses_discards_with_shell_redirections(self):
        self._init_dirty_repo()
        for command in [
            "git reset 2>/dev/null --hard",
            "git reset 2> /dev/null --hard",
            "git reset 2>&1 --hard",
            "git 2>/dev/null reset --hard",
            "git restore 2>/dev/null .",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveGitRefusalError):
                    bash(command)
                self.assertEqual(self._tracked("tracked.txt").read_text(), "modified\n")
                self.assertTrue(self._tracked("untracked.txt").exists())

    async def test_safe_redirection_commands_still_run(self):
        self._init_dirty_repo()
        result = await bash("git status > status.txt")
        self.assertEqual(result.exit_code, 0)
        result = await bash("git log --oneline > log.txt 2>/dev/null")
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(self._tracked("tracked.txt").read_text(), "modified\n")

    async def test_redirect_targets_with_substitutions_stay_scannable(self):
        # A command substitution as redirect target executes: its content
        # must stay visible to the scan, and this one discards.
        self._init_dirty_repo()
        with self.assertRaises(DestructiveGitRefusalError):
            bash("echo 2> $(git reset --hard)")
        self.assertEqual(self._tracked("tracked.txt").read_text(), "modified\n")

    async def test_redirections_in_cd_chains_do_not_block_the_probe(self):
        _init_dirty_git_repo(str(self._tracked("sub")))
        self._init_dirty_repo()
        with self.assertRaises(DestructiveGitRefusalError) as caught:
            bash("cd sub 2>/dev/null && git reset --hard")
        self.assertIn("tracked.txt", str(caught.exception))
        self.assertEqual(self._tracked("sub", "tracked.txt").read_text(), "modified\n")

    async def test_eval_payloads_with_redirections_are_refused(self):
        self._init_dirty_repo()
        with self.assertRaises(DestructiveGitRefusalError):
            bash("eval 'git reset 2>/dev/null --hard'")
        self.assertEqual(self._tracked("tracked.txt").read_text(), "modified\n")

    async def test_refuses_brace_group_cd_relocations(self):
        _init_dirty_git_repo(str(self._tracked("sub")))
        # The parent tree stays clean: the brace group's cd must relocate
        # the probe like a bare cd chain.
        self._init_dirty_repo()
        _run_git(self.test_dir, "add", "-A")
        _run_git(self.test_dir, "commit", "-q", "-m", "second")
        with self.assertRaises(DestructiveGitRefusalError) as caught:
            bash("{ cd sub && git reset --hard; }")
        self.assertIn("tracked.txt", str(caught.exception))
        self.assertEqual(self._tracked("sub", "tracked.txt").read_text(), "modified\n")
        # A cd followed by `;` inside the group depends on the cd
        # succeeding; the guard refuses it instead of probing one outcome.
        with self.assertRaises(DestructiveGitRefusalError) as caught:
            bash("{ cd sub; git reset --hard; }")
        self.assertIn("changes directory (or repository) first", str(caught.exception))

    async def test_refuses_persistent_env_assignment_relocations(self):
        _init_dirty_git_repo(str(self._tracked("sub")))
        self._init_dirty_repo()
        _run_git(self.test_dir, "add", "-A")
        _run_git(self.test_dir, "commit", "-q", "-m", "second")
        for command in [
            "export GIT_DIR=sub/.git GIT_WORK_TREE=sub; git reset --hard",
            "GIT_DIR=sub/.git; git reset --hard",
            "export GIT_DIR=sub/.git && git reset --hard",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveGitRefusalError) as caught:
                    bash(command)
                self.assertIn("tracked.txt", str(caught.exception))
                self.assertEqual(self._tracked("sub", "tracked.txt").read_text(), "modified\n")

    async def test_refuses_env_assignments_it_cannot_replay(self):
        self._init_dirty_repo()
        with self.assertRaises(DestructiveGitRefusalError) as caught:
            bash("export GIT_DIR=$(pwd)/sub; git reset --hard")
        self.assertIn("changes directory (or repository) first", str(caught.exception))
        # Command-scoped assignments in a mixed segment do not persist.
        result = await bash("FOO=1 git status")
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(self._tracked("tracked.txt").read_text(), "modified\n")

    async def test_refuses_quiet_restore_discards(self):
        self._init_dirty_repo()
        for command in [
            "git restore --quiet .",
            "git restore -q .",
            "git restore --quiet --source=HEAD .",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveGitRefusalError):
                    bash(command)
                self.assertEqual(self._tracked("tracked.txt").read_text(), "modified\n")
                self.assertTrue(self._tracked("untracked.txt").exists())

    async def test_refuses_discards_hidden_behind_shell_escapes(self):
        self._init_dirty_repo()
        for command in [
            "g\\it reset --ha\\rd",
            "git res\\et --hard",
            "git reset --ha\\rd",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveGitRefusalError):
                    bash(command)
                self.assertEqual(self._tracked("tracked.txt").read_text(), "modified\n")
        # Escaped data stays inert: this only prints.
        result = await bash("echo \\# git reset --hard")
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(self._tracked("tracked.txt").read_text(), "modified\n")

    async def test_refuses_discards_with_ampersand_redirects(self):
        self._init_dirty_repo()
        for command in [
            "git reset &>/dev/null --hard",
            "git reset &> /dev/null --hard",
            "git reset &>>/dev/null --hard",
            "git reset >&/dev/null --hard",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveGitRefusalError):
                    bash(command)
                self.assertEqual(self._tracked("tracked.txt").read_text(), "modified\n")

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


# Vectors for the recursive-force rm guard: an rm invocation must combine a
# recursive flag (-r/-R/--recursive) with a force flag (-f) in any position;
# a lone -r or lone -f must stay untouched.
RM_MATCHING_COMMANDS = [
    "rm -rf sub",
    "rm -fr sub",
    "rm -Rf sub",
    "rm -fR sub",
    "rm -r -f sub",
    "rm -f -r sub",
    "rm -rfv sub",
    "rm -I -rf sub",
    "rm --recursive --force sub",
    "rm --force --recursive sub",
    "rm --recursive -f sub",
    "rm sub -rf",
    "rm -rf sub && rm -fr other",
    "rm -rf sub; git status",
    "/bin/rm -rf sub",
    '"rm" -rf sub',
    "\\rm -rf sub",
    "sudo rm -rf sub",
    "FOO=1 rm -rf sub",
    "xargs rm -rf",
    "rm -rf",
    "rm -rf sub 2>/dev/null",
    "rm -rf &>/dev/null sub",
    "rm 2>/dev/null -rf sub",
    "rm -rf sub # cleanup",
    "rm -rf \\\nsub",
    "rm -rf -- sub",
    "(rm -rf sub)",
    "{ rm -rf sub; }",
    "echo $(rm -rf sub)",
    "echo `rm -rf sub`",
    "rm -rf ~",
    "rm -rf $HOME",
    "rm -rf ..",
    "rm -rf /",
    "rm --recurs --forc sub",
    "rm --recursive --f sub",
    'echo $(echo ")"; rm -rf /)',
    "echo \"$(echo ')'; rm -rf sub)\"",
]

RM_NON_MATCHING_COMMANDS = [
    "rm -r sub",
    "rm -f file.txt",
    "rm -R sub",
    "rm -d -f sub",
    "rm -F sub",
    "rm --recursive sub",
    "rm --force sub",
    "rm -r sub && rm -f other",
    "rm -i sub",
    "rm sub",
    "rm -- sub",
    "echo 'rm -rf sub'",
    'echo "rm -rf ~"',
    "# rm -rf sub",
    "eval 'rm -rf sub'",
    "git rm -r --cached .",
    "npm run rm-build",
    "permanent -rf sub",
    "perm -rf sub",
    "git status",
    "echo hello world",
    "sh -c 'rm -r sub'",
    "sh -c 'echo rm -rf sub'",
    "python3 -c 'print(\"rm -rf ~\")'",
]


class RecursiveForceRmDetectionTest(unittest.TestCase):
    def test_matches_recursive_force_rm(self):
        for command in RM_MATCHING_COMMANDS:
            with self.subTest(command=command):
                self.assertTrue(is_recursive_force_rm_command(command))

    def test_does_not_match_other_commands(self):
        for command in RM_NON_MATCHING_COMMANDS:
            with self.subTest(command=command):
                self.assertFalse(is_recursive_force_rm_command(command))


class RmEvalPayloadDetectionTest(unittest.TestCase):
    def test_eval_payloads_hiding_recursive_force_rm(self):
        for command in [
            "eval 'rm -rf ~'",
            'eval "rm -rf ~"',
            "eval 'rm -rf ~' && echo done",
            "eval 'eval \"rm -rf ~\"'",
            "eval 'rm -rf \\\n~'",
        ]:
            with self.subTest(command=command):
                self.assertTrue(
                    bash_module._wrapped_payloads_hide_recursive_force_rm(command)
                )

    def test_safe_eval_payloads_stay_unflagged(self):
        for command in [
            "eval",
            "eval 'echo hi'",
            'eval "echo \'rm -rf ~\'"',
            "npm run eval:suite",
            "echo 'eval rm -rf ~'",
        ]:
            with self.subTest(command=command):
                self.assertFalse(
                    bash_module._wrapped_payloads_hide_recursive_force_rm(command)
                )


class RmWrapperPayloadDetectionTest(unittest.TestCase):
    def test_shell_dash_c_payloads_hiding_recursive_force_rm(self):
        for command in [
            "sh -c 'rm -rf ~'",
            'bash -c "rm -rf ~"',
            "zsh -c 'rm -rf $HOME'",
            "/bin/sh -c 'rm -rf sub'",
            "dash -c 'rm -rf sub'",
            "sh -c 'cd / && rm -rf x'",
            'sh -c "sh -c \'rm -rf sub\'"',
            "bash -c 'eval \"rm -rf sub\"'",
            "eval 'sh -c \"rm -rf sub\"'",
            "sudo bash -c 'rm -rf sub'",
            "busybox sh -c 'rm -rf sub'",
            'sh -c \'rm -rf "$1"\' sh ~',
            '$BASH -c "rm -rf sub"',
        ]:
            with self.subTest(command=command):
                self.assertTrue(
                    bash_module._wrapped_payloads_hide_recursive_force_rm(command)
                )

    def test_safe_dash_c_payloads_stay_unflagged(self):
        for command in [
            "sh -c 'echo hi'",
            "bash -c 'ls -la'",
            "sh -c 'rm -r sub'",
            "sh -c 'rm -f sub'",
            "sh -c",
            "python3 -c 'print(\"rm -rf ~\")'",
            "echo 'sh -c \"rm -rf ~\"'",
        ]:
            with self.subTest(command=command):
                self.assertFalse(
                    bash_module._wrapped_payloads_hide_recursive_force_rm(command)
                )


class TrackedCwdDetectionTest(unittest.TestCase):
    def test_cd_tracking_scopes_subshells(self):
        command = "(cd /tmp; rm -rf a) && rm -rf b"
        words = bash_module._scan_shell_words(command)
        tracked = bash_module._tracked_cwd_at_words(command, words, "/ws")
        rm_indices = [i for i, w in enumerate(words) if w.value == "rm"]
        self.assertEqual(tracked[rm_indices[0]], os.path.realpath("/tmp"))
        self.assertEqual(tracked[rm_indices[1]], os.path.realpath("/ws"))

    def test_unresolvable_cd_targets_fail_closed(self):
        command = "cd $UNSET; rm -rf sub"
        words = bash_module._scan_shell_words(command)
        tracked = bash_module._tracked_cwd_at_words(command, words, "/ws")
        rm_index = next(i for i, w in enumerate(words) if w.value == "rm")
        self.assertIsNone(tracked[rm_index])

    def test_bare_cd_goes_home(self):
        command = "cd; rm -rf sub"
        words = bash_module._scan_shell_words(command)
        tracked = bash_module._tracked_cwd_at_words(command, words, "/ws")
        rm_index = next(i for i, w in enumerate(words) if w.value == "rm")
        home = os.environ.get("HOME")
        self.assertEqual(tracked[rm_index], os.path.realpath(home) if home else None)


class RmExpansionDetectionTest(unittest.TestCase):
    def test_unresolvable_expansion_reasons(self):
        for command, expected in [
            ("R=rm; $R -rf x", "$R"),
            ("flags=-rf; rm $flags /", "$flags"),
            ("rm {--recursive,--force} sub", "--recursive,--force"),
        ]:
            with self.subTest(command=command):
                words = bash_module._scan_shell_words(command)
                reasons = bash_module._unresolvable_expansion_rm_reasons(words)
                self.assertTrue(any(expected in reason for reason in reasons), reasons)

    def test_resolvable_expansion_tokens_stay_unflagged(self):
        for command in [
            "rm -rf $PWD/sub",
            "rm -rf $HOME/sub",
            "rm -rf sub",
        ]:
            with self.subTest(command=command):
                words = bash_module._scan_shell_words(command)
                self.assertEqual(
                    bash_module._unresolvable_expansion_rm_reasons(words), []
                )


class RecursiveForceRmGuardTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self._prev_cwd = os.getcwd()
        os.environ.pop(BASH_DESTRUCTIVE_RM_BYPASS_ENV, None)
        os.environ.pop("PRIME_AGENT_BASH_COMMAND_PREFIX", None)
        # The launch-time bypass snapshot is a module attribute frozen at
        # import; pin it to "unset" so tests stay deterministic.
        frozen_patch = mock.patch.object(bash_module, "_BASH_RM_BYPASS_AT_KERNEL_START", None)
        frozen_patch.start()
        self.addCleanup(frozen_patch.stop)
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        # Restore cwd before the temp dir disappears (cleanups run LIFO).
        self.addCleanup(os.chdir, self._prev_cwd)
        self.test_dir = temp.name
        os.chdir(self.test_dir)

    def _make_tree(self) -> None:
        Path(self.test_dir, "sub", "nested").mkdir(parents=True, exist_ok=True)
        Path(self.test_dir, "sub", "nested", "file.txt").write_text("x\n")
        Path(self.test_dir, "my dir").mkdir(exist_ok=True)
        Path(self.test_dir, "my dir", "file.txt").write_text("x\n")

    def _outside_target(self) -> str:
        """A sibling directory outside the workspace with a file in it."""
        name = "outside-sibling-" + os.path.basename(self.test_dir)
        outside = str(Path(self.test_dir).parent / name)
        Path(outside).mkdir(exist_ok=True)
        Path(outside, "file.txt").write_text("keep\n")
        self.addCleanup(shutil.rmtree, outside, ignore_errors=True)
        return outside

    def _tracked(self, *parts: str) -> Path:
        return Path(self.test_dir, *parts)

    async def test_refuses_escapes_to_home_and_root_and_outside(self):
        home = tempfile.TemporaryDirectory()
        self.addCleanup(home.cleanup)
        Path(home.name, "keep.txt").write_text("keep\n")
        for command in [
            "rm -rf ~",
            "rm -rf ~/",
            "rm -rf $HOME",
            "rm -rf $HOME/",
            "rm -rf ~/anything",
            "rm -rf ${HOME}/anything",
            "rm -rf /",
            "rm -rf //",
            "rm -rf /tmp/pa-rm-guard-elsewhere",
        ]:
            with self.subTest(command=command):
                with mock.patch.dict(os.environ, {"HOME": home.name}):
                    with self.assertRaises(DestructiveRmRefusalError) as caught:
                        bash(command)
                self.assertIn("Refusing to run this recursive-force rm command", str(caught.exception))
                self.assertTrue(Path(home.name, "keep.txt").exists())

    async def test_refuses_parent_directory_escapes(self):
        self._make_tree()
        outside = self._outside_target()
        for command in [
            "rm -rf ..",
            f"rm -rf ../{Path(outside).name}",
            "rm -rf ./..",
            "rm -rf sub/..",
            "rm -rf sub/../sub/..",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveRmRefusalError) as caught:
                    bash(command)
                self.assertIn("Refusing to run this recursive-force rm command", str(caught.exception))
                self.assertTrue(Path(outside, "file.txt").exists())
                self.assertTrue(self._tracked("sub", "nested", "file.txt").exists())

    async def test_refuses_dot_dirs_and_dotfiles(self):
        self._make_tree()
        Path(self.test_dir, ".git", "objects").mkdir(parents=True, exist_ok=True)
        Path(self.test_dir, ".env").write_text("SECRET=1\n")
        Path(self.test_dir, "sub", ".env.local").write_text("SECRET=1\n")
        Path(self.test_dir, ".venv", "bin").mkdir(parents=True, exist_ok=True)
        for command in [
            "rm -rf .git",
            "rm -rf sub/.git",
            "rm -rf .env",
            "rm -rf .env.local",
            "rm -rf sub/.env.local",
            "rm -rf .venv",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveRmRefusalError) as caught:
                    bash(command)
                message = str(caught.exception)
                self.assertIn("Refusing to run this recursive-force rm command", message)
                self.assertIn("dot", message.lower())

    async def test_refuses_the_workspace_root_itself(self):
        self._make_tree()
        for command in ["rm -rf .", "rm -rf ./"]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveRmRefusalError) as caught:
                    bash(command)
                self.assertIn("workspace root itself", str(caught.exception))
                self.assertTrue(self._tracked("sub", "nested", "file.txt").exists())

    async def test_refuses_operands_it_cannot_resolve(self):
        self._make_tree()
        for command in [
            "rm -rf *",
            "rm -rf build/*",
            "rm -rf $SECRET",
            "rm -rf $(pwd)",
            "rm -rf ~otheruser",
            "rm -rf {}",
            "rm -rf -",
            "rm -rf",
            "echo hi | xargs rm -rf",
            "find . -name x -exec rm -rf {} +",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveRmRefusalError) as caught:
                    bash(command)
                self.assertIn("Refusing to run this recursive-force rm command", str(caught.exception))
                self.assertTrue(self._tracked("sub", "nested", "file.txt").exists())

    async def test_allows_inside_workspace_rm_rf(self):
        # Long options and flags-after-operand forms stay in the detection
        # vectors; BSD rm rejects them, so execution here sticks to forms that
        # work on every supported platform.
        for command in [
            "rm -rf sub",
            "rm -fr sub",
            "rm -Rf sub",
            "rm -rf ./sub",
            "rm -rf sub/nested",
            'rm -rf "my dir"',
            "rm -rf $PWD/sub",
            'rm -rf sub ./"my dir"',
        ]:
            with self.subTest(command=command):
                self._make_tree()
                result = await bash(command)
                self.assertEqual(result.exit_code, 0)

    async def test_dash_r_without_force_and_dash_f_without_recursion_untouched(self):
        self._make_tree()
        Path(self.test_dir, "file.txt").write_text("x\n")
        result = await bash("rm -r sub")
        self.assertEqual(result.exit_code, 0)
        self.assertFalse(self._tracked("sub").exists())
        self._make_tree()
        result = await bash("rm -f file.txt")
        self.assertEqual(result.exit_code, 0)
        self.assertFalse(self._tracked("file.txt").exists())
        self._make_tree()
        Path(self.test_dir, "file.txt").write_text("x\n")
        # Neither invocation combines both flags, so the compound stays untouched too.
        result = await bash("rm -r sub && rm -f file.txt")
        self.assertEqual(result.exit_code, 0)
        self.assertFalse(self._tracked("sub").exists())
        self.assertFalse(self._tracked("file.txt").exists())

    async def test_kwarg_bypass_runs_the_deletion(self):
        outside = self._outside_target()
        result = await bash(f"rm -rf ../{Path(outside).name}", allow_destructive_rm=True)
        self.assertEqual(result.exit_code, 0)
        self.assertFalse(Path(outside).exists())

    async def test_frozen_bypass_env_honored_when_set_at_launch(self):
        with mock.patch.object(bash_module, "_BASH_RM_BYPASS_AT_KERNEL_START", "1"):
            # -f on a path that does not exist is a successful no-op, so the
            # bypass is observable without deleting anything real.
            result = await bash("rm -rf ~/pa-rmguard-bypass-noop")
        self.assertEqual(result.exit_code, 0)

    async def test_frozen_bypass_zero_still_refuses(self):
        with mock.patch.object(bash_module, "_BASH_RM_BYPASS_AT_KERNEL_START", "0"):
            with self.assertRaises(DestructiveRmRefusalError):
                bash("rm -rf ~")

    async def test_mid_session_env_write_does_not_unlock(self):
        with mock.patch.dict(os.environ, {BASH_DESTRUCTIVE_RM_BYPASS_ENV: "1"}):
            with self.assertRaises(DestructiveRmRefusalError) as caught:
                bash("rm -rf ~")
        message = str(caught.exception)
        self.assertIn("WARNING", message)
        self.assertIn(BASH_DESTRUCTIVE_RM_BYPASS_ENV, message)
        self.assertIn("ignored by design", message)

    async def test_refuses_eval_wrapped_rm(self):
        self._make_tree()
        for command in [
            "eval 'rm -rf ~'",
            'eval "rm -rf ~"',
            "eval 'eval \"rm -rf ~\"'",
            "eval 'cd ~ && rm -rf .'",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveRmRefusalError) as caught:
                    bash(command)
                self.assertIn("wraps rm in eval", str(caught.exception))
                self.assertTrue(self._tracked("sub", "nested", "file.txt").exists())

    async def test_safe_eval_commands_still_run(self):
        result = await bash("eval 'echo hi'")
        self.assertEqual(result.exit_code, 0)
        self.assertIn("hi", result.output)
        result = await bash("eval \"echo 'rm -rf ~'\"")
        self.assertEqual(result.exit_code, 0)
        self.assertIn("rm -rf ~", result.output)

    async def test_quoted_data_is_untouched(self):
        for command in [
            "echo 'rm -rf ~'",
            'echo "rm -rf ~"',
        ]:
            with self.subTest(command=command):
                result = await bash(command)
                self.assertEqual(result.exit_code, 0)

    async def test_hardened_forms_are_refused(self):
        self._make_tree()
        home = tempfile.TemporaryDirectory()
        self.addCleanup(home.cleanup)
        Path(home.name, "keep.txt").write_text("keep\n")
        for command in [
            "rm 2>/dev/null -rf ~",
            "rm -rf \\\n~",
            "rm -rf &>/dev/null ~",
            "\\rm -rf ~",
            "/bin/rm -rf ~",
            "sudo rm -rf ~",
            '"rm" -rf ~',
            "FOO=1 rm -rf ~",
            "rm -rf ~ # cleanup",
        ]:
            with self.subTest(command=command):
                with mock.patch.dict(os.environ, {"HOME": home.name}):
                    with self.assertRaises(DestructiveRmRefusalError):
                        bash(command)
                self.assertTrue(Path(home.name, "keep.txt").exists())

    async def test_refuses_compound_when_any_invocation_escapes(self):
        self._make_tree()
        with self.assertRaises(DestructiveRmRefusalError):
            bash("rm -rf sub && rm -rf ..")
        self.assertTrue(self._tracked("sub", "nested", "file.txt").exists())
        # Every invocation inside the workspace stays allowed.
        self._make_tree()
        result = await bash("rm -rf sub && rm -fr \"my dir\"")
        self.assertEqual(result.exit_code, 0)

    async def test_refusal_elides_long_operand_lists(self):
        operands = " ".join(f"/outside-{index}" for index in range(12))
        with self.assertRaises(DestructiveRmRefusalError) as caught:
            bash(f"rm -rf {operands}")
        self.assertIn("... and 2 more", str(caught.exception))

    async def test_refusal_lists_both_bypasses(self):
        with self.assertRaises(DestructiveRmRefusalError) as caught:
            bash("rm -rf ~")
        message = str(caught.exception)
        self.assertIn("allow_destructive_rm=True", message)
        self.assertIn(BASH_DESTRUCTIVE_RM_BYPASS_ENV, message)
        self.assertIn("Delete inside the workspace", message)

    async def test_kernel_cwd_is_home_scenario(self):
        # The wave-1 audit confirmed a live kernel can boot with cwd == HOME.
        # HOME itself must stay refused while in-workspace deletions run.
        with mock.patch.dict(os.environ, {"HOME": self.test_dir}):
            self._make_tree()
            for command in ["rm -rf ~", "rm -rf .", "rm -rf $HOME"]:
                with self.subTest(command=command):
                    with self.assertRaises(DestructiveRmRefusalError):
                        bash(command)
            self.assertTrue(self._tracked("sub", "nested", "file.txt").exists())
            result = await bash("rm -rf sub")
            self.assertEqual(result.exit_code, 0)
            self.assertFalse(self._tracked("sub").exists())

    async def test_symlink_escape_refused(self):
        victim = tempfile.TemporaryDirectory()
        self.addCleanup(victim.cleanup)
        Path(victim.name, "keep").mkdir()
        os.symlink(victim.name, str(self._tracked("link")))
        with self.assertRaises(DestructiveRmRefusalError):
            bash("rm -rf link")
        self.assertTrue(Path(victim.name, "keep").exists())

    async def test_refuses_shell_dash_c_wrapped_rm(self):
        # A quoted argument to sh/bash -c is a live command string: the
        # payload must be scanned, not treated as data.
        self._make_tree()
        outside = self._outside_target()
        for command in [
            f"sh -c 'rm -rf {outside}'",
            f'bash -c "rm -rf {outside}"',
            f"zsh -c 'rm -rf {outside}'",
            f"sh -c 'sh -c \"rm -rf {outside}\"'",
            f"eval 'sh -c \"rm -rf {outside}\"'",
            f"busybox sh -c 'rm -rf {outside}'",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveRmRefusalError) as caught:
                    bash(command)
                self.assertIn("wraps rm in", str(caught.exception))
                self.assertTrue(Path(outside, "file.txt").exists())

    async def test_safe_shell_dash_c_commands_still_run(self):
        result = await bash("sh -c 'echo hi'")
        self.assertEqual(result.exit_code, 0)
        self.assertIn("hi", result.output)
        # Non-shell -c payloads are not shell syntax and stay unscanned.
        result = await bash("python3 -c 'print(\"safe\")'")
        self.assertEqual(result.exit_code, 0)

    async def test_refuses_rm_hidden_behind_quoted_paren(self):
        # A paren inside quotes must not close a $(...) scan early: the
        # live rm after it runs inside the substitution.
        self._make_tree()
        outside = self._outside_target()
        for command in [
            f'echo $(echo ")"; rm -rf {outside})',
            f"echo $(echo ')'; rm -rf {outside})",
            f"echo \"$(echo ')'; rm -rf {outside})\"",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveRmRefusalError) as caught:
                    bash(command)
                self.assertIn(
                    "Refusing to run this recursive-force rm command",
                    str(caught.exception),
                )
                self.assertTrue(Path(outside, "file.txt").exists())

    async def test_refuses_expansion_hidden_rm(self):
        # $VAR command words and $VAR/brace followers expand after the
        # guard runs; they cannot be resolved statically, so refuse them.
        self._make_tree()
        outside = self._outside_target()
        for command in [
            f"R=rm; $R -rf {outside}",
            f"flags=-rf; rm $flags {outside}",
            f"$R -rf {outside}",
            "rm {--recursive,--force} " + outside,
            f"rm -rf {{sub,{outside}}}",
        ]:
            with self.subTest(command=command):
                self._make_tree()
                with self.assertRaises(DestructiveRmRefusalError) as caught:
                    bash(command)
                self.assertIn(
                    "Refusing to run this recursive-force rm command",
                    str(caught.exception),
                )
                self.assertTrue(Path(outside, "file.txt").exists())
                self.assertTrue(self._tracked("sub", "nested", "file.txt").exists())

    async def test_refuses_relocated_rm_after_cd(self):
        # Relative operands resolve against the directory the shell runs
        # from after cd/pushd, not the kernel cwd; an unresolvable
        # relocation fails closed too.
        self._make_tree()
        outside = self._outside_target()
        Path(outside, "victim").mkdir()
        Path(outside, "victim", "file.txt").write_text("keep\n")
        for command in [
            f"cd {outside}; rm -rf victim",
            f"cd {outside} && rm -rf victim",
            f"cd {outside}; rm -rf $PWD/victim",
            f"pushd {outside}; rm -rf victim",
            "cd $UNRESOLVED_DIR; rm -rf sub",
        ]:
            with self.subTest(command=command):
                self._make_tree()
                with self.assertRaises(DestructiveRmRefusalError):
                    bash(command)
                self.assertTrue(Path(outside, "victim", "file.txt").exists())
                self.assertTrue(self._tracked("sub", "nested", "file.txt").exists())

    async def test_cd_into_workspace_still_allows_in_workspace_rm(self):
        self._make_tree()
        result = await bash("cd sub && rm -rf nested")
        self.assertEqual(result.exit_code, 0)
        self.assertFalse(self._tracked("sub", "nested").exists())
        self.assertTrue(self._tracked("sub").exists())
        # A subshell's cd does not relocate the caller's shell.
        self._make_tree()
        outside = self._outside_target()
        result = await bash(f"(cd {outside}; echo hi) && rm -rf sub")
        self.assertEqual(result.exit_code, 0)
        self.assertFalse(self._tracked("sub").exists())
        self.assertTrue(Path(outside, "file.txt").exists())

    async def test_refuses_brace_expanded_rm(self):
        self._make_tree()
        outside = self._outside_target()
        for command in [
            "rm {--recursive,--force} sub",
            f"rm -rf {{sub,{outside}}}",
        ]:
            with self.subTest(command=command):
                self._make_tree()
                with self.assertRaises(DestructiveRmRefusalError):
                    bash(command)
                self.assertTrue(self._tracked("sub", "nested", "file.txt").exists())
                self.assertTrue(Path(outside, "file.txt").exists())

    async def test_guard_scans_the_spawn_prefix(self):
        # The spawn prepends PRIME_AGENT_BASH_COMMAND_PREFIX, and that env
        # value is model-writable mid-session: the guard must scan exactly
        # what the shell will run.
        outside = self._outside_target()
        with mock.patch.dict(
            os.environ, {"PRIME_AGENT_BASH_COMMAND_PREFIX": f"rm -rf {outside}"}
        ):
            with self.assertRaises(DestructiveRmRefusalError) as caught:
                bash("echo hi")
        self.assertIn(
            "Refusing to run this recursive-force rm command",
            str(caught.exception),
        )
        self.assertTrue(Path(outside, "file.txt").exists())
        # A benign prefix stays harmless.
        with mock.patch.dict(
            os.environ, {"PRIME_AGENT_BASH_COMMAND_PREFIX": "export PREFIX_OK=1"}
        ):
            result = await bash("echo $PREFIX_OK")
        self.assertEqual(result.exit_code, 0)

    async def test_refuses_symlink_swaps_and_missing_operands(self):
        self._make_tree()
        outside = self._outside_target()
        # The command creates the symlink and deletes through it in one
        # go: at guard time the operand does not exist yet.
        with self.assertRaises(DestructiveRmRefusalError):
            bash(f"ln -s {outside} swap && rm -rf swap/")
        self.assertTrue(Path(outside, "file.txt").exists())
        # A symlink operand can be swapped between check and run, even
        # when it currently points inside the workspace.
        os.symlink(str(self._tracked("sub")), str(self._tracked("alias")))
        with self.assertRaises(DestructiveRmRefusalError):
            bash("rm -rf alias/")
        self.assertTrue(self._tracked("sub", "nested", "file.txt").exists())
        # Missing paths cannot be verified: they may be created as
        # symlinks before the rm runs.
        with self.assertRaises(DestructiveRmRefusalError):
            bash("rm -rf never-created-yet")
        self.assertTrue(self._tracked("sub", "nested", "file.txt").exists())

    async def test_refuses_expansion_hidden_rm_in_more_positions(self):
        # Assignment prefixes, env, and keyword/grouping positions still put
        # an expansion word in command position; any of them with rm-flag
        # followers must be refused as unresolvable.
        self._make_tree()
        outside = self._outside_target()
        for command in [
            f"FOO=1 $R -rf {outside}",
            f"env $R -rf {outside}",
            "if true; then $R -rf x; fi",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveRmRefusalError) as caught:
                    bash(command)
                self.assertIn(
                    "Refusing to run this recursive-force rm command",
                    str(caught.exception),
                )
                self.assertTrue(Path(outside, "file.txt").exists())
        # Expansion command words without rm-shaped flags stay allowed.
        result = await bash('ECHO_BIN=echo; "$ECHO_BIN" -u hi')
        self.assertEqual(result.exit_code, 0)
        self.assertIn("hi", result.output)

    async def test_refuses_gnu_long_option_abbreviations(self):
        # GNU rm accepts unambiguous long-option abbreviations: --recurs and
        # --forc behave exactly like --recursive and --force.
        self._make_tree()
        outside = self._outside_target()
        for command in [
            f"rm --recurs --forc {outside}",
            f"rm --recursive --forc {outside}",
            f"rm --recurs --force {outside}",
            f"rm --r --f {outside}",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveRmRefusalError) as caught:
                    bash(command)
                self.assertIn(
                    "Refusing to run this recursive-force rm command",
                    str(caught.exception),
                )
                self.assertTrue(Path(outside, "file.txt").exists())
        # Non-matching abbreviations of other options stay allowed.
        self._make_tree()
        result = await bash("rm -v sub/nested/file.txt")
        self.assertEqual(result.exit_code, 0)

    async def test_refuses_trap_action_rm(self):
        # A trap action string is a live command executed at shell exit;
        # it must be scanned like an eval payload.
        self._make_tree()
        outside = self._outside_target()
        for command in [
            f"trap 'rm -rf {outside}' EXIT",
            f'trap "rm -rf {outside}" EXIT',
            f"sh -c 'trap \"rm -rf {outside}\" EXIT'",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveRmRefusalError) as caught:
                    bash(command)
                self.assertIn("wraps rm in", str(caught.exception))
                self.assertTrue(Path(outside, "file.txt").exists())
        # Benign trap actions still run.
        result = await bash("trap 'echo done' EXIT; echo hi")
        self.assertEqual(result.exit_code, 0)

    async def test_heredoc_data_bodies_are_not_live_commands(self):
        # A here-document body fed to a non-interpreter command is data:
        # it must not be scanned as live rm commands.
        result = await bash("cat <<'EOF'\nrm -rf /printed-not-run\nEOF")
        self.assertEqual(result.exit_code, 0)
        self.assertIn("rm -rf /printed-not-run", result.output)
        result = await bash("cat <<EOF\nrm -rf outside-not-run\nEOF")
        self.assertEqual(result.exit_code, 0)
        self.assertIn("rm -rf outside-not-run", result.output)
        # Bodies fed to interpreters (directly or through a pipeline) stay
        # live commands and are refused.
        self._make_tree()
        outside = self._outside_target()
        for command in [
            f"sh <<EOF\nrm -rf {outside}\nEOF",
            f"cat <<EOF | sh\nrm -rf {outside}\nEOF",
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveRmRefusalError):
                    bash(command)
                self.assertTrue(Path(outside, "file.txt").exists())

    async def test_refuses_heredoc_bodies_reaching_interpreters(self):
        # The body may reach an interpreter through a pipeline consumer after
        # the terminator or through a script file written in the same
        # command: any interpreter word in the command keeps bodies live.
        self._make_tree()
        outside = self._outside_target()
        for command in [
            "{ cat <<EOF\nrm -rf %s\nEOF\n} | sh" % outside,
            "cat <<EOF > script.sh\nrm -rf %s\nEOF\nsh script.sh" % outside,
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveRmRefusalError):
                    bash(command)
                self.assertTrue(Path(outside, "file.txt").exists())

    async def test_refuses_trap_end_of_options_and_escaped_wrappers(self):
        # `trap --` puts the action behind an end-of-options marker, and a
        # backslash inside a wrapper name folds to the plain word, so the
        # scan must match parsed word values, not raw substrings.
        self._make_tree()
        outside = self._outside_target()
        for command in [
            "trap -- 'rm -rf %s' EXIT" % outside,
            "t\\rap 'rm -rf %s' EXIT" % outside,
            "ev\\al 'rm -rf %s'" % outside,
        ]:
            with self.subTest(command=command):
                with self.assertRaises(DestructiveRmRefusalError):
                    bash(command)
                self.assertTrue(Path(outside, "file.txt").exists())
        # Benign escaped wrappers still run.
        result = await bash("t\\rap 'echo done' EXIT; echo hi")
        self.assertEqual(result.exit_code, 0)

    async def test_operands_after_end_of_options_are_checked(self):
        self._make_tree()
        with self.assertRaises(DestructiveRmRefusalError):
            bash("rm -rf -- ..")
        self.assertTrue(self._tracked("sub", "nested", "file.txt").exists())


if __name__ == "__main__":
    unittest.main()
