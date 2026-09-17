"""Async-by-default shell execution: bash() spawns immediately and returns a live handle."""

from __future__ import annotations

import asyncio
import atexit
import functools
import json
import os
import re
import secrets
import selectors
import shutil
import signal
import socket
import struct
import subprocess
import sys
import threading
import time
from collections import deque
from collections.abc import Callable, Generator
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any, cast

from . import _winjob

_IS_POSIX = os.name == "posix"

if _IS_POSIX:
    import fcntl
    import termios

_HEAD_CAP = 512 * 1024
_TAIL_CAP = 3 * 512 * 1024
_READ_CHUNK = 65536
# Fixed child-side fd for the status channel; POSIX shells (notably dash) only
# guarantee single-digit fds in redirection syntax.
_STATUS_FD = 9
_OUTPUT_FD = 8
_COMPLETION_PREFIX = b"\x1eprime-agent-complete:"
_COMPLETION_SUFFIX = b"\x1f"
# Cancelled one-shot awaits: TERM grace before the group KILL, then the bounded
# wait for a confirmed group exit before CancelledError propagates.
_CANCEL_TERM_GRACE = 0.5
_CANCEL_KILL_WAIT = 2.0
_COMPLETION_NOTICE_COMMAND_CAP = 1000
_ASYNCIO_WRAPPER_CALLBACKS = {
    ("asyncio.tasks", "gather.<locals>._done_callback"),
    ("asyncio.tasks", "shield.<locals>._inner_done_callback"),
    ("asyncio.tasks", "_wait.<locals>._on_completion"),
    ("asyncio.tasks", "as_completed.<locals>._on_completion"),
    ("asyncio.tasks", "_release_waiter"),
}

_live_handles: set["BashHandle"] = set()
_live_lock = threading.Lock()
_hook_installed = False
_hook_lock = threading.Lock()


# Privilege escalation (sudo/doas) is the one class no other guard can contain:
# a command that becomes root escapes every per-command restriction below, so it
# is refused before any process starts.
BASH_SUDO_BYPASS_ENV = "PI_BASH_ALLOW_SUDO"
# Frozen at import: the model can write os.environ mid-session, so a live read
# would let one write neuter the guard. Late writes only warn (see below).
_SUDO_BYPASS_AT_KERNEL_START = os.environ.get(BASH_SUDO_BYPASS_ENV) not in (None, "", "0")
_sudo_late_bypass_warned = False


class PrivilegeEscalationRefusalError(RuntimeError):
    """Raised when a command would run as root (or another user) via sudo/doas."""


@dataclass
class _Word:
    """One shell word/operator/redirect with its position and quote-folded value."""

    value: str
    start: int
    end: int
    kind: str = "word"
    starts_command: bool = False
    has_expansion: bool = False
    is_operand: bool = False
    is_assignment: bool = False
    is_data: bool = False
    heredoc: str | None = None
    heredoc_delim: str | None = None
    heredoc_body: str | None = None

    @property
    def is_operator(self) -> bool:
        return self.kind == "operator"

    @property
    def is_redirect(self) -> bool:
        return self.kind == "redirect"


_SUDO_COMMAND_WORDS = frozenset({"sudo", "doas"})
_WRAPPERS = frozenset(
    {
        "env",
        "nice",
        "nohup",
        "stdbuf",
        "timeout",
        "setsid",
        "ionice",
        "builtin",
        "exec",
        "busybox",
        "strace",
        "ltrace",
        "watch",
        "faketime",
        "systemd-run",
        "chroot",
    }
)
# These report on a program instead of running it, so a later sudo/doas is a name
# being looked up, not a command being escalated.
_LOOKUP_COMMANDS = frozenset({"type", "which", "whereis"})
_SHELL_RUNNERS = frozenset({"sh", "bash", "zsh", "dash"})
_PAYLOAD_RUNNERS = _SHELL_RUNNERS | frozenset({"eval", "source", "."})
# Compound-command words are syntax, not programs: skipping them lets the body's
# command word (e.g. sudo after `do`/`then`/`else`) reach the command position.
_KEYWORDS = frozenset(
    {
        "!",
        "time",
        "if",
        "then",
        "elif",
        "else",
        "fi",
        "do",
        "done",
        "while",
        "until",
        "for",
        "in",
        "case",
        "esac",
        "coproc",
    }
)
_BREAK_CHARS = frozenset(";&|()<>")
_REDIRECT_OPERATORS = ("<<<", "<<-", "<<", ">>", "<>", ">&", "<&", ">|", ">", "<")
_MAX_PAYLOAD_DEPTH = 6
_DEPTH_VIOLATION = "the payload nests deeper than the sudo scan can follow"
# Value-taking options of a wrapper: their operand is a value or (for env
# -S/--split-string) a whole command line, never the command the wrapper runs.
_WRAPPER_VALUE_OPTIONS: dict[str, frozenset[str]] = {
    "env": frozenset(
        {
            "-u",
            "--unset",
            "-C",
            "--chdir",
            "-S",
            "--split-string",
            "--block-signal",
            "--default-signal",
            "--ignore-signal",
        }
    ),
    "timeout": frozenset({"-s", "--signal", "-k", "--kill-after"}),
    "stdbuf": frozenset({"-i", "--input", "-o", "--output", "-e", "--error"}),
    "ionice": frozenset(
        {"-c", "--class", "-n", "--classdata", "-p", "--pid", "-P", "--pgid", "-u", "--uid"}
    ),
    "nice": frozenset({"-n", "--adjustment"}),
    "exec": frozenset({"-a", "--argv0"}),
    # -D, -f, -i, -q, -t, -T, -x, -y, -c, -C, -n, -v, -V are boolean in strace.
    "strace": frozenset(
        {
            "-a",
            "-b",
            "-e",
            "-I",
            "-o",
            "-O",
            "-p",
            "-P",
            "-s",
            "-S",
            "-u",
            "-U",
            "-X",
            "--attach",
            "--columns",
            "--output",
            "--trace",
        }
    ),
    # -L, -i, -q, -f, -c, -S, -T are boolean in ltrace; -n takes a value here.
    "ltrace": frozenset(
        {
            "-A",
            "-a",
            "-d",
            "-D",
            "-e",
            "-F",
            "-l",
            "-n",
            "-o",
            "-p",
            "-s",
            "-u",
            "-w",
            "-x",
            "--output",
        }
    ),
    # -d and -t are boolean in watch: only the interval takes a value.
    "watch": frozenset({"-n", "--interval"}),
    "faketime": frozenset({"-f", "-m", "-p"}),
    "chroot": frozenset({"--userspec", "--groups"}),
    "systemd-run": frozenset(
        {
            "-u",
            "-p",
            "-E",
            "-C",
            "-M",
            "--capsule",
            "--unit",
            "--property",
            "--setenv",
            "--machine",
            "--working-directory",
            "--slice",
            "--description",
            "--nice",
        }
    ),
}
# Wrappers whose first non-flag operands are values, not the command they run.
_WRAPPER_LEADING_OPERANDS: dict[str, int] = {"chroot": 1, "faketime": 1}
# xargs options whose operand is a value, not the command xargs runs.
_XARGS_OPERAND_OPTIONS = frozenset(
    {
        "-I",
        "--replace",
        "-n",
        "--max-args",
        "-a",
        "--arg-file",
        "-d",
        "--delimiter",
        "-E",
        "--eof",
        "-L",
        "--max-lines",
        "-P",
        "--max-procs",
        "-s",
        "--max-chars",
        "-J",
        "--process-slot-var",
    }
)
_FIND_EXEC_FLAGS = frozenset({"-exec", "-execdir", "-ok", "-okdir"})
_FD_EXEC_FLAGS = frozenset({"-x", "--exec", "-X", "--exec-batch"})
# Launchers whose `exec` flag hands the following words to a command.
_EXEC_LAUNCHER_FLAGS: dict[str, frozenset[str]] = {
    "find": _FIND_EXEC_FLAGS,
    "fd": _FD_EXEC_FLAGS,
    "fdfind": _FD_EXEC_FLAGS,
}
# Letters of the short flags above: a bundle such as `env -vu NAME` still starts
# with a value-taking letter, so the walk must consume its operand there too.
_WRAPPER_VALUE_LETTERS: dict[str, str] = {
    "env": "uCS",
    "timeout": "sk",
    "stdbuf": "ioe",
    "ionice": "cnpPu",
    "nice": "n",
    "exec": "a",
    "strace": "oepsaubIPOUXS",
    "ltrace": "oepsluaFAwnDxd",
    "watch": "n",
    "faketime": "fmp",
    "chroot": "",
    "systemd-run": "upEMC",
}
_XARGS_OPERAND_LETTERS = "InadELPsJ"
_PARALLEL_OPERAND_OPTIONS = frozenset(
    {
        "-j",
        "-N",
        "-n",
        "-L",
        "-S",
        "-a",
        "-I",
        "--jobs",
        "--max-args",
        "--max-lines",
        "--sshlogin",
        "--ssh",
        "--joblog",
        "--results",
        "--tmpdir",
        "--colsep",
        "--arg-file",
    }
)
# Launchers that run their first non-flag word as a command, like xargs.
_LAUNCHER_OPERAND_OPTIONS: dict[str, frozenset[str]] = {
    "xargs": _XARGS_OPERAND_OPTIONS,
    "parallel": _PARALLEL_OPERAND_OPTIONS,
}
_LAUNCHER_OPERAND_LETTERS: dict[str, str] = {"xargs": _XARGS_OPERAND_LETTERS, "parallel": "jNnLSaI"}
_BRACE_EXPANSION_CAP = 64
_HEX_DIGITS = frozenset("0123456789abcdefABCDEF")
_ANSI_C_ESCAPES = {
    "a": "\a",
    "b": "\b",
    "e": "\x1b",
    "E": "\x1b",
    "f": "\f",
    "n": "\n",
    "r": "\r",
    "t": "\t",
    "v": "\v",
    "\\": "\\",
    "'": "'",
    '"': '"',
    "?": "?",
}


def _join_line_continuations(command: str) -> str:
    """Drop backslash-newline pairs the way the shell does (not inside single quotes)."""
    out: list[str] = []
    single = False
    double = False
    index = 0
    length = len(command)
    while index < length:
        char = command[index]
        following = command[index + 1] if index + 1 < length else ""
        if char == "\\" and following == "\n":
            if not single:
                index += 2
                continue
            out.append(char)
            index += 1
            continue
        if char == "\\" and following and not single:
            # Keep the escaped pair intact: the tokenizer folds it later.
            out.append(char + following)
            index += 2
            continue
        if char == "'" and not double:
            single = not single
        elif char == '"' and not single:
            double = not double
        out.append(char)
        index += 1
    return "".join(out)


def _match_redirect(text: str, start: int) -> tuple[str, int] | None:
    """Redirect operator at `start` (optional leading fd digits), or None."""
    index = start
    while index < len(text) and text[index].isdigit():
        index += 1
    for operator in _REDIRECT_OPERATORS:
        if text.startswith(operator, index):
            return operator, index + len(operator)
    return None


def _is_assignment(value: str) -> bool:
    name, separator, _ = value.partition("=")
    if not separator:
        return False
    if name.endswith("+"):
        name = name[:-1]
    if not name or not (name[0].isalpha() or name[0] == "_"):
        return False
    return all(char.isalnum() or char == "_" for char in name)


def _code_point_char(code: int) -> str | None:
    """Character for a decoded code point, or None when it is not a valid one."""
    if code < 0 or code > 0x10FFFF or 0xD800 <= code <= 0xDFFF:
        return None
    return chr(code)


def _read_ansi_c(command: str, quote_index: int) -> tuple[str, int]:
    """Decode `$'...'` text from its opening quote; return the text and the next index."""
    out: list[str] = []
    index = quote_index + 1
    length = len(command)
    while index < length:
        char = command[index]
        if char == "'":
            return "".join(out), index + 1
        if char != "\\" or index + 1 >= length:
            out.append(char)
            index += 1
            continue
        code = command[index + 1]
        if code in _ANSI_C_ESCAPES:
            out.append(_ANSI_C_ESCAPES[code])
            index += 2
            continue
        if code in "01234567":
            digits = ""
            cursor = index + 1
            while cursor < length and len(digits) < 3 and command[cursor] in "01234567":
                digits += command[cursor]
                cursor += 1
            decoded = _code_point_char(int(digits, 8) & 0xFF)
            if decoded is not None:
                out.append(decoded)
                index = cursor
                continue
        if code == "x":
            digits = ""
            cursor = index + 2
            while cursor < length and len(digits) < 2 and command[cursor] in _HEX_DIGITS:
                digits += command[cursor]
                cursor += 1
            decoded = _code_point_char(int(digits, 16)) if digits else None
            if decoded is not None:
                out.append(decoded)
                index = cursor
                continue
        if code in ("u", "U"):
            width = 4 if code == "u" else 8
            digits = command[index + 2 : index + 2 + width]
            if len(digits) == width and all(digit in _HEX_DIGITS for digit in digits):
                decoded = _code_point_char(int(digits, 16))
                if decoded is not None:
                    out.append(decoded)
                    index += 2 + width
                    continue
        if code == "c":
            control = command[index + 2 : index + 3]
            decoded = _code_point_char(ord(control.upper()) ^ 0x40) if control else None
            if decoded is not None and control != "\\":
                out.append(decoded)
                index += 3
                continue
        # Unknown or out-of-range escape: keep the backslash and the character.
        out.append(char)
        index += 1
    return "".join(out), index


def _tokenize(command: str) -> list[_Word]:
    """Split shell text into words, operators, and redirects, folding quotes."""
    words: list[_Word] = []
    buffer: list[str] = []
    started = False
    expansion = False
    single = False
    double = False
    segment_start = True
    operand_next = False
    word_start = 0
    index = 0
    length = len(command)

    def flush() -> None:
        nonlocal buffer, started, expansion, segment_start, operand_next, word_start
        if not started:
            return
        value = "".join(buffer)
        words.append(
            _Word(
                value=value,
                start=word_start,
                end=index,
                starts_command=segment_start,
                has_expansion=expansion,
                is_operand=operand_next,
            )
        )
        buffer = []
        started = False
        expansion = False
        segment_start = False
        operand_next = False
        word_start = index

    def note_character() -> None:
        nonlocal started, word_start
        if not started:
            started = True
            word_start = index

    while index < length:
        char = command[index]
        if single:
            if char == "'":
                single = False
            else:
                note_character()
                buffer.append(char)
            index += 1
            continue
        if double:
            if char == '"':
                double = False
                index += 1
                continue
            note_character()
            if char in "$`":
                expansion = True
            buffer.append(char)
            index += 1
            continue
        if char == "\\" and index + 1 < length:
            note_character()
            buffer.append(command[index + 1])
            index += 2
            continue
        if char == "'":
            note_character()
            single = True
            index += 1
            continue
        if char == '"':
            note_character()
            double = True
            index += 1
            continue
        if char == "$" and index + 1 < length and command[index + 1] in "'\"":
            note_character()
            if command[index + 1] == "'":
                # $'...' is ANSI-C text: decode it so escapes cannot hide a name.
                text, index = _read_ansi_c(command, index + 1)
                buffer.append(text)
                if any(escaped in "$`" for escaped in text):
                    expansion = True
            else:
                double = True  # $"..." folds like double quotes
                index += 2
            continue
        if char == "#" and not started:
            while index < length and command[index] != "\n":
                index += 1
            continue
        if char in "\t\n " or char in "\r\v\f":
            flush()
            if char == "\n":
                segment_start = True
            index += 1
            continue
        if char in "<>" and command.startswith("(", index + 1):
            # Process substitution (`<(cmd)`, `>(cmd)`) runs the span as a command
            # of its own. Keep it in one word so the span scan recurses into it.
            note_character()
            end = _matching_paren(command, index + 1) + 1
            expansion = True
            buffer.append(command[index:end])
            index = end
            continue
        redirect = None
        if started or char.isdigit() or char in "<>":
            redirect = _match_redirect(command, index)
        if redirect is not None:
            flush()
            if not started:
                word_start = index
            operator, after = redirect
            target_end = after
            quote: str | None = None
            while target_end < length:
                char = command[target_end]
                if quote is None:
                    if char.isspace() or char in _BREAK_CHARS:
                        break
                    if char in "'\"":
                        quote = char
                elif char == quote:
                    quote = None
                # A quoted target is one word: `bash<<<"sh -c 'sudo id'"` is a script.
                target_end += 1
            target = command[after:target_end]
            heredoc = operator.startswith("<<")
            words.append(
                _Word(
                    value=command[index:after] + target,
                    start=index,
                    end=target_end,
                    kind="redirect",
                    starts_command=segment_start,
                    is_operand=operand_next,
                    heredoc=operator if heredoc else None,
                    heredoc_delim=_strip_quotes(target) if heredoc and target else None,
                )
            )
            segment_start = False
            operand_next = not target
            index = target_end
            continue
        if char in _BREAK_CHARS:
            flush()
            operator_text = char
            if char in "&|" and command[index : index + 2] == char * 2:
                operator_text = char * 2
            kind = "operator"
            words.append(
                _Word(
                    value=operator_text,
                    start=index,
                    end=index + len(operator_text),
                    kind=kind,
                )
            )
            segment_start = True
            index += len(operator_text)
            continue
        note_character()
        if char in "$`":
            expansion = True
        buffer.append(char)
        index += 1
    flush()
    _classify_words(words)
    return words


def _strip_quotes(value: str) -> str:
    return value.strip("\"'")


def _classify_words(words: list[_Word]) -> None:
    """Mark assignments and standalone group braces; `}` starts the next command."""
    for index, word in enumerate(words):
        if word.kind != "word":
            continue
        if word.value in ("{", "}"):
            word.kind = "group"
            if index + 1 < len(words):
                words[index + 1].starts_command = True
        elif _is_assignment(word.value):
            word.is_assignment = True


def _apply_heredocs(text: str, words: list[_Word]) -> None:
    """Resolve heredoc delimiters and mark body words as data (not commands)."""
    for index, word in enumerate(words):
        if not word.heredoc or word.heredoc == "<<<" or word.heredoc_delim:
            continue
        if index + 1 < len(words):
            word.heredoc_delim = _strip_quotes(words[index + 1].value)
    for word in words:
        if not word.heredoc or word.heredoc == "<<<" or not word.heredoc_delim:
            continue
        newline = text.find("\n", word.end)
        if newline < 0:
            continue
        body_start = newline + 1
        cursor = body_start
        while cursor <= len(text):
            line_end = text.find("\n", cursor)
            line_end = len(text) if line_end < 0 else line_end
            if text[cursor:line_end].strip() == word.heredoc_delim:
                break
            cursor = line_end + 1
        else:
            cursor = len(text)
        word.heredoc_body = text[body_start:cursor]
        for other in words:
            # Only the body itself is data; the rest of the command line still runs.
            if body_start <= other.start < cursor:
                other.is_data = True


def _is_flag_word(word: _Word) -> bool:
    return word.value.startswith("-") and word.value != "-"


def _matching_paren(value: str, open_index: int) -> int:
    """Index of the `)` matching the `(` at `open_index`, skipping quoted spans."""
    depth = 0
    single = False
    double = False
    index = open_index
    while index < len(value):
        char = value[index]
        if char == "\\" and not single and index + 1 < len(value):
            index += 2  # an escaped character cannot open or close a quote
            continue
        if single:
            if char == "'":
                single = False
        elif double:
            if char == '"':
                double = False
        elif char == "'":
            single = True
        elif char == '"':
            double = True
        elif char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth == 0:
                return index
        index += 1
    return len(value)


def _expansion_spans(value: str) -> list[str]:
    """`$(...)` and backtick spans of an expansion word, as command text."""
    spans: list[str] = []
    index = 0
    while index < len(value):
        char = value[index]
        if char == "$" and value.startswith("$(", index):
            end = _matching_paren(value, index + 1)
            spans.append(value[index + 2 : end])
            index = end + 1 if end < len(value) else end
            continue
        if char in "<>" and value.startswith("(", index + 1):
            end = _matching_paren(value, index + 1)
            spans.append(value[index + 2 : end])
            index = end + 1 if end < len(value) else end
            continue
        if char == "`":
            end = value.find("`", index + 1)
            if end < 0:
                break
            spans.append(value[index + 1 : end])
            index = end + 1
            continue
        index += 1
    return spans


def _scan_expansion(value: str, depth: int, parent_mentions_sudo: bool = False) -> str | None:
    """Scan substitution spans inside a word: they run as commands of their own."""
    if depth >= _MAX_PAYLOAD_DEPTH:
        return _DEPTH_VIOLATION
    for span in _expansion_spans(value):
        violation = _scan_text(span, depth + 1, parent_mentions_sudo)
        if violation:
            return violation
    return None


def _scan_text(text: str, depth: int, parent_mentions_sudo: bool = False) -> str | None:
    words = _tokenize(text)
    _apply_heredocs(text, words)
    inner = parent_mentions_sudo or _mentions_sudo(words)
    # Word indices the walk reaches as command words, so the heredoc gate below
    # uses the same judgment as the refusals instead of guessing from the tokens.
    command_words: set[int] = set()
    for word in words:
        if word.is_data or not word.has_expansion:
            continue
        violation = _scan_expansion(word.value, depth, inner)
        if violation:
            return violation
    for index, word in enumerate(words):
        if word.is_data or not word.starts_command:
            continue
        violation = _scan_segment(words, index, depth, inner, command_words)
        if violation:
            return violation
    # A heredoc body that the same text feeds to a runner is a script, not data.
    runner_alias = _alias_body_names_runner(words, command_words)
    if depth < _MAX_PAYLOAD_DEPTH and (
        runner_alias
        or any(
            os.path.basename(words[position].value) in _PAYLOAD_RUNNERS
            for position in command_words
        )
    ):
        for word in words:
            if word.is_data or not word.heredoc_body:
                continue
            violation = _scan_text(word.heredoc_body, depth + 1, inner)
            if violation:
                return violation
    if runner_alias and depth < _MAX_PAYLOAD_DEPTH:
        # An alias whose body names a runner can still read a redirect as its
        # script, so judge this text's redirects the way a runner's own are judged.
        return _scan_script_source(words, list(range(len(words))), depth, inner)
    return None


def _scan_segment(
    words: list[_Word],
    start: int,
    depth: int,
    parent_mentions_sudo: bool = False,
    command_words: set[int] | None = None,
) -> str | None:
    """Walk one command segment to its command word and judge that word."""
    while start < len(words):
        word = words[start]
        if word.is_operator:
            return None
        if word.is_data or word.is_redirect or word.is_operand or word.is_assignment:
            start += 1
            continue
        if word.value in ("for", "select"):
            start = _skip_loop_header(words, start + 1)
            continue
        if word.value == "case":
            start = _skip_case_header(words, start + 1)
            continue
        if word.kind == "group" or word.value in _KEYWORDS:
            start += 1
            continue
        name = os.path.basename(word.value)
        if name in _LOOKUP_COMMANDS:
            return None  # `which sudo`, `type sudo`: operands are just names
        if name == "command":
            # `command` only defeats functions and aliases; it is a lookup with
            # -v/-V, and otherwise it still runs the next word.
            start += 1
            lookup = False
            while start < len(words) and _is_flag_word(words[start]):
                if "v" in words[start].value or "V" in words[start].value:
                    lookup = True
                start += 1
            if lookup:
                return None
            continue
        if name in _WRAPPERS:
            start, violation = _skip_wrapper_operands(
                words, start + 1, name, depth, parent_mentions_sudo
            )
            if violation:
                return violation
            continue
        if command_words is not None:
            command_words.add(start)
        if _word_names_sudo(word.value):
            return f"{name} would run this command as root or another user"
        if name == "alias":
            return _scan_alias_bodies(words, start + 1, depth, parent_mentions_sudo)
        if name in _EXEC_LAUNCHER_FLAGS:
            return _scan_find_execs(
                words,
                start + 1,
                depth,
                parent_mentions_sudo,
                command_words,
                _EXEC_LAUNCHER_FLAGS[name],
            )
        if word.has_expansion:
            if _mentions_sudo(words) or parent_mentions_sudo:
                return (
                    "the command position expands to an unknown program while the "
                    "text invokes sudo/doas"
                )
            return None
        return _scan_interpreter(words, start, depth, parent_mentions_sudo, command_words)
    return None


def _is_duration(value: str) -> bool:
    digits = value[:-1] if value[-1:].isalpha() else value
    return bool(digits) and digits.isdigit()


def _skip_loop_header(words: list[_Word], start: int) -> int:
    """Index after a `for`/`select` header: the loop variable and `in` list are names."""
    while start < len(words):
        word = words[start]
        if word.is_operator or word.value in ("do", "done"):
            break
        start += 1
    return start


def _skip_case_header(words: list[_Word], start: int) -> int:
    """Index of the `)` that closes the first `case` label list: subject and labels are names."""
    while start < len(words):
        word = words[start]
        if word.is_operator:
            if word.value == ")":
                break
            if word.value not in ("(", "|"):
                break
        elif word.value == "esac":
            break
        start += 1
    return start


def _ends_segment(word: _Word) -> bool:
    return word.is_operator or word.is_redirect or word.is_data or word.is_operand


def _skip_wrapper_operands(
    words: list[_Word],
    index: int,
    wrapper: str,
    depth: int,
    parent_mentions_sudo: bool = False,
) -> tuple[int, str | None]:
    """Index after a wrapper's own operands, plus any violation their text carries."""
    value_options = _WRAPPER_VALUE_OPTIONS.get(wrapper, frozenset())
    value_letters = _WRAPPER_VALUE_LETTERS.get(wrapper, "")
    leading = _WRAPPER_LEADING_OPERANDS.get(wrapper, 0)
    while index < len(words):
        word = words[index]
        if word.is_operator or word.is_redirect or word.is_data:
            break
        if word.is_assignment:
            index += 1
            continue
        if _is_flag_word(word):
            option, glued = _split_option(word.value, value_options, value_letters)
            if option is None:
                index += 1
                continue
            # env -S/--split-string takes a whole command line, so its operand is
            # text a shell runs; every other value operand is just a value.
            split_string = wrapper == "env" and option in ("-S", "--split-string")
            if glued is None:
                operand = index + 1
                if split_string and operand < len(words) and not _ends_segment(words[operand]):
                    violation = _scan_text(words[operand].value, depth + 1, parent_mentions_sudo)
                    if violation:
                        return index, violation
                index += 2
                continue
            if split_string:
                violation = _scan_text(glued, depth + 1, parent_mentions_sudo)
                if violation:
                    return index, violation
            index += 1
            continue
        if wrapper in ("nice", "timeout") and _is_duration(word.value):
            index += 1
            continue
        if leading:
            # A positional operand such as chroot's NEWROOT or faketime's
            # timestamp: consume it and keep walking, so later flags still cannot
            # swallow the command.
            leading -= 1
            index += 1
            continue
        break
    return index, None


def _mentions_sudo(words: list[_Word]) -> bool:
    """True when any word (or assignment value) names sudo/doas, obfuscations included."""
    for word in words:
        if word.is_operator:
            continue
        candidates = [word.value]
        if word.is_assignment:
            candidates.append(word.value.partition("=")[2].lstrip("+"))
        for candidate in candidates:
            if _word_names_sudo(candidate):
                return True
    return False


def _word_names_sudo(value: str) -> bool:
    """True when a command word can name sudo/doas: braces, globs, and letters."""
    alternatives = _brace_alternatives(value)
    if alternatives is None:
        return True  # too many alternatives to enumerate: fail closed
    for candidate in alternatives:
        if os.path.basename(candidate) in _SUDO_COMMAND_WORDS:
            return True
        if _matches_sudo_pattern(candidate):
            return True
        letters = "".join(char for char in candidate if char.isalpha()).lower()
        if "sudo" in letters or "doas" in letters:
            return True
    return False


def _matches_sudo_pattern(value: str) -> bool:
    """True when the glob pattern in a word matches the name sudo or doas."""
    if not any(char in value for char in "*?["):
        return False
    pattern: list[str] = []
    index = 0
    while index < len(value):
        char = value[index]
        if char == "*":
            pattern.append(".*")
        elif char == "?":
            pattern.append(".")
        elif char == "[":
            end = value.find("]", index + 1)
            if end < 0:
                pattern.append("\\[")
            else:
                body = value[index + 1 : end]
                if body.startswith("!"):
                    body = "^" + body[1:]
                pattern.append("[" + body + "]")
                index = end
        else:
            pattern.append(re.escape(char))
        index += 1
    try:
        compiled = re.compile("".join(pattern))
    except re.error:
        return False
    return any(compiled.fullmatch(name) for name in _SUDO_COMMAND_WORDS)


def _is_integer(value: str) -> bool:
    return value.lstrip("-").isdigit() and value.lstrip("-") != ""


def _sequence_elements(body: str) -> tuple[int, list[str] | None] | None:
    """Elements of a `x..y`/`x..y..step` brace sequence as (count, elements), else None.

    The count is computed arithmetically first, so an oversized range such as
    `{1..9999999}` fails closed without ever building its elements.
    """
    parts = body.split("..")
    if len(parts) not in (2, 3):
        return None
    start_text, end_text = parts[0], parts[1]
    step_text = parts[2].strip() if len(parts) == 3 else "1"
    if not _is_integer(step_text) or int(step_text) == 0:
        return None
    step = abs(int(step_text))
    numeric = _is_integer(start_text) and _is_integer(end_text)
    if numeric:
        low, high = int(start_text), int(end_text)
    elif len(start_text) == 1 and len(end_text) == 1:
        low, high = ord(start_text), ord(end_text)
    else:
        return None
    if low > high:
        step = -step
    count = (high - low) // step + 1
    if count > _BRACE_EXPANSION_CAP:
        return count, None  # over the cap: fail closed, unbuilt
    bounds = range(low, high + step, step)
    if numeric:
        return count, [str(number) for number in bounds]
    return count, [chr(code) for code in bounds]


def _brace_group_elements(body: str) -> tuple[int, list[str] | None] | None:
    """Elements of an expanding brace group as (count, elements), or None when it stays literal.

    `elements` is None when the group holds more than the cap, so the caller can fail
    closed without building the product.
    """
    sequence = _sequence_elements(body)
    if sequence is not None:
        return sequence
    parts = _top_level_split(body)
    if parts is None:
        return 0, None  # over the cap: `_top_level_split` stopped early
    if len(parts) < 2:
        return None  # `su{d}o` has no comma, so bash does not expand it
    return len(parts), parts


def _brace_group_chain(value: str) -> tuple[list[tuple[str, int, list[str] | None]], str] | None:
    """Left-to-right expanding brace groups of a word, with the literal tail."""
    chain: list[tuple[str, int, list[str] | None]] = []
    tail = value
    while True:
        group = _first_brace_group(tail)
        if group is None:
            break
        prefix, body, suffix = group
        count, elements = _brace_group_elements(body)
        chain.append((prefix, count, elements))
        tail = suffix
    return (chain, tail) if chain else None


def _brace_alternatives(value: str) -> list[str] | None:
    """Brace-expansion candidates of a word, or None when they exceed the cap."""
    if value.count("{") > _BRACE_EXPANSION_CAP:
        return None  # a brace flood: more groups than the cap enumerates, fail closed
    chain = _brace_group_chain(value)
    if chain is None:
        return [value]
    groups, tail = chain
    total = 1
    for _prefix, count, elements in groups:
        if elements is None or count > _BRACE_EXPANSION_CAP:
            return None
        total *= count
        if total > _BRACE_EXPANSION_CAP:
            return None
    expanded = [""]
    for prefix, _count, elements in groups:
        assert elements is not None
        expanded = [candidate + prefix + element for candidate in expanded for element in elements]
    return [candidate + tail for candidate in expanded]


def _first_brace_group(value: str) -> tuple[str, str, str] | None:
    """Leftmost expanding brace group, as (prefix, body, suffix), else None.

    One pass with a stack of open groups matches every `{` to its `}` once, so a
    brace-heavy word stays linear instead of rescanning the tail for each `{`.
    """
    open_groups: list[int] = []
    groups: list[tuple[int, int]] = []
    for index, char in enumerate(value):
        if char == "{":
            open_groups.append(index)
        elif char == "}" and open_groups:
            groups.append((open_groups.pop(), index))
    for start, end in sorted(groups):
        if _brace_group_elements(value[start + 1 : end]) is not None:
            return value[:start], value[start + 1 : end], value[end + 1 :]
    return None


def _top_level_split(body: str) -> list[str] | None:
    """Split a brace group body on its top-level commas, or None past the cap."""
    parts: list[str] = []
    current: list[str] = []
    depth = 0
    for char in body:
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
        if char == "," and depth == 0:
            parts.append("".join(current))
            if len(parts) > _BRACE_EXPANSION_CAP:
                return None  # stop early instead of materialising a huge group
            current = []
            continue
        current.append(char)
    parts.append("".join(current))
    return parts


def _alias_body(word: _Word) -> str | None:
    """Value half of an `alias NAME=BODY` operand, else None."""
    if word.is_data or word.is_redirect or "=" not in word.value:
        return None
    return word.value.partition("=")[2].lstrip("+") or None


def _body_reaches_runner(body: str) -> bool:
    """True when a runner is a command word of the body, wrapper chains included."""
    words = _tokenize(body)
    _apply_heredocs(body, words)
    reached: set[int] = set()
    for index, word in enumerate(words):
        if word.is_data or not word.starts_command:
            continue
        _scan_segment(words, index, 0, False, reached)
    return any(os.path.basename(words[index].value) in _PAYLOAD_RUNNERS for index in reached)


def _alias_body_names_runner(words: list[_Word], command_words: set[int]) -> bool:
    """True when an alias defined in this text runs a payload runner as its command."""
    for index in command_words:
        if os.path.basename(words[index].value) != "alias":
            continue
        for candidate in _segment_tail(words, index + 1):
            body = _alias_body(words[candidate])
            if body is not None and _body_reaches_runner(body):
                return True
    return False


def _split_option(
    value: str, options: frozenset[str], value_letters: str = ""
) -> tuple[str | None, str | None]:
    """Match a flag word against an option set: (option, glued operand) or (None, None).

    A short bundle (`env -vu NAME`, `xargs -rn 2`) is scanned for a value-taking
    letter: letters before it are plain flags, letters after it are the glued operand.
    """
    if value in options:
        return value, None
    if value.startswith("--") and "=" in value:
        option, _, glued = value.partition("=")
        return (option, glued) if option in options else (None, None)
    if len(value) > 2 and value[0] == "-" and value[1] != "-":
        offset = next(
            (index for index, char in enumerate(value[1:]) if char in value_letters), None
        )
        if offset is None:
            return None, None
        return "-" + value[1 + offset], value[2 + offset :] or None
    return None, None


def _process_substitution_body(value: str) -> str | None:
    """Inner command text of a `<(cmd)` process substitution, else None."""
    if not value.startswith("<("):
        return None
    return value[2 : _matching_paren(value, 1)]


def _is_command_flag(value: str) -> bool:
    """`-c`, a bundled flag containing c (e.g. `-lc`), or `--command`."""
    if value == "--command":
        return True
    return len(value) >= 2 and value[0] == "-" and "c" in value[1:] and value[1:].isalpha()


def _glued_payload(value: str) -> str | None:
    """Payload folded onto the flag word itself (`-c$'sudo id'` -> `-csudo id`)."""
    if not value.startswith("-") or "c" not in value[1:]:
        return None
    return value[value.index("c") + 1 :] or None


def _scan_interpreter(
    words: list[_Word],
    index: int,
    depth: int,
    parent_mentions_sudo: bool = False,
    command_words: set[int] | None = None,
) -> str | None:
    """Judge payloads a runner executes: shell -c, eval, xargs operands, heredocs."""
    name = os.path.basename(words[index].value)
    if name not in _PAYLOAD_RUNNERS and name not in _LAUNCHER_OPERAND_OPTIONS:
        return None
    if depth >= _MAX_PAYLOAD_DEPTH:
        return _DEPTH_VIOLATION
    following = _segment_tail(words, index + 1)
    if name in _LAUNCHER_OPERAND_OPTIONS:
        return _scan_xargs(words, following, depth, parent_mentions_sudo, command_words, name)
    if name in _SHELL_RUNNERS:
        for offset, candidate in enumerate(following):
            word = words[candidate]
            if word.is_data or word.is_redirect:
                break
            command_flag = _is_command_flag(word.value)
            if command_flag and offset + 1 < len(following):
                payload = words[following[offset + 1]]
                violation = _scan_text(
                    _strip_quotes(payload.value), depth + 1, parent_mentions_sudo
                )
                if violation:
                    return violation
            glued = _glued_payload(word.value)
            if glued:
                violation = _scan_text(glued, depth + 1, parent_mentions_sudo)
                if violation:
                    return violation
                break
            if command_flag:
                break
    if name == "eval":
        joined = " ".join(words[i].value for i in following if not words[i].is_data)
        if joined:
            violation = _scan_text(joined, depth + 1, parent_mentions_sudo)
            if violation:
                return violation
    violation = _scan_script_source(words, following, depth, parent_mentions_sudo)
    if violation:
        return violation
    # A heredoc body owned by a runner is a script, not data.
    for candidate in following:
        body = words[candidate].heredoc_body
        if body:
            violation = _scan_text(body, depth + 1, parent_mentions_sudo)
            if violation:
                return violation
    return None


def _scan_xargs(
    words: list[_Word],
    following: list[int],
    depth: int,
    parent_mentions_sudo: bool,
    command_words: set[int] | None = None,
    launcher: str = "xargs",
) -> str | None:
    """xargs/parallel run their first non-flag word; option operands are judged fail-closed."""
    operand_options = _LAUNCHER_OPERAND_OPTIONS.get(launcher, _XARGS_OPERAND_OPTIONS)
    operand_letters = _LAUNCHER_OPERAND_LETTERS.get(launcher, _XARGS_OPERAND_LETTERS)
    position = 0
    while position < len(following):
        word = words[following[position]]
        if word.is_data or word.is_redirect:
            return None
        if not _is_flag_word(word):
            return _scan_segment(
                words, following[position], depth, parent_mentions_sudo, command_words
            )
        option, glued = _split_option(word.value, operand_options, operand_letters)
        if option is not None and glued is None and position + 1 < len(following):
            # BSD and GNU disagree on which operands are optional, so the operand
            # is judged as a command either way.
            operand = words[following[position + 1]]
            if not operand.is_data and not operand.is_redirect:
                violation = _scan_segment(
                    words, following[position + 1], depth, parent_mentions_sudo, command_words
                )
                if violation:
                    return violation
            position += 2
            continue
        position += 1
    return None


def _scan_script_source(
    words: list[_Word], following: list[int], depth: int, parent_mentions_sudo: bool
) -> str | None:
    """Judge a runner's script given as a redirect: `<<<` text or `<(cmd)` output."""
    for candidate in following:
        word = words[candidate]
        if word.heredoc == "<<<":
            # The payload can be attached to the operator (`bash<<<'sudo id'`) or
            # be the next word (`bash <<< 'sudo id'`); both are the script.
            sources = [word.value[3:]] if word.value[3:] else []
            operand = candidate + 1
            if operand < len(words) and not words[operand].is_data:
                sources.append(words[operand].value)
            for source in sources:
                violation = _scan_text(_strip_quotes(source), depth + 1, parent_mentions_sudo)
                if violation:
                    return violation
            continue
        if word.is_data:
            continue
        body = _process_substitution_body(word.value)
        if body and (parent_mentions_sudo or _mentions_sudo(_tokenize(body))):
            return (
                "the shell reads its script from a process substitution whose "
                "text invokes sudo/doas"
            )
    return None


def _scan_alias_bodies(
    words: list[_Word], start: int, depth: int, parent_mentions_sudo: bool = False
) -> str | None:
    """An alias body is text that a later use of the alias runs."""
    for candidate in _segment_tail(words, start):
        body = _alias_body(words[candidate])
        if body is None:
            continue
        violation = _scan_text(body, depth + 1, parent_mentions_sudo)
        if violation:
            return violation
    return None


def _scan_find_execs(
    words: list[_Word],
    start: int,
    depth: int,
    parent_mentions_sudo: bool,
    command_words: set[int] | None = None,
    flags: frozenset[str] = _FIND_EXEC_FLAGS,
) -> str | None:
    """`find -exec cmd` (and `fd -x cmd`) runs cmd, so its operand is judged as a command."""
    tail = _segment_tail(words, start)
    for offset, candidate in enumerate(tail):
        if words[candidate].value not in flags or offset + 1 >= len(tail):
            continue
        operand = words[tail[offset + 1]]
        if operand.is_data or operand.is_redirect:
            continue
        violation = _scan_segment(
            words, tail[offset + 1], depth, parent_mentions_sudo, command_words
        )
        if violation:
            return violation
    return None


def _segment_tail(words: list[_Word], start: int) -> list[int]:
    """Indices of the words after the command word, up to the segment boundary."""
    indices: list[int] = []
    for index in range(start, len(words)):
        if words[index].is_operator:
            break
        indices.append(index)
    return indices
def _sudo_violation(command: str) -> str | None:
    """Reason phrase when the text invokes sudo/doas as a command, else None."""
    return _scan_text(_join_line_continuations(command), 0)


def _format_sudo_refusal(violation: str) -> str:
    return (
        f"Refusing to run this command: {violation}. sudo and doas run the command as "
        "root (or another user), which escapes the containment every other guard "
        "relies on; on a passwordless-sudo setup the escalation is silent. "
        "Bypass deliberately, so the intent stays visible in the transcript: "
        "call bash(command, allow_sudo=True), or start the kernel with "
        f"{BASH_SUDO_BYPASS_ENV}=1."
    )


def _warn_once_about_late_sudo_bypass() -> None:
    global _sudo_late_bypass_warned
    if _sudo_late_bypass_warned:
        return
    value = os.environ.get(BASH_SUDO_BYPASS_ENV)
    if value is None or value in ("", "0"):
        return
    _sudo_late_bypass_warned = True
    print(
        f"prime-agent bash: {BASH_SUDO_BYPASS_ENV} appeared after kernel start and is "
        "ignored; the sudo guard only honors it when the kernel is started with it set.",
        file=sys.stderr,
        flush=True,
    )


def _guard_sudo(command: str, allow_sudo: bool) -> None:
    """String-only scan for sudo/doas before any spawn; refusals never start a process."""
    if allow_sudo or _SUDO_BYPASS_AT_KERNEL_START:
        return
    violation = _sudo_violation(_with_prefix(command))
    if violation is None:
        return
    _warn_once_about_late_sudo_bypass()
    raise PrivilegeEscalationRefusalError(_format_sudo_refusal(violation))


def _current_cell_completion_context() -> tuple[asyncio.Event, asyncio.Task[Any] | None] | None:
    """Get the creating REPL cell's lifecycle without coupling standalone use to repl."""
    try:
        from . import repl

        if repl.is_active():
            return repl.current_cell_completion_context()
    except (ImportError, RuntimeError):
        pass
    return None


def _consume_notice_task(task: asyncio.Task[None]) -> None:
    """Retrieve detached notifier failures so they never become loop warnings."""
    if not task.cancelled():
        task.exception()


def _completion_reaches(
    start: asyncio.Future[Any], targets: tuple[asyncio.Future[Any], ...]
) -> bool:
    """Follow asyncio's wrapper and TaskGroup ownership callbacks."""
    pending = [start]
    seen_futures: set[int] = set()
    seen_values: set[int] = set()

    def collect(value: Any, depth: int = 0) -> None:
        if isinstance(value, asyncio.Future):
            pending.append(value)
            return
        identity = id(value)
        if depth >= 4 or identity in seen_values:
            return
        seen_values.add(identity)

        nested: list[Any] = []
        if isinstance(value, asyncio.Queue):
            pending.extend(value._getters)
        elif isinstance(value, functools.partial):
            nested.extend((value.func, value.args, value.keywords))
        elif isinstance(value, dict):
            nested.extend(value.keys())
            nested.extend(value.values())
        elif isinstance(value, (tuple, list, set, frozenset)):
            nested.extend(value)
        else:
            closure = getattr(value, "__closure__", None) or ()
            for cell in closure:
                try:
                    nested.append(cell.cell_contents)
                except ValueError:
                    pass
            bound_self = getattr(value, "__self__", None)
            if bound_self is not None:
                nested.append(bound_self)
        for item in nested:
            collect(item, depth + 1)

    while pending:
        future = pending.pop()
        if any(future is target for target in targets):
            return True
        if id(future) in seen_futures:
            continue
        seen_futures.add(id(future))
        for entry in getattr(future, "_callbacks", None) or ():
            callback = entry[0] if isinstance(entry, tuple) else entry
            base = callback.func if isinstance(callback, functools.partial) else callback
            identity = (getattr(base, "__module__", None), getattr(base, "__qualname__", None))
            if identity in _ASYNCIO_WRAPPER_CALLBACKS:
                collect(callback)
            elif identity == ("asyncio.tasks", "_AsCompletedIterator._handle_completion"):
                collect(base.__self__._done)
            elif identity == (None, "Task.task_wakeup"):
                task = getattr(callback, "__self__", None)
                if isinstance(task, asyncio.Task):
                    pending.append(task)
            elif identity == ("asyncio.taskgroups", "TaskGroup._on_task_done"):
                parent = getattr(getattr(callback, "__self__", None), "_parent_task", None)
                if isinstance(parent, asyncio.Future):
                    pending.append(parent)
    return False


def _creating_cell_waits_for(
    owner: asyncio.Task[Any] | None, awaiter: asyncio.Task[Any] | None
) -> bool:
    """Return whether the cell owner directly or transitively waits for awaiter."""
    if owner is None or awaiter is None:
        return False
    if owner is awaiter:
        return True
    waiter = getattr(owner, "_fut_waiter", None)
    targets: tuple[asyncio.Future[Any], ...] = (owner,)
    if isinstance(waiter, asyncio.Future):
        targets += (waiter,)
    return _completion_reaches(awaiter, targets)


def _live_cell_owner() -> asyncio.Task[Any] | None:
    """Body task of the cell executing right now, ignoring detached context copies."""
    try:
        from . import repl

        if repl.is_active():
            return repl.active_cell_task()
    except (ImportError, RuntimeError):
        pass
    return None


@dataclass(frozen=True)
class BashResult:
    exit_code: int
    output: str
    duration: float


class _BoundedBuffer:
    """First _HEAD_CAP bytes plus a rolling _TAIL_CAP-byte tail; the middle is dropped."""

    def __init__(self) -> None:
        self._head = bytearray()
        self._tail: deque[bytes] = deque()
        self._tail_size = 0
        self._dropped = 0
        self._lock = threading.Lock()

    def write(self, chunk: bytes) -> None:
        with self._lock:
            if len(self._head) < _HEAD_CAP:
                take = _HEAD_CAP - len(self._head)
                self._head.extend(chunk[:take])
                chunk = chunk[take:]
            if not chunk:
                return
            self._tail.append(chunk)
            self._tail_size += len(chunk)
            # Trim the oldest chunk instead of dropping it whole so exactly _TAIL_CAP bytes stay.
            while self._tail_size > _TAIL_CAP:
                excess = self._tail_size - _TAIL_CAP
                oldest = self._tail[0]
                if len(oldest) <= excess:
                    self._tail.popleft()
                    self._tail_size -= len(oldest)
                    self._dropped += len(oldest)
                else:
                    self._tail[0] = oldest[excess:]
                    self._tail_size -= excess
                    self._dropped += excess

    def size(self) -> int:
        with self._lock:
            return len(self._head) + self._tail_size

    def text(self) -> str:
        with self._lock:
            head = bytes(self._head)
            tail = b"".join(self._tail)
            dropped = self._dropped
        if not dropped:
            return (head + tail).decode("utf-8", errors="replace")
        marker = f"\n... [{dropped} bytes dropped] ...\n"
        return head.decode("utf-8", errors="replace") + marker + tail.decode("utf-8", errors="replace")


class BashHandle:
    """Live handle to a shell command; await it for the BashResult.

    A handle awaited before any other API use (the `await bash(cmd)` one-shot
    form, including `h = bash(cmd)` awaited immediately) owns the command:
    cancelling that await kills the process group. Touching .pid/.running/
    .output()/.tail()/.poll()/.kill() first marks the handle as a background
    handle; later awaits only wait and cancelling them leaves it running.
    """

    def __init__(self, command: str) -> None:
        self.command = command
        completion_context = _current_cell_completion_context()
        self._creating_cell_finished = completion_context[0] if completion_context else None
        self._creating_cell_task = completion_context[1] if completion_context else None
        self._awaited_by_creating_cell = False
        self._buffer = _BoundedBuffer()
        self._done = threading.Event()
        self._eof = threading.Event()
        self._completion_terminal = threading.Event()
        self._completion_output: str | None = None
        self._completion_lock = threading.Lock()
        self._completion_pending = b""
        self._status: int | None = None
        self._status_known = threading.Event()
        self._reaped = False
        self._result: BashResult | None = None
        self._callbacks: list[Callable[[], None]] = []
        self._reap_callback: Callable[[], None] | None = None
        self._result_consumed = False
        self._consumed_notice: Callable[[], None] | None = None
        self._callback_lock = threading.Lock()
        # Serializes kill/reap so a pid fallback can never outlive the process handle.
        self._kill_lock = threading.Lock()
        self._started = time.monotonic()
        # POSIX: own process group so kill() signals the whole pipeline; Windows
        # contains the tree in a kill-on-close job object.
        self._status_read = -1
        self._wake_read = -1
        self._wake_write = -1
        # True only while the pump moves a chunk from the pipe into the buffer.
        self._pump_transfer = False
        self._job: int | None = None
        self._completion_marker: bytes | None = None
        status_write = -1
        if _IS_POSIX:
            # Full-duplex status channel: the child end rides in as stdin (fd 0)
            # and the script remaps it to _STATUS_FD before swapping in /dev/null
            # (dash rejects multi-digit fds in redirections at parse time). The
            # parent end doubles as the gate: the child blocks on it until the
            # pid is journaled, so a kernel kill in that window cannot leak an
            # unjournaled command (parent death closes the socket -> child exits).
            parent_sock, child_sock = socket.socketpair()
            self._status_read = parent_sock.detach()
            status_write = child_sock.detach()
            try:
                self._wake_read, self._wake_write = os.pipe()
            except BaseException:
                os.close(self._status_read)
                os.close(status_write)
                raise
            completion_token = secrets.token_hex(32)
            # Halves stop passive echoes; a deliberate forgery freezes only this call while later bytes stay live.
            token_midpoint = len(completion_token) // 2
            self._completion_marker = (
                _COMPLETION_PREFIX + completion_token.encode("ascii") + _COMPLETION_SUFFIX
            )
            script = _status_script(
                _with_prefix(command),
                completion_token[:token_midpoint],
                completion_token[token_midpoint:],
            )
        else:
            # Windows lacks a foreground-status channel, so its exit drain stays best-effort.
            script = _with_prefix(command)
            self._job = _winjob.create_job()
            if self._job is None:
                # Nothing spawned yet, so nothing can leak: refuse to start.
                raise RuntimeError("bash(): Windows job containment could not be established")
        try:
            self._proc: subprocess.Popen[bytes] | _winjob.JobProcess
            if _IS_POSIX:
                self._proc = subprocess.Popen(
                    [_shell(), "-c", script],
                    cwd=os.getcwd(),
                    env=_child_env(),
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                    stdin=status_write,
                )
            else:
                self._proc = _winjob.spawn_in_job(
                    self._job, [_shell(), "-c", script], cwd=os.getcwd(), env=_child_env()
                )
        except BaseException:
            for fd in (self._status_read, self._wake_read, self._wake_write):
                if fd >= 0:
                    os.close(fd)
            if self._job is not None:
                job, self._job = self._job, None
                _winjob.close(job)
            raise
        finally:
            if status_write >= 0:
                os.close(status_write)
        self._pid: int = self._proc.pid
        self._released = False
        with _live_lock:
            _live_handles.add(self)
        enrolled = _record_journal(self._pid, active=True)
        if not enrolled:
            # Fail closed: a configured journal that cannot enroll the pid must
            # not let the command run (the host reaper would never see it).
            self._abort_spawn()
            raise RuntimeError(
                "bash(): orphan-journal enrollment failed (journal configured but the "
                "pid could not be recorded); the spawned process was killed"
            )
        if _IS_POSIX:
            # Journal first, then open the gate: the child does not run the user
            # command until this byte arrives. A failed write means the child
            # already died; the status/EOF paths report that normally.
            try:
                os.write(self._status_read, b"\n")
            except OSError:
                pass
        else:
            # The child is already job-contained and journaled; resume is the
            # last step. A failed resume would strand a permanently suspended
            # child: fail closed via the assigned job.
            if not cast("_winjob.JobProcess", self._proc).resume():
                self._abort_spawn()
                raise RuntimeError("bash(): Windows job containment could not be established")
        threading.Thread(target=self._pump, daemon=True).start()
        threading.Thread(target=self._report, daemon=True).start()
        threading.Thread(target=self._watch, daemon=True).start()
        self._schedule_background_completion_notice()

    @property
    def pid(self) -> int:
        self._released = True
        return self._pid

    @property
    def running(self) -> bool:
        # Group liveness, matching kill()'s guard and the journal; poll()/await
        # keep foreground result semantics after `cmd &` returns early.
        self._released = True
        return not self._reaped

    def output(self) -> str:
        self._released = True
        self._note_result_consumed()
        return self._buffer.text()

    def tail(self, n: int = 50) -> str:
        self._released = True
        self._note_result_consumed()
        return "\n".join(self._buffer.text().splitlines()[-n:])

    def poll(self) -> BashResult | None:
        self._released = True
        self._note_result_consumed()
        return self._result if self._done.is_set() else None

    def kill(self, sig: int = signal.SIGTERM, grace: float = 5.0) -> None:
        # Guard on group death, not _done: kill() must still reach a lingering
        # background group after the foreground result was already delivered.
        self._released = True
        if self._reaped:
            return
        if not _IS_POSIX:
            with self._kill_lock:
                if self._reaped:  # re-check: _watch may have reaped while we waited
                    return
                if self._job is not None and _winjob.terminate(self._job):
                    return
                # TerminateJobObject failed or reap raced: taskkill fallback.
                if not _taskkill_tree(self._pid):
                    try:
                        self._proc.kill()
                    except OSError:
                        pass
            return
        _signal_group(self._pid, sig)
        if sig == signal.SIGTERM:
            timer = threading.Timer(grace, self._force_kill)
            timer.daemon = True
            timer.start()

    def _force_kill(self) -> None:
        if not self._reaped:
            _signal_group(self._pid, signal.SIGKILL)

    def _pump(self) -> None:
        stdout = self._proc.stdout
        assert stdout is not None
        if not _IS_POSIX:
            try:
                while chunk := stdout.read1(_READ_CHUNK):
                    self._buffer.write(chunk)
            except (OSError, ValueError):
                pass
            stdout.close()
            self._eof.set()
            return
        fd = stdout.fileno()
        try:
            with selectors.DefaultSelector() as sel:
                sel.register(fd, selectors.EVENT_READ)
                while True:
                    sel.select()
                    self._pump_transfer = True
                    try:
                        chunk = os.read(fd, _READ_CHUNK)
                        if not chunk:
                            break
                        self._consume_output(chunk)
                    finally:
                        self._pump_transfer = False
        except (OSError, ValueError):
            pass
        self._abandon_completion()
        try:
            stdout.close()
        except OSError:
            pass
        self._eof.set()

    def _consume_output(self, chunk: bytes) -> None:
        marker = self._completion_marker
        assert marker is not None
        with self._completion_lock:
            if self._completion_terminal.is_set():
                self._buffer.write(chunk)
                return
            data = self._completion_pending + chunk
            marker_at = data.find(marker)
            if marker_at >= 0:
                self._buffer.write(data[:marker_at])
                self._completion_pending = b""
                self._completion_output = self._buffer.text()
                self._completion_terminal.set()
                self._buffer.write(data[marker_at + len(marker) :])
                return
            retained = 0
            for size in range(min(len(data), len(marker) - 1), 0, -1):
                if data.endswith(marker[:size]):
                    retained = size
                    break
            self._buffer.write(data[:-retained] if retained else data)
            self._completion_pending = data[-retained:] if retained else b""

    def _abandon_completion(self) -> None:
        with self._completion_lock:
            if self._completion_terminal.is_set():
                return
            self._buffer.write(self._completion_pending)
            self._completion_pending = b""
            self._completion_terminal.set()

    def _wait_for_completion(self) -> str | None:
        self._completion_terminal.wait()
        return self._completion_output

    def _report(self) -> None:
        # Finalize at foreground completion (status channel), not EOF, so
        # `cmd &` does not hang the await; the shell then `wait`s for its
        # background jobs, keeping the journaled group identity alive.
        status: int | None = None
        try:
            status = self._read_status()
            # Reserve the delivered status before draining so a shell death during
            # the drain window cannot override it with wait()'s signal exit code.
            with self._callback_lock:
                self._status = status
        finally:
            # _watch blocks on this event without a timeout, so every exit path
            # (parsed status, EOF, garbage, exception) must set it.
            self._status_known.set()
        if status is not None:
            output = self._wait_for_completion()
            if output is None:
                self._drain_grace()
            self._finalize(status, output)

    def _watch(self) -> None:
        # Observe shell death independently of the status socket: an early
        # `exit`/`exec`/`set -e`/fatal signal skips `printf`, and background
        # children can hold the socket open past the shell's lifetime.
        exit_code = self._proc.wait()
        if self._wake_write >= 0:
            # Unblock _read_status: background children can hold the status socket
            # open past the shell's lifetime via bash's saved-fd duplicate.
            try:
                os.write(self._wake_write, b"x")
            except OSError:
                pass
            os.close(self._wake_write)
        # _report always sets _status_known (try/finally), so wait indefinitely:
        # a slow reporter can never lose a delivered status to wait()'s code.
        self._status_known.wait()
        with self._callback_lock:
            delivered = self._status
        if delivered is None and not self._done.is_set():
            self._abandon_completion()
            self._drain_grace()
            self._finalize(exit_code)
        with self._kill_lock:
            delivered = self._reap_group()
            self._reaped = True
            if not _IS_POSIX:
                # Reaped: pid fallbacks are gone, so the handle may finally close.
                cast("_winjob.JobProcess", self._proc).close()
        with self._callback_lock:
            callback, self._reap_callback = self._reap_callback, None
        if callback is not None:
            callback()
        if delivered:
            _record_journal(self._pid, active=False)
        with _live_lock:
            _live_handles.discard(self)

    def _reap_group(self) -> bool:
        # Group liveness, not leader death, gates the inactive record: members
        # that outlive the leader would leak behind a stale journal anchor.
        if not _IS_POSIX:
            # Terminate then close the last handle: kill-on-close reaps
            # stragglers. An unproven terminate falls back to taskkill; if
            # that also fails the record stays active for the host reaper.
            delivered = False
            if self._job is not None:
                delivered = _winjob.terminate(self._job)
                job, self._job = self._job, None
                _winjob.close(job)
            return delivered or _taskkill_tree(self._pid)
        try:
            os.killpg(self._pid, 0)
        except ProcessLookupError:
            return True  # group already gone
        except PermissionError:
            pass
        return _signal_group(self._pid, signal.SIGKILL)

    def _read_status(self) -> int | None:
        if self._status_read < 0:
            return None
        try:
            # DefaultSelector (kqueue/epoll) instead of select(): select() rejects
            # fds >= FD_SETSIZE (1024) even when the process fd limit is higher.
            with selectors.DefaultSelector() as sel:
                sel.register(self._status_read, selectors.EVENT_READ)
                sel.register(self._wake_read, selectors.EVENT_READ)
                line = b""
                while b"\n" not in line:
                    ready = {key.fd for key, _ in sel.select()}
                    # Prefer status bytes: any status write happens before shell exit,
                    # so it is already readable whenever the wake fd fires.
                    if self._status_read not in ready:
                        break  # shell died without writing a status
                    chunk = os.read(self._status_read, 64)
                    if not chunk:
                        break  # EOF without a full status line
                    line += chunk
            return int(line)
        except (OSError, ValueError):
            return None
        finally:
            os.close(self._status_read)
            os.close(self._wake_read)

    def _drain_grace(self) -> None:
        # Best-effort fallback when process exit/EOF arrives without a sentinel.
        deadline = time.monotonic() + 0.5
        size = self._buffer.size()
        while time.monotonic() < deadline:
            if self._eof.wait(0.05):
                return
            # A chunk between pipe read and buffer commit (transfer flag) is
            # invisible to both FIONREAD and the buffer size; wait it out.
            if self._pipe_pending() or self._pump_transfer:
                size = self._buffer.size()
                continue
            current = self._buffer.size()
            if current == size:
                return
            size = current

    def _pipe_pending(self) -> bool:
        # POSIX only: FIONREAD on the capture pipe; Windows keeps the
        # quiescence heuristic (best-effort parity).
        if not _IS_POSIX or self._eof.is_set():
            return False
        stdout = self._proc.stdout
        if stdout is None:
            return False
        try:
            pending = struct.unpack("i", fcntl.ioctl(stdout.fileno(), termios.FIONREAD, struct.pack("i", 0)))[0]
        except (OSError, ValueError):
            return False
        return pending > 0

    def _finalize(self, exit_code: int, output: str | None = None) -> None:
        with self._callback_lock:
            if self._done.is_set():
                return
            self._result = BashResult(
                exit_code=exit_code,
                output=self._buffer.text() if output is None else output,
                duration=time.monotonic() - self._started,
            )
            self._done.set()
            callbacks = self._callbacks
            self._callbacks = []
        for callback in callbacks:
            callback()

    def _add_done_callback(self, callback: Callable[[], None]) -> None:
        with self._callback_lock:
            if not self._done.is_set():
                self._callbacks.append(callback)
                return
        callback()

    def _note_result_consumed(self, awaiter: asyncio.Task[Any] | None = None) -> None:
        """Record a result read that reaches the model: only reads during a live
        cell count (a detached reader between turns must keep the notice — it is
        the idle session's only wake-up), and an awaiting reader must be one the
        live cell waits for."""
        if not self._done.is_set():
            return
        owner = _live_cell_owner()
        if owner is None:
            return
        if awaiter is not None and not _creating_cell_waits_for(owner, awaiter):
            return
        with self._callback_lock:
            if self._result_consumed:
                return
            self._result_consumed = True
            notice, self._consumed_notice = self._consumed_notice, None
        if notice is not None:
            notice()

    def _schedule_background_completion_notice(self) -> None:
        cell_finished = self._creating_cell_finished
        if cell_finished is None:
            return
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            return
        from . import repl

        activity = {"id": secrets.token_hex(16), "pid": self._pid, "active": True}
        # Publish synchronously before bash() returns and the creating cell can end.
        repl.emit({"application/vnd.prime-agent.bash-activity+json": activity})
        notice = self._notify_background_completion(cell_finished, activity)
        try:
            task = loop.create_task(notice)
        except BaseException:
            self.kill(signal.SIGKILL if _IS_POSIX else signal.SIGTERM)
            notice.close()
            repl.emit({"application/vnd.prime-agent.bash-activity+json": {**activity, "active": False}})
            raise
        task.add_done_callback(_consume_notice_task)

    async def _notify_background_completion(
        self, cell_finished: asyncio.Event, activity: dict[str, Any]
    ) -> None:
        from . import repl

        try:
            result = await self._wait()
            await self._wait_reaped()
            # The cell may do other work before awaiting this handle. Do not classify
            # it as detached until that whole cell has crossed its completion barrier.
            await cell_finished.wait()
            if self._awaited_by_creating_cell or self._result_consumed or not repl.is_active():
                return
            command = self.command
            if len(command) > _COMPLETION_NOTICE_COMMAND_CAP:
                command = command[:_COMPLETION_NOTICE_COMMAND_CAP] + "\n... [command truncated]"
            reply = await repl.host_request(
                {
                    "type": "bash.completed",
                    "pid": self._pid,
                    "command": command,
                    "exitCode": result.exit_code,
                }
            )
            if isinstance(reply, dict) and reply.get("status") == "ok":
                # Notice accepted by the host; later reads must ask it to withdraw.
                self._arm_consumed_notice(command)
            else:
                sys.stderr.write(
                    f"Background bash completion follow-up for pid {self._pid} was not accepted. "
                    "Inspect the saved handle with poll(), output(), or tail().\n"
                )
        except (OSError, RuntimeError):
            # Standalone runtimes have no host handler, and teardown can close
            # the bridge while a process is finishing. Shell results stay usable.
            return
        finally:
            # Reap and deliver (or report rejection) before releasing kernel residency.
            repl.emit({"application/vnd.prime-agent.bash-activity+json": {**activity, "active": False}})

    def _arm_consumed_notice(self, command: str) -> None:
        # Armed only post-acceptance: the withdrawal can never overtake its notice.
        loop = asyncio.get_running_loop()

        def dispatch() -> None:
            def start() -> None:
                task = loop.create_task(self._notify_result_consumed(command))
                task.add_done_callback(_consume_notice_task)

            try:
                loop.call_soon_threadsafe(start)
            except RuntimeError:
                pass  # notifying loop already closed

        with self._callback_lock:
            if not self._result_consumed:
                self._consumed_notice = dispatch
                return
        dispatch()

    async def _notify_result_consumed(self, command: str) -> None:
        from . import repl

        if not repl.is_active():
            return
        try:
            await repl.host_request(
                {"type": "bash.consumed", "pid": self._pid, "command": command}
            )
        except (OSError, RuntimeError):
            return  # bridge closed at teardown; old hosts error-reply — both fine

    async def _wait_reaped(self) -> None:
        loop = asyncio.get_running_loop()
        future: asyncio.Future[None] = loop.create_future()

        def wake() -> None:
            try:
                loop.call_soon_threadsafe(lambda: future.done() or future.set_result(None))
            except RuntimeError:
                pass

        with self._callback_lock:
            if self._reaped:
                return
            self._reap_callback = wake
        try:
            await future
        finally:
            with self._callback_lock:
                if self._reap_callback is wake:
                    self._reap_callback = None

    async def _wait(self) -> BashResult:
        # Asyncio-native wakeup: no executor thread is parked for the command's
        # duration, so many concurrent awaits cannot exhaust the default pool.
        loop = asyncio.get_running_loop()
        fut: asyncio.Future[None] = loop.create_future()

        def _wake() -> None:
            try:
                loop.call_soon_threadsafe(lambda: fut.done() or fut.set_result(None))
            except RuntimeError:
                pass  # awaiting loop already closed

        self._add_done_callback(_wake)
        await fut
        assert self._result is not None
        return self._result

    async def _wait_owned(self) -> BashResult:
        # One-shot `await bash(cmd)` owns the process: a cancelled await (e.g.
        # a kernel interrupt) must not leave the command running. TERM, bounded
        # grace, group KILL, then a bounded confirmed-exit wait before the
        # CancelledError propagates, so no side effect can land after it.
        try:
            return await self._wait()
        except asyncio.CancelledError:
            # Signal synchronously first: even if the cleanup awaits below are
            # re-cancelled, TERM is already delivered and the escalation timer
            # armed. The confirm wait runs as a shielded task so repeated
            # cancels of this task cannot skip it (they re-raise into awaits
            # inside this except block); the loop re-awaits until it finishes
            # (the confirm coroutine itself is bounded).
            self.kill(grace=_CANCEL_TERM_GRACE)
            confirm = asyncio.ensure_future(self._confirm_group_exit())
            while not confirm.done():
                try:
                    await asyncio.shield(confirm)
                except asyncio.CancelledError:
                    continue
            raise

    async def _confirm_group_exit(self) -> None:
        if not await self._await_group_death(_CANCEL_TERM_GRACE):
            if _IS_POSIX:
                _signal_group(self._pid, signal.SIGKILL)
            else:
                # kill() holds the escalation lock; to_thread keeps the loop free.
                await asyncio.to_thread(self.kill)
            await self._await_group_death(_CANCEL_KILL_WAIT)

    def _group_alive(self) -> bool:
        if not _IS_POSIX:
            job = self._job  # snapshot: _watch may clear it concurrently
            if job is not None:
                # Job accounting sees detached descendants a dead leader hides.
                empty = _winjob.is_empty(job)
                if empty is not None:
                    return not empty
            return self._proc.poll() is None
        try:
            os.killpg(self._pid, 0)
        except ProcessLookupError:
            return False
        except PermissionError:
            pass
        return True

    async def _await_group_death(self, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        while self._group_alive():
            if time.monotonic() >= deadline:
                return False
            await asyncio.sleep(0.02)
        return True

    def _abort_spawn(self) -> None:
        # Enrollment or containment failed before the gate opened (POSIX) or
        # while the child is still suspended, before resume (Windows): kill
        # the child and unwind the handle before threads start.
        if _IS_POSIX:
            for fd in (self._status_read, self._wake_read, self._wake_write):
                if fd >= 0:
                    try:
                        os.close(fd)
                    except OSError:
                        pass
            self._status_read = self._wake_read = self._wake_write = -1
            delivered = _signal_group(self._pid, signal.SIGKILL)
        else:
            with self._kill_lock:
                delivered = False
                if self._job is not None:
                    delivered = _winjob.terminate(self._job)
                    job, self._job = self._job, None
                    _winjob.close(job)
                if not delivered:
                    # Pre-resume abort: the never-run leader has no descendants, so a
                    # delivered kill retires the journal record.
                    try:
                        self._proc.kill()
                        delivered = True
                    except OSError:
                        pass
        if self._proc.stdout is not None:
            self._proc.stdout.close()
        # The blocking wait stays outside the lock: hProcess is still open, so a
        # concurrent raw-pid fallback stays pinned to the right process.
        try:
            self._proc.wait(timeout=5)
        except (OSError, subprocess.SubprocessError):
            pass
        with self._kill_lock:
            self._reaped = True
            if not _IS_POSIX:
                # Reaped commits before close: later lock holders skip raw-pid fallbacks.
                cast("_winjob.JobProcess", self._proc).close()
        with _live_lock:
            _live_handles.discard(self)
        if delivered:
            _record_journal(self._pid, active=False)

    def __await__(self) -> Generator[Any, None, BashResult]:
        # A handle awaited before any other API use is a one-shot command tied
        # to the await (kill-on-cancel); touching the handle API first marks it
        # as a deliberate background handle whose awaits only wait.
        try:
            current_task = asyncio.current_task()
        except RuntimeError:
            current_task = None
        creating_cell_waited = _creating_cell_waits_for(self._creating_cell_task, current_task)
        owned = not self._released
        wait = self._wait_owned() if owned else self._wait()
        self._released = True
        completed = False
        try:
            result = yield from wait.__await__()
            completed = True
            return result
        finally:
            if (completed or owned) and (
                creating_cell_waited
                or _creating_cell_waits_for(self._creating_cell_task, current_task)
            ):
                self._awaited_by_creating_cell = True
            if completed:
                self._note_result_consumed(current_task)

    def __repr__(self) -> str:
        state = f"exit_code={self._result.exit_code}" if self._result else "running"
        return f"<BashHandle pid={self._pid} {state} command={self.command!r}>"


def bash(command: str, *, allow_sudo: bool = False) -> BashHandle:
    """Start a shell command immediately; await the handle for the result.

    `await bash(cmd)` is a one-shot: cancelling the await (e.g. an interrupt)
    kills the command's process group. `h = bash(cmd)` used as a background
    handle (any .pid/.running/.output()/.tail()/.poll()/.kill() access before
    the first await) survives cancellation; awaiting it only waits. Leak
    containment is per-platform: process groups plus the orphan journal on
    POSIX; a kill-on-close job object on Windows entered while the child is
    still suspended, so no descendant can escape it and kill()/crash cleanup
    are unconditional -- bash() raises if containment cannot be established.
    Output written after the completion fence (e.g. by an EXIT trap or a
    background job) is not in BashResult.output but stays visible via
    handle.output()/tail().
    A command that invokes sudo or doas is refused before any process starts,
    because root escapes the containment every other guard relies on. Bypass it
    deliberately with allow_sudo=True, or by starting the kernel with
    PI_BASH_ALLOW_SUDO=1 (honored only when set at kernel start, so a mid-session
    environment write cannot disable the guard).
    """
    if not isinstance(command, str) or not command:
        raise TypeError("command must be a non-empty str")
    _install_shutdown_hook()
    _guard_sudo(command, allow_sudo)
    return BashHandle(command)


def _shell() -> str:
    # Read per call so env changes made in the REPL apply to later commands.
    override = os.environ.get("PRIME_AGENT_BASH_SHELL")
    if override:
        if not os.path.isabs(override):
            raise ValueError("PRIME_AGENT_BASH_SHELL must be an absolute path")
        return override
    if not _IS_POSIX:
        # Never consult PATH on Windows: a repo-controlled PATH could supply
        # the shell. The host injects PRIME_AGENT_BASH_SHELL when one exists.
        raise RuntimeError(
            "bash() needs PRIME_AGENT_BASH_SHELL set to the absolute path of a "
            "POSIX shell on Windows (e.g. install Git Bash in its default "
            "location so the host injects it)"
        )
    # PATH fallback only serves bare/standalone POSIX runtime use: the host
    # always injects PRIME_AGENT_BASH_SHELL (an absolute path) when a shell exists.
    shell = shutil.which("bash")
    return shell or "/bin/sh"


def _with_prefix(command: str) -> str:
    prefix = os.environ.get("PRIME_AGENT_BASH_COMMAND_PREFIX")
    return f"{prefix}\n{command}" if prefix else command


def _fence_printf() -> str:
    # `\command -p printf` defeats alias expansion but not a user-defined shell
    # function named `command`, which would swallow both fence frames and leave
    # the await hanging until the shell dies (wedged behind background jobs). A
    # slash-qualified command name bypasses function and alias lookup for
    # ordinary command names, so resolve printf on the system default utility PATH.
    path = shutil.which("printf", path=os.confstr("CS_PATH") or os.defpath)
    if path and "'" not in path:
        return f"'{path}'"
    return "\\command -p printf"


def _status_script(command: str, completion_a: str, completion_b: str) -> str:
    # Closed control fds preserve background behavior; supported shells atomically write the frame.
    emit = _fence_printf()
    return (
        f"exec {_STATUS_FD}>&0 {_OUTPUT_FD}>&1 0</dev/null\n"
        f"read -r _prime_agent_gate <&{_STATUS_FD} || exit 127\n"
        "{\n"
        f"{command}\n"
        f"}} {_OUTPUT_FD}>&- {_STATUS_FD}>&-\n"
        "__prime_status=$?\n"
        "\\set +x\n"
        f"{emit} '\\036prime-agent-complete:%s%s\\037' "
        f"'{completion_a}' '{completion_b}' >&{_OUTPUT_FD} || exit \"$__prime_status\"\n"
        f"{emit} '%s\\n' \"$__prime_status\" >&{_STATUS_FD}\n"
        f"exec {_OUTPUT_FD}>&- {_STATUS_FD}>&-\n"
        "wait\n"
        'exit "$__prime_status"\n'
    )


def _child_env() -> dict[str, str]:
    """Environment for kernel-spawned shell commands.

    Same non-interactive guard as the coding-agent shell tool
    (packages/coding-agent/src/utils/shell.ts): agent shell commands have no
    usable stdin, so interactive prompts (git commit without -m opening
    $EDITOR, credential asks, pagers) can only hang. Fail fast or no-op
    instead. Deliberately overrides inherited terminal settings; a
    per-command inline assignment (`GIT_EDITOR=vim git commit`) still wins
    because it replaces the exported value for that command.
    """
    env = {
        **os.environ,
        "NO_COLOR": "1",
        "TERM": "dumb",
        "CLICOLOR": "0",
        "FORCE_COLOR": "0",
        "GIT_EDITOR": "true",
        "GIT_SEQUENCE_EDITOR": "true",
        "GIT_TERMINAL_PROMPTS": "0",
        "GIT_ASKPASS": "true",
        "SSH_ASKPASS_REQUIRE": "never",
        "EDITOR": "true",
        "VISUAL": "true",
        "PAGER": "cat",
        "GIT_PAGER": "cat",
        "DEBIAN_FRONTEND": "noninteractive",
    }
    if not _SUDO_BYPASS_AT_KERNEL_START:
        # A mid-session os.environ write must not arm a child kernel's frozen snapshot.
        env.pop(BASH_SUDO_BYPASS_ENV, None)
    return env


def _signal_group(pid: int, sig: int) -> bool:
    """True when the signal was delivered or the group is already gone."""
    try:
        os.killpg(pid, sig)
    except ProcessLookupError:
        return True  # already dead: safe to mark the journal record inactive
    except OSError:
        return False  # not delivered: the record must stay active for the host reaper
    return True


def _system32(*parts: str) -> str:
    # Absolute paths for Windows helper binaries: PATH (and CWD on Windows
    # CPython) lookup could resolve a planted taskkill.exe/powershell.exe.
    root = os.environ.get("SystemRoot", r"C:\Windows")
    return os.path.join(root, "System32", *parts)


def _helper_env() -> dict[str, str]:
    return {**os.environ, "NoDefaultCurrentDirectoryInExePath": "1"}


def _taskkill_tree(pid: int) -> bool:
    # Windows has no process groups to signal; taskkill /T kills the whole tree.
    try:
        return (
            subprocess.run(
                [_system32("taskkill.exe"), "/PID", str(pid), "/T", "/F"],
                capture_output=True,
                timeout=10,
                env=_helper_env(),
            ).returncode
            == 0
        )
    except (OSError, subprocess.SubprocessError):
        return False


def _process_start_id(pid: int) -> str | None:
    if os.name == "nt":
        # Mirrors getWindowsProcessStartId in session-lease.ts byte-for-byte so
        # the host's identity comparison matches the journaled string.
        try:
            out = subprocess.run(
                [
                    _system32("WindowsPowerShell", "v1.0", "powershell.exe"),
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    f"([System.Diagnostics.Process]::GetProcessById({pid})).StartTime.ToUniversalTime().Ticks",
                ],
                capture_output=True,
                text=True,
                timeout=5,
                env=_helper_env(),
            ).stdout.strip()
            return f"win:{out}" if out.isdigit() else None
        except (OSError, subprocess.SubprocessError):
            return None
    try:
        with open(f"/proc/{pid}/stat", "r") as f:
            stat = f.read()
        fields = stat[stat.rindex(")") + 2 :].split(" ")
        if len(fields) > 19 and fields[19]:
            return f"proc:{fields[19]}"
    except (OSError, ValueError):
        pass
    try:
        # macOS has no /proc; /bin/ps is always present there, so use the
        # absolute path (bare `ps` stays only as the exotic-POSIX last resort).
        ps = "/bin/ps" if sys.platform == "darwin" else "ps"
        out = subprocess.run(
            [ps, "-p", str(pid), "-o", "lstart="], capture_output=True, text=True, timeout=5
        ).stdout.strip()
        return f"ps:{out}" if out else None
    except (OSError, subprocess.SubprocessError):
        return None


def _record_journal(pid: int, active: bool) -> bool:
    # Returns False only when the journal is configured but enrollment failed;
    # active-record callers must then fail closed. Active records always carry
    # a processStartId so host reaping stays identity-verified.
    path = os.environ.get("PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL")
    owner = os.environ.get("PRIME_AGENT_KERNEL_OWNER_PID")
    if not path or not owner:
        return True
    try:
        owner_pid = int(owner)
    except ValueError:
        return False
    start_id = _process_start_id(pid) if active else None
    if active and start_id is None:
        return False
    record: dict[str, Any] = {
        "version": 1,
        "pid": pid,
        "ownerPid": owner_pid,
        # The host reaps bash children per kernel pid when it kills or loses this kernel.
        "kernelPid": os.getpid(),
        **({"processStartId": start_id} if start_id else {}),
        "active": active,
        "recordedAt": datetime.now(timezone.utc).isoformat(),
    }
    data = (json.dumps(record) + "\n").encode()
    try:
        fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        try:
            # Complete-write loop: a short write would leave a truncated JSON
            # line that the host discards, which must count as failure.
            view = memoryview(data)
            while view:
                written = os.write(fd, view)
                if written <= 0:
                    return False
                view = view[written:]
            os.fsync(fd)
        finally:
            os.close(fd)
    except OSError:
        return False
    return True


def _kill_live_handles() -> None:
    with _live_lock:
        handles = list(_live_handles)
    for handle in handles:
        if _IS_POSIX:
            delivered = _signal_group(handle._pid, signal.SIGKILL)
        else:
            with handle._kill_lock:
                if handle._reaped:
                    continue
                delivered = handle._job is not None and _winjob.terminate(handle._job)
                if not delivered:
                    delivered = _taskkill_tree(handle._pid)
                if not delivered:
                    # Leader-only fallback cannot prove the tree died: never
                    # justifies an inactive record.
                    try:
                        handle._proc.kill()
                    except OSError:
                        pass
        if delivered:
            _record_journal(handle._pid, active=False)


def _install_shutdown_hook() -> None:
    global _hook_installed
    with _hook_lock:
        if _hook_installed:
            return
        _hook_installed = True
    atexit.register(_kill_live_handles)
