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
from rlm.bash import BASH_SECRET_ECHO_BYPASS_ENV, SecretEchoRefusalError

# The package re-exports the bash() function under the same name, so reach the
# module through sys.modules for internals.
bash_module = sys.modules["rlm.bash"]

# Every spawned command and probe in this suite carries an explicit timeout.
AWAIT_TIMEOUT = 10.0
SUBPROCESS_TIMEOUT = 30

# Vectors for the secret-echo detector. Two command shapes leak secrets into
# the transcript: a bare environment dump, and a read of a known secret file
# under the user's home. Only the `~` spelling of that path expands when it is
# unquoted, while `$HOME` expands unquoted and inside double quotes too.
SECRET_ECHO_MATCHING_COMMANDS = [
    # Bare dumps: zero operands means the whole environment goes to stdout.
    "env",
    "  env  ",
    "printenv",
    "env -0",
    "env -i",
    "env --",
    "printenv -0",
    "export -p",
    "echo hi && env",
    "cd /tmp; printenv",
    "env # dump the environment",
    # Secret-file reads, both home spellings, quoted and unquoted.
    "cat ~/.ssh/id_rsa",
    "cat ~/.ssh/id_ed25519",
    "cat $HOME/.ssh/id_rsa",
    'cat "$HOME/.ssh/id_rsa"',
    "cat ${HOME}/.ssh/id_rsa",
    "cat ~/.aws/credentials",
    "cat $HOME/.aws/credentials",
    "cat ~/.gnupg/secring.gpg",
    "echo ~/.gnupg/secring.gpg",
    # Assignment prefixes: the shell runs the dump with those bindings set.
    "FOO=1 env",
    "FOO=1 printenv",
    "FOO=1 export -p",
    "AWS_PROFILE=prod cat ~/.aws/credentials",
    "FOO='bar baz' env",
    # Redirections never narrow what the command prints.
    "env 2>/dev/null",
    "env 2> /dev/null",
    "export -p 2>&1",
    "env 1>&2",
    "env > /tmp/env.txt",
    "2> /dev/null env",
    # Quoted command words and concatenations still run the command.
    '"env"',
    '"cat" ~/.ssh/id_rsa',
    'ca"t" ~/.ssh/id_rsa',
    # $HOME glued to the path across a closing double quote still reads it.
    'cat "$HOME"/.ssh/id_rsa',
    'cat $HOME"/.ssh/id_rsa"',
    # A dump piped into an unbounded filter is still a dump.
    "env | grep .",
    "env | grep -v SAFE_VAR",
    "env | grep ''",
    "env | grep ^AWS_",
    # Context flags widen every match with surrounding lines.
    "env | grep -A5 SAFE_VAR",
    "env | grep -B5 SAFE_VAR",
    "env | grep -C5 SAFE_VAR",
    "env | grep --after-context=5 SAFE_VAR",
    # A `2>&1` redirect is part of the dump word, not a background operator.
    "env 2>&1",
]

SECRET_ECHO_NON_MATCHING_COMMANDS = [
    # Listing a directory of secrets shows filenames, it does not print them.
    "ls ~/.ssh",
    "ls -la ~/.aws",
    "ls ~/.gnupg",
    # Targeted reads: one named variable, plus the assignment and executor
    # forms of env/export.
    "printenv HOME",
    "printenv PATH SAFE_VAR",
    "env FOO=1 cmd",
    "env -u FOO cmd",
    "printenv -0 FOO",
    "printenv -l FOO",
    "export FOO=1",
    "export -n FOO",
    # A `.env`-class file in the workspace is part of normal development.
    "cat .env",
    "grep GITHUB_TOKEN .env",
    # A dump filtered down to one key by grep is the documented targeted read.
    "env | grep SAFE_VAR",
    "printenv | grep SAFE_VAR",
    "export -p | grep SAFE_VAR",
    # One key out of a secret file is a targeted read, not a dump.
    "grep GITHUB_TOKEN ~/.aws/credentials",
    # Quoted data is data: a single-quoted span never expands, and a tilde
    # inside double quotes never expands either.
    "echo 'env'",
    "echo 'cat ~/.ssh/id_rsa'",
    'echo "cat ~/.ssh/id_rsa"',
    'cat "~/.ssh/id_rsa"',
    # Ordinary commands.
    "cat /etc/passwd",
    "cat README.md",
    "echo $HOME",
    "git status",
    "npm run check",
    # A quoted operand stays one word, so these are executor forms that fail
    # to find a command, not dumps.
    "env 'foo&bar'",
    'echo "a & env"',
    '"env -0"',
    # A dump filtered by a fixed-string single-key grep stays bounded:
    # `-F` matches `--fixed-strings`, `-i`/`-a`/`-b`/`-c` stay bounded, and a
    # `2>&1` redirect before the pipe does not split the dump from its filter.
    "env | grep -e SAFE_VAR",
    "env | grep -- SAFE_VAR",
    "env 2>/dev/null | grep PATH",
    "env | grep -F SAFE_VAR",
    "env | grep -i PATH",
    "env | grep -a PATH",
    "env 2>&1 | grep PATH",
    "printenv 2>&1 | grep SAFE_VAR",
]


class SecretEchoDetectionTest(unittest.TestCase):
    def test_matches_dumps_and_secret_file_reads(self):
        for command in SECRET_ECHO_MATCHING_COMMANDS:
            with self.subTest(command=command):
                self.assertIsNotNone(bash_module._secret_echo_violation(command))

    def test_does_not_match_targeted_or_literal_commands(self):
        for command in SECRET_ECHO_NON_MATCHING_COMMANDS:
            with self.subTest(command=command):
                self.assertIsNone(bash_module._secret_echo_violation(command))

    def test_reason_distinguishes_dump_from_secret_file(self):
        self.assertEqual(
            bash_module._secret_echo_violation("env"), "the full environment"
        )
        self.assertEqual(
            bash_module._secret_echo_violation("cat ~/.ssh/id_rsa"),
            "a known secret file",
        )


class SecretEchoGuardTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self._prev_cwd = os.getcwd()
        self._prev_env = dict(os.environ)
        os.environ.pop(BASH_SECRET_ECHO_BYPASS_ENV, None)
        os.environ.pop("PRIME_AGENT_BASH_COMMAND_PREFIX", None)
        # The launch-time bypass snapshot is a module attribute frozen at
        # import; pin it to "unset" so tests stay deterministic.
        frozen_patch = mock.patch.object(
            bash_module, "_SECRET_ECHO_BYPASS_AT_KERNEL_START", False
        )
        frozen_patch.start()
        self.addCleanup(frozen_patch.stop)
        late_warn_patch = mock.patch.object(
            bash_module, "_secret_echo_late_bypass_warned", False
        )
        late_warn_patch.start()
        self.addCleanup(late_warn_patch.stop)
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        # Restore cwd before the temp dir disappears (cleanups run LIFO).
        self.addCleanup(self._restore_env)
        self.addCleanup(os.chdir, self._prev_cwd)
        self.test_dir = temp.name
        os.chdir(self.test_dir)

    def _restore_env(self):
        os.environ.clear()
        os.environ.update(self._prev_env)

    async def _run(self, command: str, **kwargs):
        return await asyncio.wait_for(bash(command, **kwargs), AWAIT_TIMEOUT)

    def _refused(self, command: str, **kwargs) -> str:
        # The refusal is synchronous: nothing may spawn before it is raised.
        with self.assertRaises(SecretEchoRefusalError) as caught:
            bash(command, **kwargs)
        return str(caught.exception)

    async def test_bare_env_dump_refused(self):
        message = self._refused("env")
        self.assertIn("Refusing to run this command", message)
        lowered = message.lower()
        # The transcript-leak risk is stated.
        self.assertIn("transcript", lowered)
        self.assertIn("session logs", lowered)
        # Targeted reads are suggested, including the piped form the guard
        # itself allows.
        self.assertIn("printenv SAFE_VAR", message)
        self.assertIn("env | grep SAFE_VAR", message)
        # Both bypasses are documented.
        self.assertIn("allow_secret_echo=True", message)
        self.assertIn(BASH_SECRET_ECHO_BYPASS_ENV, message)

    async def test_refusal_happens_before_any_process_starts(self):
        # A refused command must never reach BashHandle, so the guard cannot
        # leave a half-spawned process behind.
        with mock.patch.object(
            bash_module, "BashHandle", side_effect=AssertionError("spawned")
        ):
            self._refused("env")

    async def test_bare_printenv_refused(self):
        message = self._refused("printenv")
        self.assertIn("Refusing to run this command", message)
        self.assertIn("the full environment", message)

    async def test_flags_only_env_dump_refused(self):
        # `env -0` and friends print the same unfiltered environment as a bare
        # `env`, in another encoding, so only flags may follow the word.
        for command in ["env -0", "env -i", "env --", "printenv -0"]:
            with self.subTest(command=command):
                message = self._refused(command)
                self.assertIn("the full environment", message)

    async def test_export_p_dump_refused(self):
        message = self._refused("export -p")
        self.assertIn("Refusing to run this command", message)
        self.assertIn("the full environment", message)

    async def test_secret_file_read_refused(self):
        for command in ["cat ~/.ssh/id_ed25519", 'cat "$HOME/.ssh/id_rsa"']:
            with self.subTest(command=command):
                message = self._refused(command)
                self.assertIn("Refusing to run this command", message)
                self.assertIn("a known secret file", message)

    async def test_chained_dump_refused(self):
        message = self._refused("echo hi && env")
        self.assertIn("the full environment", message)

    async def test_assignment_prefix_dump_refused(self):
        # The shell runs `FOO=1 env` as a bare dump, so the guard reads the
        # command word after the assignment and refuses before any spawn.
        with mock.patch.object(
            bash_module, "BashHandle", side_effect=AssertionError("spawned")
        ):
            message = self._refused("FOO=1 env")
        self.assertIn("the full environment", message)

    async def test_redirected_dump_refused(self):
        # A redirection never narrows what reaches the transcript, so it is
        # dropped before the bare-dump check.
        with mock.patch.object(
            bash_module, "BashHandle", side_effect=AssertionError("spawned")
        ):
            message = self._refused("env 2>/dev/null")
        self.assertIn("the full environment", message)

    async def test_unbounded_grep_filter_refused(self):
        # `grep .` matches every line, so the follower filters nothing and
        # the dump stays a dump.
        with mock.patch.object(
            bash_module, "BashHandle", side_effect=AssertionError("spawned")
        ):
            message = self._refused("env | grep .")
        self.assertIn("the full environment", message)

    async def test_redirected_dump_with_bounded_filter_allowed(self):
        # The redirected dump piped through a bounded grep is the documented
        # targeted read and must still run.
        result = await self._run("env 2>/dev/null | grep PATH")
        self.assertEqual(result.exit_code, 0)
        lines = [line for line in result.output.splitlines() if line]
        self.assertTrue(lines, result.output)
        self.assertTrue(all("PATH" in line for line in lines), result.output)

    async def test_dotenv_read_allowed(self):
        Path(self.test_dir, ".env").write_text("SAFE_VAR=1\n")
        result = await self._run("cat .env")
        self.assertEqual(result.exit_code, 0)
        self.assertIn("SAFE_VAR=1", result.output)

    async def test_targeted_env_read_allowed(self):
        result = await self._run("env | grep PATH")
        self.assertEqual(result.exit_code, 0)
        lines = [line for line in result.output.splitlines() if line]
        self.assertTrue(any(line.startswith("PATH=") for line in lines), result.output)
        # The dump was filtered: only matching lines reached the transcript.
        self.assertTrue(all("PATH" in line for line in lines), result.output)

    async def test_listing_and_targeted_forms_allowed(self):
        ssh = Path(self.test_dir, ".ssh")
        ssh.mkdir()
        Path(ssh, "id_rsa").write_text("PRIVATE-KEY-BODY\n")
        with mock.patch.dict(os.environ, {"HOME": self.test_dir}):
            # Listing shows the filename; the guard must not refuse it, and
            # the file's contents must not reach the transcript.
            result = await self._run("ls ~/.ssh")
            self.assertEqual(result.exit_code, 0)
            self.assertIn("id_rsa", result.output)
            self.assertNotIn("PRIVATE-KEY-BODY", result.output)
            result = await self._run("printenv HOME")
            self.assertEqual(result.exit_code, 0)
            self.assertIn(self.test_dir, result.output)
            result = await self._run("export FOO=1")
            self.assertEqual(result.exit_code, 0)

    async def test_kwarg_bypass_runs_the_dump(self):
        result = await self._run("env", allow_secret_echo=True)
        self.assertEqual(result.exit_code, 0)
        self.assertIn("PATH=", result.output)

    async def test_kwarg_bypass_does_not_leak_into_later_commands(self):
        result = await self._run("env", allow_secret_echo=True)
        self.assertEqual(result.exit_code, 0)
        self._refused("env")

    async def test_frozen_bypass_env_honored_when_set_at_launch(self):
        with mock.patch.object(
            bash_module, "_SECRET_ECHO_BYPASS_AT_KERNEL_START", True
        ):
            result = await self._run("env")
        self.assertEqual(result.exit_code, 0)
        self.assertIn("PATH=", result.output)

    async def test_mid_session_env_write_does_not_unlock(self):
        stderr = io.StringIO()
        with (
            mock.patch.dict(os.environ, {BASH_SECRET_ECHO_BYPASS_ENV: "1"}),
            redirect_stderr(stderr),
        ):
            self._refused("env")
            self._refused("printenv")
        warning = stderr.getvalue()
        self.assertIn(BASH_SECRET_ECHO_BYPASS_ENV, warning)
        self.assertIn("appeared after kernel start", warning)
        # One loud warning across both refusals, and the guard stayed armed.
        self.assertEqual(warning.count("appeared after kernel start"), 1)
        # A falsy mid-session value stays inert too.
        second = io.StringIO()
        with (
            mock.patch.dict(os.environ, {BASH_SECRET_ECHO_BYPASS_ENV: "0"}),
            redirect_stderr(second),
        ):
            self._refused("cat ~/.ssh/id_rsa")
        self.assertEqual(second.getvalue(), "")


PROBE = (
    "import asyncio\n"
    "import sys\n"
    "from rlm import bash\n"
    "async def main():\n"
    "    result = await bash(sys.argv[1])\n"
    "    return result.exit_code\n"
    "raise SystemExit(asyncio.run(main()))\n"
)


class FrozenBypassEnvLaunchTest(unittest.TestCase):
    """Launch-level behavior of the frozen bypass env var, in fresh kernels."""

    def _launch(
        self, extra_env: dict[str, str], probe: str = PROBE, command: str = "env"
    ) -> subprocess.CompletedProcess:
        env = dict(os.environ)
        env.pop(BASH_SECRET_ECHO_BYPASS_ENV, None)
        env.update(extra_env)
        return subprocess.run(
            [sys.executable, "-c", probe, command],
            cwd=tempfile.gettempdir(),
            env=env,
            capture_output=True,
            text=True,
            timeout=SUBPROCESS_TIMEOUT,
        )

    def test_launch_value_disables_the_guard_for_that_kernel(self):
        # Without the variable the same probe is refused, so the test proves
        # the guard runs and that the launch value is what disables it.
        refused = self._launch({})
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("Refusing to run this command", refused.stderr)
        allowed = self._launch({BASH_SECRET_ECHO_BYPASS_ENV: "1"})
        self.assertEqual(allowed.returncode, 0, allowed.stderr)
        # The honored bypass is not a late one, so nothing is warned about.
        self.assertEqual(allowed.stderr, "")

    def test_mid_session_os_environ_write_does_not_unlock_a_fresh_kernel(self):
        completed = self._launch(
            {},
            "import asyncio\n"
            "import os\n"
            "import sys\n"
            "from rlm import bash\n"  # kernel start: the variable is absent
            f"os.environ[{BASH_SECRET_ECHO_BYPASS_ENV!r}] = '1'\n"
            "async def main():\n"
            "    result = await bash(sys.argv[1])\n"
            "    return result.exit_code\n"
            "raise SystemExit(asyncio.run(main()))\n",
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("Refusing to run this command", completed.stderr)
        self.assertIn("appeared after kernel start", completed.stderr)


if __name__ == "__main__":
    unittest.main()
