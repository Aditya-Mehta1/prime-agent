"""Privilege-escalation guard for the kernel bash tool."""

from __future__ import annotations

import asyncio
import contextlib
import io
import os
import sys
import tempfile
import time
import unittest
from unittest import mock

from rlm import bash

bash_module = sys.modules["rlm.bash"]
PrivilegeEscalationRefusalError = bash_module.PrivilegeEscalationRefusalError
BASH_SUDO_BYPASS_ENV = bash_module.BASH_SUDO_BYPASS_ENV

AWAIT_TIMEOUT = 15.0

SUDO_MATCHING_COMMANDS = [
    "sudo ls",
    "sudo apt install ripgrep",
    "sudo -i",
    "sudo -u root id",
    "/usr/bin/sudo id",
    "./sudo id",
    "doas id",
    "/usr/local/bin/doas id",
    "echo x | sudo tee /etc/hosts",
    "FOO=1 sudo ls",
    "nice sudo ls",
    "env sudo ls",
    "env -i sudo ls",
    "timeout 30 sudo ls",
    "nice -n 10 sudo id",
    "! sudo id",
    "time sudo id",
    "cd /tmp && sudo make install",
    "echo hi\nsudo id",
    "( sudo id )",
    "{ sudo id; }",
    "sudo id || sudo -i",
    "sh -c 'sudo id'",
    'bash -lc "sudo id"',
    "eval 'sudo id'",
    "eval sudo id",
    "ls | xargs sudo rm",
    "find . | xargs -I{} sudo chown root {}",
    '"sudo" id',
    'su"do" id',
    "CMD=sudo; $CMD id",
    "CMD=sudo\n$CMD id",
    "$SUDO id",
    "sh <<EOF\nsudo id\nEOF",
    "exec sudo id",
    "nohup sudo id",
    "setsid sudo id",
    "2>/dev/null sudo id",
    "foo >x& sudo id",
    "if true; then sudo id; fi",
    "while :; do sudo id; break; done",
    "for i in 1; do sudo id; done",
    "until false; do doas id; done",
    "else sudo id; fi",
    "command sudo id",
    "command -p sudo id",
    'x="$(sudo id)"',
    'x="`sudo id`"',
    'echo "`sudo id`"',
    "busybox sudo id",
    "exec -a name sudo id",
    "xargs -n1 -I{} sh -c 'sudo id'",
    "sh -c$'sudo id'",
    "diff <(sudo id) x",
    "bash <(sudo id)",
    "cat >(sudo id)",
    "diff <(sudo -l) <(echo x)",
    "while read l; do echo $l; done < <(sudo id)",
    "$'su\\x64o' id",
    "$'su\\144o' -n id",
    "$'su\\u0064o' id",
    "$'su'$'\\x64''o' id",
    "echo \"$(printf ')'; sudo id)\"",
    "coproc sudo id",
    "coproc sh -c 'sudo id'",
    "env -S 'sudo id'",
    "env -S'sudo id'",
    "env -C /tmp sudo id",
    "env --split-string 'sudo id'",
    "env --chdir /tmp sudo id",
    "env -u FOO sudo id",
    "timeout -s KILL 5 sudo id",
    "stdbuf -o L sudo id",
    "ionice -c 2 sudo id",
    "timeout -k 1 5 sudo id",
    "timeout --signal=KILL 5 sudo id",
    "stdbuf -oL sudo id",
    "setsid -w sudo id",
    "printf x | xargs -I '{}' sudo id",
    "printf x | xargs -n 1 sudo id",
    "printf x | xargs -P 2 sudo id",
    "printf x | xargs -s 100 sudo id",
    "printf x | xargs -L 1 sudo id",
    "printf x | xargs -E eof sudo id",
    "printf x | xargs -J % sudo id",
    "printf x | xargs -a FILE sudo id",
    "printf x | xargs -d '' sudo id",
    "printf x | xargs -n1 sudo id",
    "bash -ce 'sudo id'",
    "bash -cx 'sudo id'",
    "find . -maxdepth 0 -exec sudo id \\;",
    "find . -maxdepth 0 -execdir sudo id \\;",
    "find . -maxdepth 0 -exec sudo id {} +",
    "find . -maxdepth 0 -exec sh -c 'sudo id' \\;",
    "bash <<< 'sudo id'",
    "sh -s <<< 'sudo id'",
    "source /dev/stdin <<< 'sudo id'",
    "sh < <(printf 'sudo id')",
    "bash < <(echo sudo id)",
    "source <(printf 'sudo id')",
    ". <(echo sudo id)",
    "sh <(printf 'sudo id')",
    "${SUDO_CMD:-sudo} id",
    "${X:-$(printf sudo)} id",
    "CMD=sudo; eval \"$CMD id\"",
    "CMD=sudo; sh -c \"$CMD id\"",
    "eval eval eval eval eval eval eval 'sudo id'",
    "eval eval eval eval eval eval 'sh -c \"sudo id\"'",
    "su{d,}o id",
    "s{u,x}do id",
    "{sudo,} id",
    "{sudo,echo} hi",
    "sud[o] id",
    "[s]udo id",
    "su?do id",
    "alias p='sudo id'",
    "alias p=$'sudo id'",
    "shopt -s expand_aliases\nalias p='sudo id'\np",
    "shopt -s expand_aliases\nalias p='sudo id'\neval p",
    "cat <<EOF | sh\nsudo id\nEOF",
    "while read -r l; do eval \"$l\"; done <<EOF\nsudo id\nEOF",
    "env -vu FOO sudo id",
    "env -vC /tmp sudo id",
    "env -iu FOO sudo id",
    "env -iC /tmp sudo id",
    "env -iS'sudo id'",
    "printf x | xargs -rn 2 sudo id",
    "printf x | xargs -tn 1 sudo id",
    "printf x | xargs -0n 1 sudo id",
    "cat <<EOF | exec sh\nsudo id\nEOF",
    "cat <<EOF | sudo id\nEOF",
    "shopt -s expand_aliases; alias p='sudo id'; eval p",
    "shopt -s expand_aliases\nalias p=$'sudo id'\np",
    "cat <<EOF | command sh\nsudo id\nEOF",
    "cat <<EOF | command -- sh\nsudo id\nEOF",
    "cat <<EOF | command -p sh\nsudo id\nEOF",
    "cat <<EOF | xargs -I{} sh -c {}\nsudo id\nEOF",
    "cat <<EOF | find . -maxdepth 0 -exec sh -s {} \\;\nsudo id\nEOF",
    "cat <<EOF | timeout 5 sh\nsudo id\nEOF",
    "cat <<EOF | xargs -- sh\nsudo id\nEOF",
    "cat <<EOF | env -i sh\nsudo id\nEOF",
    "cat <<EOF | nohup sh\nsudo id\nEOF",
    "cat <<EOF | stdbuf -o L sh\nsudo id\nEOF",
    "cat <<EOF | builtin sh\nsudo id\nEOF",
    "cat <<EOF | busybox sh\nsudo id\nEOF",
    "cat <<EOF | find . -maxdepth 0 -execdir sh -s {} \\;\nsudo id\nEOF",
    "cat <<EOF | xargs -n1 sh\nsudo id\nEOF",
    "shopt -s expand_aliases; alias p='sudo id'; p",
    "sudo",
]

SUDO_NON_MATCHING_COMMANDS = [
    "man sudo",
    "grep sudo file.md",
    "echo sudo",
    "echo 'sudo apt install'",
    'echo "sudo"',
    "ls # sudo id",
    "ls /etc/sudoers.d",
    "cat sudo.txt",
    "command -v sudo",
    "which sudo",
    "type sudo",
    "whereis sudo",
    "ls",
    "git status",
    "npm run check",
    "env | grep PATH",
    "printenv HOME",
    "env FOO=1 cmd",
    "cat <<EOF\nsudo id\nEOF",
    "python -c \"print('sudo')\"",
    "$(date)",
    "timeout 30 ls",
    "env -i ls",
    "cd /tmp && make install",
    "echo hi && echo bye",
    "time ls",
    "nice -n 5 ls",
    "exit 0 || ls",
    "command -V sudo",
    "if true; then echo hi; fi",
    "for i in 1; do ls; done",
    "case x in y) ls;; esac",
    'x="$(date)"',
    "diff <(echo a) <(echo b)",
    "cat >(wc -l) < /dev/null",
    "env -0 ls",
    "xargs -r echo hi",
    "xargs -0 -n 1 echo",
    "xargs -rn2 echo hi",
    "echo sh <<EOF\nsudo id\nEOF",
    "grep bash <<EOF\nsudo id\nEOF",
    "cat <<EOF | command -v sh\nsudo id\nEOF",
    "cat <<EOF | command -V sh\nsudo id\nEOF",
    "bash <(echo hi)",
    "CMD=ls; eval \"$CMD\"",
    "echo {a,b}",
    "{ls,} id",
    "./bin/*.sh",
    "alias ll='ls -l'",
    "alias",
    "timeout -s KILL 5 ls",
    "printf x | xargs -n 1 ls",
    "env -C /tmp ls",
    "stdbuf -o L ls",
]


class SudoDetectionTest(unittest.TestCase):
    def test_sudo_command_words_are_violations(self):
        for command in SUDO_MATCHING_COMMANDS:
            with self.subTest(command=command):
                self.assertIsNotNone(bash_module._sudo_violation(command))

    def test_non_command_mentions_are_allowed(self):
        for command in SUDO_NON_MATCHING_COMMANDS:
            with self.subTest(command=command):
                self.assertIsNone(bash_module._sudo_violation(command))

    def test_reason_phrases(self):
        self.assertIn("sudo", bash_module._sudo_violation("sudo ls"))
        self.assertIn("doas", bash_module._sudo_violation("doas id"))


class BraceFloodTest(unittest.TestCase):
    """The brace-expansion cap: a flood fails closed and never scans quadratically."""

    FLOOD = "{" * 32000

    def test_brace_flood_command_word_is_refused_promptly(self):
        started = time.monotonic()
        violation = bash_module._sudo_violation(self.FLOOD)
        elapsed = time.monotonic() - started
        self.assertLess(elapsed, 5.0)
        self.assertIsNotNone(violation)

    def test_brace_flood_operand_word_is_still_judged(self):
        # A long comma-free blob in operand position is data, not a command word:
        # the word is judged without rescanning the tail for every `{`.
        started = time.monotonic()
        violation = bash_module._sudo_violation("echo " + self.FLOOD)
        elapsed = time.monotonic() - started
        self.assertLess(elapsed, 5.0)
        self.assertIsNone(violation)


class SudoGuardTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self._cwd = os.getcwd()
        self._environ = dict(os.environ)
        os.environ.pop(BASH_SUDO_BYPASS_ENV, None)
        os.environ.pop("PRIME_AGENT_BASH_COMMAND_PREFIX", None)
        frozen = mock.patch.object(bash_module, "_SUDO_BYPASS_AT_KERNEL_START", False)
        frozen.start()
        self.addCleanup(frozen.stop)
        warned = mock.patch.object(bash_module, "_sudo_late_bypass_warned", False)
        warned.start()
        self.addCleanup(warned.stop)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        os.chdir(tmp.name)
        self.addCleanup(self._restore)

    def _restore(self):
        os.chdir(self._cwd)
        os.environ.clear()
        os.environ.update(self._environ)

    def _mock_handle(self):
        handle_patch = mock.patch.object(bash_module, "BashHandle")
        instance = handle_patch.start()
        self.addCleanup(handle_patch.stop)
        return instance

    def _refused(self, command, **kwargs):
        with self.assertRaises(PrivilegeEscalationRefusalError) as caught:
            bash(command, **kwargs)
        return str(caught.exception)

    async def _run(self, command, **kwargs):
        return await asyncio.wait_for(bash(command, **kwargs), AWAIT_TIMEOUT)

    def test_refusal_message_documents_bypasses(self):
        message = self._refused("sudo id")
        self.assertIn("Refusing to run this command", message)
        self.assertIn("allow_sudo=True", message)
        self.assertIn(BASH_SUDO_BYPASS_ENV, message)
        self.assertIn("root", message)

    def test_refusal_happens_before_any_process_starts(self):
        spawn = mock.patch.object(bash_module, "BashHandle", side_effect=AssertionError("spawned"))
        spawn.start()
        self.addCleanup(spawn.stop)
        self._refused("sudo id")

    def test_doas_refused(self):
        self._refused("doas id")

    def test_widest_matching_sample_refused(self):
        for command in SUDO_MATCHING_COMMANDS:
            with self.subTest(command=command):
                self._refused(command)

    def test_lookup_and_operand_forms_allowed(self):
        instance = self._mock_handle()
        for command in ("man sudo", "command -v sudo", "which sudo", "ls /etc/sudoers.d"):
            with self.subTest(command=command):
                bash(command)
        self.assertEqual(instance.call_count, 4)

    def test_kwarg_bypass_passes_guard(self):
        instance = self._mock_handle()
        bash("sudo id", allow_sudo=True)
        instance.assert_called_once_with("sudo id")

    def test_kwarg_bypass_does_not_leak(self):
        self._mock_handle()
        bash("sudo id", allow_sudo=True)
        self._refused("sudo id")

    def test_frozen_env_bypass_honored(self):
        instance = self._mock_handle()
        with mock.patch.object(bash_module, "_SUDO_BYPASS_AT_KERNEL_START", True):
            bash("sudo id")
        instance.assert_called_once_with("sudo id")

    def test_late_bypass_env_ignored_and_warned(self):
        os.environ[BASH_SUDO_BYPASS_ENV] = "1"
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self._refused("sudo id")
            self._refused("sudo id")
        self.assertIn("PI_BASH_ALLOW_SUDO appeared after kernel start", stderr.getvalue())
        self.assertEqual(stderr.getvalue().count("appeared after kernel start"), 1)

    def test_child_env_strips_late_bypass(self):
        os.environ[BASH_SUDO_BYPASS_ENV] = "1"
        self.assertNotIn(BASH_SUDO_BYPASS_ENV, bash_module._child_env())
        with mock.patch.object(bash_module, "_SUDO_BYPASS_AT_KERNEL_START", True):
            self.assertIn(BASH_SUDO_BYPASS_ENV, bash_module._child_env())
