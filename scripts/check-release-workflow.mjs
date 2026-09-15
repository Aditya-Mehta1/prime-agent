#!/usr/bin/env node
/**
 * Release pipeline invariants.
 *
 * These are the properties that make the release safe to run with 45 people
 * holding write access. They are cheap to break by accident in a YAML edit, so
 * they are asserted in CI instead of in a review checklist.
 *
 * The central one: no job holds both a credential and repository code. A job is
 * credential-bearing when it runs in an environment, holds a write permission
 * (a contents:write GITHUB_TOKEN can push tags; id-token:write mints OIDC
 * tokens) or references any secret other than exactly `secrets.GITHUB_TOKEN`
 * anywhere in an expression. Such a job may not check the repository out,
 * install dependencies, or execute anything that lives in the checkout -
 * through any interpreter, not just `node scripts/...`. Because the checker
 * reads shell statically, constructs whose effect it cannot prove (`sh -c`,
 * `eval`, `xargs`, `env -S`, piping into a shell, a command substitution that
 * names a script) are forbidden outright in those jobs.
 */

import { readFileSync } from "node:fs";
import { posix } from "node:path";
import { pathToFileURL } from "node:url";

import { parse } from "yaml";

const RELEASE_WORKFLOW = ".github/workflows/build-binaries.yml";
const STANDALONE_WORKFLOW = ".github/workflows/standalone-binaries.yml";
const BUILD_WORKFLOWS = [RELEASE_WORKFLOW, STANDALONE_WORKFLOW];
/** Where the updater pins the release signer; the checker proves the standalone job cannot mint that identity. */
const RELEASE_TRUST_SOURCE = "packages/coding-agent/src/utils/release-trust.ts";
const SHA_PIN = /@[0-9a-f]{40}$/;

/** Jobs that hold a deployment credential and therefore must run in a protected environment. */
export const CREDENTIAL_JOBS = ["publish-r2", "finalize-release", "publish-beta-r2", "publish-npm", "tap-bump"];
/** Jobs that must exist so the ordering invariants below have something to hold on to. */
const ORDERED_JOBS = ["github-release", "publish-r2", "verify", "finalize-release"];
/** The only job allowed to move a production channel pointer. */
const POINTER_JOB = "finalize-release";
/** The production channel pointers, in the order finalize-release writes them. */
export const PRODUCTION_POINTERS = ["latest.json", "stable", "install.sh", "install-beta.sh"];
/**
 * The only jobs that may invoke the aws CLI at all, and the only R2 keys each may write to.
 *
 * Every `aws s3 cp` destination in these jobs must be spelled out in the workflow file:
 * `s3://${R2_BUCKET}/releases/v${<prefix>}/<literal>` or `.../${name}` where `name` is the
 * basename of the `for file in artifacts/*` loop variable, or - in the job's LAST step only - one
 * of the listed pointer keys. Any other variable, command substitution, bucket or prefix is an
 * error, so `key=stable; aws s3 cp x "s3://$B/$key"` can never sneak a pointer write past the
 * publication order. `aws s3 mv|sync|rm`, `--recursive` and every writing `s3api` call are errors.
 */
export const R2_WRITERS = {
	"publish-r2": { prefix: "PRODUCTION_VERSION", pointers: [] },
	"publish-beta-r2": { prefix: "BETA_VERSION", pointers: ["beta", "beta.json"] },
	[POINTER_JOB]: { prefix: null, pointers: PRODUCTION_POINTERS },
};
/** Options an `aws s3 cp` in a publish job may carry, with the number of values each consumes. */
const AWS_CP_OPTIONS = { "--endpoint-url": 1, "--content-type": 1, "--cache-control": 1, "--quiet": 0, "--no-progress": 0, "--only-show-errors": 0 };
/** `aws s3api` operations that only read. */
const AWS_S3API_READS = /^(head-object|head-bucket|get-object|list-objects|list-objects-v2)$/;
/** Variables the R2 destinations are built from; a publish step may not reassign them. */
const R2_PROTECTED_VARIABLES = /^(R2_BUCKET|R2_ENDPOINT_URL|PRODUCTION_VERSION|BETA_VERSION)$/;
/** How the R2 destination variables must reach a publish job. */
const R2_ENV_SOURCES = {
	R2_BUCKET: /^\$\{\{\s*secrets\.(NIGHTLY_)?R2_BUCKET\s*\}\}$/,
	R2_ENDPOINT_URL: /^\$\{\{\s*secrets\.(NIGHTLY_)?R2_ENDPOINT_URL\s*\}\}$/,
	PRODUCTION_VERSION: /^\$\{\{\s*needs\.context\.outputs\.production_version\s*\}\}$/,
	BETA_VERSION: /^\$\{\{\s*needs\.context\.outputs\.beta_version\s*\}\}$/,
};
/** A literal object name: no expansion, no slash, no leading dot. */
const LITERAL_OBJECT_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;
/** Builtins that change the working directory; in a credential-bearing job only `cd`/`pushd` into a downloaded artifact directory is allowed. */
const DIRECTORY_COMMANDS = /^(cd|pushd|popd|chdir)$/;
/** Commands that assign the names given as their arguments. */
const ASSIGNING_COMMANDS = /^(read|local|declare|typeset|export|readonly|mapfile|readarray|let|unset|getopts|printf)$/;
/** The only `shell:` a credential-bearing step may declare: the checker reads bash. */
const PLAIN_SHELL = /^(bash|sh)$/;

/**
 * The only place a binary may be compiled with a test signer override, and the only step that may
 * do it: the standalone job's end-to-end updater test against an archive this very job signs.
 * The test binary never leaves $RUNNER_TEMP; the uploaded `standalone-<platform>` artifact must
 * not include it.
 */
export const TEST_SIGNER_FLAG = "--test-signer-json";
export const TEST_SIGNER_STEP = { workflow: STANDALONE_WORKFLOW, job: "build", step: "Compile a test-signer binary for the updater test" };
export const TEST_SIGNER_DIRECTORIES = ["binaries-test-signer", "test-release"];

/** Actions that bring repository code or a toolchain onto a credential-bearing runner. */
const FORBIDDEN_ACTIONS = [/(^|\/)checkout(@|$)/, /actions\/setup-node/, /oven-sh\/setup-bun/, /astral-sh\/setup-uv/, /setup-python/];
/** publish-npm needs npm >= 11.5.1 for OIDC trusted publishing; setup-node is a pinned third-party action, not repository code. */
const ALLOWED_ACTIONS = { "publish-npm": [/actions\/setup-node/] };

const PACKAGE_MANAGERS = /^(npm|npx|yarn|pnpm|bun|bunx|corepack|uv|uvx|pip|pip3)$/;
const SHELLS = /^(bash|sh|zsh|dash|ksh|ash|fish)$/;
const INTERPRETERS = /^(node|nodejs|bash|sh|zsh|dash|ksh|ash|fish|python|python3|perl|ruby|tsx|ts-node|deno)$/;
const SOURCE_COMMANDS = /^(source|\.)$/;
/** Commands whose effect the static checker cannot follow; forbidden outright in credential-bearing jobs. */
const OPAQUE_EXECUTORS = /^(eval|xargs|parallel)$/;
/** Builtins that change how later command names resolve or run code the checker cannot see (`trap 'node x' EXIT`). */
const RESOLUTION_BUILTINS = /^(alias|unalias|shopt|enable|hash|trap)$/;
/** Environment variables that make a shell or interpreter load code before the `run` block starts. */
const STARTUP_ENV = /^(BASH_ENV|ENV|PATH|LD_PRELOAD|LD_LIBRARY_PATH|DYLD_INSERT_LIBRARIES|NODE_OPTIONS|NODE_PATH|PYTHONSTARTUP|PYTHONPATH|PERL5OPT|RUBYOPT|SHELLOPTS|BASHOPTS)$/;
/** Words that may precede the command without being it. */
const SHELL_KEYWORDS = /^(if|then|else|elif|fi|do|done|while|until|for|in|case|esac|!|\{|\}|\[\[|\]\]|function)$/;
/** Wrappers that run their argument list as a command after their own options. */
const COMMAND_PREFIXES = /^(sudo|doas|env|nohup|nice|command|builtin|exec|time|timeout|stdbuf|setsid|unbuffer|caffeinate)$/;
const ASSIGNMENT = /^[A-Za-z_][A-Za-z0-9_]*(\[[^\]]*\])?[+]?=/;
const CHECKOUT_PATH = /(^|[\/"'=:])(?:\.\/)?(scripts|\.github|packages|node_modules|test|src)\//;
const WORKSPACE_PATH = /\$\{?(GITHUB_WORKSPACE|RUNNER_WORKSPACE)\}?\/(scripts|\.github|packages|node_modules|test|src)\//;
const SCRIPT_EXTENSION = /\.(m?[jt]s|c[jt]s|sh|bash|zsh|py|rb|pl)$/;
const REDIRECTION = /^[0-9]*(<<<|<<-?|<>|>>|>\||>&|<&|&>>|&>|<|>)/;

/** Joins backslash-continued lines so a command and its arguments are inspected together. */
function joinContinuations(script) {
	const lines = [];
	let pending = "";
	for (const line of String(script).split("\n")) {
		const trailing = line.match(/(\\+)$/);
		if (trailing && trailing[1].length % 2 === 1) {
			pending += `${line.slice(0, -1)} `;
			continue;
		}
		lines.push(pending + line);
		pending = "";
	}
	if (pending) lines.push(pending);
	return lines;
}

const ANSI_C_ESCAPES = { n: "\n", t: "\t", r: "\r", a: "\x07", b: "\b", f: "\f", v: "\v", e: "\x1b", E: "\x1b", "\\": "\\", "'": "'", '"': '"', "?": "?" };

/**
 * POSIX-ish word splitting of one command line.
 *
 * Adjacent quoted and unquoted fragments form ONE word (`'node scr'"'"'ipts/x'` is the word
 * `node scripts/x`). Single quotes are literal, double quotes honour backslash escapes, a
 * backslash outside quotes escapes the next character, `$'...'` is ANSI-C quoted. `$(...)`,
 * backticks, `<(...)`, `>(...)` are kept literally in the word and their inner text is returned in
 * `substitutions` so the caller can inspect what they would run. `;`, `&&`, `||`, `|`, `&`, `(`
 * and `)` end the command; a `#` at the start of a word ends the line. Redirections are dropped.
 *
 * Returns `{ commands, unterminated }` where each command is `{ words, substitutions }` and each
 * word is `{ text, expansion }` - `expansion` is true when the word contains something the shell
 * would expand (`$var`, `$(...)`, backticks), so its value cannot be known statically.
 */
export function splitWords(line) {
	const commands = [];
	let words = [];
	let substitutions = [];
	let redirections = [];
	let piped = false;
	let nextPiped = false;
	let pendingOpens = 0;
	let text = "";
	let expansion = false;
	let inWord = false;
	let quoted = false;
	let unterminated = false;
	const source = String(line);
	let i = 0;

	const endWord = () => {
		if (inWord) words.push({ text, expansion, quoted });
		text = "";
		expansion = false;
		inWord = false;
		quoted = false;
	};
	const endCommand = () => {
		endWord();
		if (words.length > 0 || substitutions.length > 0 || redirections.length > 0) {
			// `opens` counts the `(` that preceded this command, `closes` the `)` that followed it,
			// so a caller can scope a `cd` inside `( ... )` to the subshell.
			commands.push({ words, substitutions, redirections, piped, opens: pendingOpens, closes: 0 });
			pendingOpens = 0;
		}
		words = [];
		substitutions = [];
		redirections = [];
		piped = nextPiped;
		nextPiped = false;
	};
	/** Scans a `$(`/`<(`/`>(` or `$((` body from `start` (just past the opener); returns [inner, end index after the closer]. */
	const scanParens = (start, arithmetic) => {
		let depth = 1;
		let j = start;
		let quote = null;
		while (j < source.length) {
			const ch = source[j];
			if (quote === "'") {
				if (ch === "'") quote = null;
				j += 1;
				continue;
			}
			if (quote === '"') {
				if (ch === "\\") j += 2;
				else {
					if (ch === '"') quote = null;
					else if (ch === "$" && source[j + 1] === "(") {
						const [, end] = scanParens(j + 2, source[j + 2] === "(");
						j = end;
						continue;
					}
					j += 1;
				}
				continue;
			}
			if (ch === "\\") {
				j += 2;
				continue;
			}
			if (ch === "'" || ch === '"') quote = ch;
			else if (ch === "(") depth += 1;
			else if (ch === ")") {
				depth -= 1;
				if (depth === 0) {
					if (arithmetic && source[j + 1] === ")") return [source.slice(start, j), j + 2];
					if (!arithmetic) return [source.slice(start, j), j + 1];
				}
			}
			j += 1;
		}
		unterminated = true;
		return [source.slice(start), source.length];
	};

	/** Index just past the word starting at `start`, honouring quotes, escapes and nested substitutions. */
	const wordExtent = (start) => {
		let j = start;
		while (j < source.length) {
			const c = source[j];
			if (c === "'") {
				const close = source.indexOf("'", j + 1);
				j = close === -1 ? source.length : close + 1;
			} else if (c === '"') {
				j += 1;
				while (j < source.length && source[j] !== '"') j += source[j] === "\\" ? 2 : 1;
				j += 1;
			} else if (c === "\\") j += 2;
			else if ((c === "$" || c === "<" || c === ">") && source[j + 1] === "(") {
				const arithmetic = c === "$" && source[j + 2] === "(";
				[, j] = scanParens(j + 2 + (arithmetic ? 1 : 0), arithmetic);
			} else if (c === "`") {
				const close = source.indexOf("`", j + 1);
				j = close === -1 ? source.length : close + 1;
			} else if (" \t\n;|&()".includes(c)) break;
			else j += 1;
		}
		return Math.min(j, source.length);
	};

	while (i < source.length) {
		const ch = source[i];
		if (ch === " " || ch === "\t") {
			endWord();
			i += 1;
			continue;
		}
		if (ch === "\n") {
			endCommand();
			i += 1;
			continue;
		}
		if (!inWord && ch === "#") break;
		if (ch === "'") {
			inWord = true;
			quoted = true;
			const close = source.indexOf("'", i + 1);
			if (close === -1) {
				unterminated = true;
				text += source.slice(i + 1);
				i = source.length;
			} else {
				text += source.slice(i + 1, close);
				i = close + 1;
			}
			continue;
		}
		if (ch === '"') {
			inWord = true;
			quoted = true;
			i += 1;
			let closed = false;
			while (i < source.length) {
				const c = source[i];
				if (c === "\\") {
					const next = source[i + 1];
					if (next === undefined) {
						i += 1;
						break;
					}
					if (next === "\n") i += 2;
					else if ('$`"\\'.includes(next)) {
						text += next;
						i += 2;
					} else {
						text += `\\${next}`;
						i += 2;
					}
					continue;
				}
				if (c === '"') {
					closed = true;
					i += 1;
					break;
				}
				if (c === "$" && source[i + 1] === "(") {
					const arithmetic = source[i + 2] === "(";
					const [inner, end] = scanParens(i + 2 + (arithmetic ? 1 : 0), arithmetic);
					if (!arithmetic) substitutions.push(inner);
					text += source.slice(i, end);
					expansion = true;
					i = end;
					continue;
				}
				if (c === "`") {
					const close = source.indexOf("`", i + 1);
					const end = close === -1 ? source.length : close + 1;
					if (close === -1) unterminated = true;
					substitutions.push(source.slice(i + 1, close === -1 ? source.length : close));
					text += source.slice(i, end);
					expansion = true;
					i = end;
					continue;
				}
				if (c === "$") expansion = true;
				text += c;
				i += 1;
			}
			if (!closed) unterminated = true;
			continue;
		}
		if (ch === "\\") {
			inWord = true;
			if (i + 1 < source.length) {
				text += source[i + 1];
				i += 2;
			} else i += 1;
			continue;
		}
		if (ch === "$" && source[i + 1] === "'") {
			inWord = true;
			quoted = true;
			i += 2;
			let closed = false;
			while (i < source.length) {
				const c = source[i];
				if (c === "\\") {
					const next = source[i + 1];
					if (next === undefined) {
						i += 1;
						break;
					}
					if (next in ANSI_C_ESCAPES) {
						text += ANSI_C_ESCAPES[next];
						i += 2;
					} else if (next === "x" && /^[0-9a-fA-F]{1,2}/.test(source.slice(i + 2))) {
						const hex = source.slice(i + 2).match(/^[0-9a-fA-F]{1,2}/)[0];
						text += String.fromCharCode(parseInt(hex, 16));
						i += 2 + hex.length;
					} else if (/^[0-7]/.test(next) && /^[0-7]{1,3}/.test(source.slice(i + 1))) {
						const octal = source.slice(i + 1).match(/^[0-7]{1,3}/)[0];
						text += String.fromCharCode(parseInt(octal, 8));
						i += 1 + octal.length;
					} else if (next === "u" && /^[0-9a-fA-F]{1,4}/.test(source.slice(i + 2))) {
						const hex = source.slice(i + 2).match(/^[0-9a-fA-F]{1,4}/)[0];
						text += String.fromCharCode(parseInt(hex, 16));
						i += 2 + hex.length;
					} else {
						text += `\\${next}`;
						i += 2;
					}
					continue;
				}
				if (c === "'") {
					closed = true;
					i += 1;
					break;
				}
				text += c;
				i += 1;
			}
			if (!closed) unterminated = true;
			continue;
		}
		if (ch === "$" && source[i + 1] === "(") {
			inWord = true;
			expansion = true;
			const arithmetic = source[i + 2] === "(";
			const [inner, end] = scanParens(i + 2 + (arithmetic ? 1 : 0), arithmetic);
			if (!arithmetic) substitutions.push(inner);
			text += source.slice(i, end);
			i = end;
			continue;
		}
		if ((ch === "<" || ch === ">") && source[i + 1] === "(") {
			inWord = true;
			expansion = true;
			const [inner, end] = scanParens(i + 2, false);
			substitutions.push(inner);
			text += source.slice(i, end);
			i = end;
			continue;
		}
		if (ch === "`") {
			inWord = true;
			expansion = true;
			const close = source.indexOf("`", i + 1);
			if (close === -1) unterminated = true;
			substitutions.push(source.slice(i + 1, close === -1 ? source.length : close));
			text += source.slice(i, close === -1 ? source.length : close + 1);
			i = close === -1 ? source.length : close + 1;
			continue;
		}
		if (ch === "(" && inWord && /^[A-Za-z_][A-Za-z0-9_]*[+]?=$/.test(text)) {
			// An array assignment: `names=(a b)`, `names+=("$x")`. The parens belong to the word.
			const [inner, end] = scanParens(i + 1, false);
			if (/[$`]/.test(inner)) expansion = true;
			for (const element of splitWords(inner).commands) substitutions.push(...element.substitutions);
			text += source.slice(i, end);
			i = end;
			continue;
		}
		if (ch === ";" || ch === "|" || ch === "&" || ch === "(" || ch === ")") {
			// `&&`, `||`, `;;`, `|&` are all command separators too. A single `|` (or `|&`) feeds
			// the next command's stdin, which matters when that command is a shell.
			let operator = ch;
			i += 1;
			while (i < source.length && ";|&".includes(source[i])) operator += source[i++];
			nextPiped = operator === "|" || operator === "|&";
			endCommand();
			if (operator[0] === "(") pendingOpens += 1;
			else if (operator[0] === ")" && commands.length > 0) commands[commands.length - 1].closes += 1;
			continue;
		}
		if (!inWord && (ch === "<" || ch === ">" || (/[0-9]/.test(ch) && REDIRECTION.test(source.slice(i))))) {
			// A redirection: the operator and its target are not a word of the command, but a
			// target such as `< <(node scripts/x.mjs)` still runs something - keep its substitutions.
			const operator = source.slice(i).match(REDIRECTION)[0];
			i += operator.length;
			while (i < source.length && (source[i] === " " || source[i] === "\t")) i += 1;
			const end = wordExtent(i);
			const target = splitWords(source.slice(i, end));
			if (target.unterminated) unterminated = true;
			for (const command of target.commands) substitutions.push(...command.substitutions);
			const word = target.commands[0]?.words[0];
			redirections.push({ operator, text: word?.text ?? "", expansion: word?.expansion ?? false });
			i = end;
			continue;
		}
		if (ch === "$") expansion = true;
		inWord = true;
		text += ch;
		i += 1;
	}
	endCommand();
	return { commands, unterminated };
}

/**
 * Splits a `run` block into simple commands, skipping heredoc bodies and comments. Yields
 * `{ words, substitutions }` per command, where each word is `{ text, expansion }`.
 */
export function* shellCommands(script) {
	let heredoc = null;
	let heredocIsShell = false;
	let heredocBody = [];
	let carried = "";
	for (const rawLine of joinContinuations(script)) {
		if (heredoc !== null) {
			if (rawLine.trim() === heredoc) {
				heredoc = null;
				// A heredoc fed to a shell IS shell code: inspect it like any other run block.
				if (heredocIsShell) yield* shellCommands(heredocBody.join("\n"));
				heredocBody = [];
			} else heredocBody.push(rawLine);
			continue;
		}
		const line = carried ? `${carried}\n${rawLine}` : rawLine;
		const marker = line.match(/<<-?\s*(?:'([^']+)'|"([^"]+)"|([A-Za-z_][A-Za-z0-9_]*))/);
		const { commands, unterminated } = splitWords(line);
		if (unterminated) {
			// A quote or substitution spans lines; keep reading until it closes.
			carried = line;
			continue;
		}
		carried = "";
		if (marker) {
			heredoc = marker[1] ?? marker[2] ?? marker[3];
			heredocIsShell = commands.some((command) => command.redirections.some((entry) => entry.operator.startsWith("<<") && !entry.operator.startsWith("<<<")) && SHELLS.test(commandWordOf(command)?.text ?? ""));
		}
		for (const command of commands) yield command;
	}
	if (heredoc !== null && heredocIsShell) yield* shellCommands(heredocBody.join("\n"));
	if (carried) {
		// An unterminated construct at the end of the script: inspect what we have, never drop it.
		for (const command of splitWords(carried).commands) yield command;
	}
}

function asCommand(input) {
	if (Array.isArray(input)) {
		return { words: input.map((text) => ({ text: String(text), expansion: /[$`]/.test(String(text)) })), substitutions: [], redirections: [], piped: false, opens: 0, closes: 0 };
	}
	return { redirections: [], piped: false, opens: 0, closes: 0, ...input };
}

/** The index of the word that names the command, after assignments, keywords and wrappers; -1 when there is none. */
function commandIndex(words, reasons = []) {
	let index = 0;
	while (index < words.length && (ASSIGNMENT.test(words[index].text) || SHELL_KEYWORDS.test(words[index].text))) {
		// `for NAME in ...`, `case WORD in ...`, `[[ ... ]]`, `[ ... ]` name no command to execute.
		if (/^(for|case|\[\[|\[|select)$/.test(words[index].text)) return -1;
		index += 1;
	}
	// Wrappers: `sudo -E cmd`, `env -i VAR=x cmd`, `timeout 10 cmd`, `exec cmd`.
	while (index < words.length && COMMAND_PREFIXES.test(words[index].text)) {
		const wrapper = words[index].text;
		index += 1;
		if (wrapper === "env") {
			for (const option of words.slice(index)) {
				if (!option.text.startsWith("-")) break;
				if (/^-[A-Za-z]*S|^--split-string/.test(option.text)) reasons.push(`env -S splits a string into a command the checker cannot see: ${option.text}`);
			}
		}
		if (wrapper === "command" && /^-[vV]/.test(words[index]?.text ?? "")) return -1; // `command -v x` only looks x up
		let positional = wrapper === "timeout" ? 1 : 0;
		while (index < words.length) {
			const word = words[index];
			if (word.text.startsWith("-") && word.text !== "-") {
				index += 1;
				if (wrapper === "timeout" && /^(-k|--kill-after|-s|--signal)$/.test(word.text)) index += 1;
				continue;
			}
			if (ASSIGNMENT.test(word.text)) {
				index += 1;
				continue;
			}
			if (positional > 0) {
				positional -= 1;
				index += 1;
				continue;
			}
			break;
		}
	}
	return index < words.length ? index : -1;
}

function commandWordOf(command) {
	const index = commandIndex(command.words);
	return index === -1 ? undefined : command.words[index];
}

/**
 * Resolves a relative, literal word against the working directory the step has `cd`-ed into.
 * Returns null for words that are not paths a program would open: options, expansions, absolute
 * paths, assignments and URLs.
 */
function resolveAgainst(cwd, word) {
	const text = word.text;
	if (!cwd || !text || word.expansion || text.startsWith("-") || text.startsWith("/") || text.includes("://") || ASSIGNMENT.test(text) || SHELL_KEYWORDS.test(text)) return null;
	return posix.normalize(posix.join(cwd, text));
}

/** The relative directories `actions/download-artifact` fills in this job; the only places a credential-bearing step may `cd` into. */
export function artifactDirectoriesOf(job) {
	const directories = [];
	for (const step of job.steps ?? []) {
		if (!step.uses?.startsWith("actions/download-artifact@")) continue;
		const path = step.with?.path;
		if (typeof path !== "string" || !LITERAL_DIRECTORY.test(path)) continue;
		directories.push(posix.normalize(path));
	}
	return directories;
}
const LITERAL_DIRECTORY = /^[A-Za-z0-9_][A-Za-z0-9_./-]*$/;

/** True when `directory` (already resolved against the workspace root) is a downloaded artifact directory or lies inside one. */
export function isArtifactDirectory(directory, artifactDirectories) {
	if (typeof directory !== "string" || directory.includes("$") || directory.includes("{") || directory.startsWith("/") || directory.startsWith("-")) return false;
	const normalized = posix.normalize(directory).replace(/\/$/, "");
	if (normalized === "" || normalized === "." || normalized.startsWith("..")) return false;
	return artifactDirectories.some((entry) => normalized === entry || normalized.startsWith(`${entry}/`));
}

/**
 * Walks one credential-bearing `run` block in order, tracking the working directory. Returns the
 * reasons the block would run repository code or move to a directory the checker does not allow.
 *
 * `cd` and `pushd` may only target a downloaded artifact directory, spelled literally; `popd`,
 * `chdir`, `cd` with no target, `cd -`, `cd ..` and any target with an expansion are errors.
 * Inside `( ... )` the directory change is scoped to the subshell.
 */
export function credentialStepReasons(run, { artifactDirectories = [], workingDirectory = "" } = {}) {
	const reasons = [];
	let cwd = workingDirectory && isArtifactDirectory(workingDirectory, artifactDirectories) ? posix.normalize(workingDirectory).replace(/\/$/, "") : "";
	const stack = [];
	for (const command of shellCommands(run)) {
		for (let n = 0; n < (command.opens ?? 0); n += 1) stack.push(cwd);
		reasons.push(...repositoryCodeReasons(command, cwd));
		const index = commandIndex(command.words);
		if (index !== -1) {
			const name = command.words[index].text;
			if (DIRECTORY_COMMANDS.test(name)) {
				const args = command.words.slice(index + 1).filter((word) => !(/^-[LPe@]+$/.test(word.text) && !word.expansion));
				const target = args[0];
				const spelled = command.words.slice(index).map((word) => word.text).join(" ");
				if (name === "popd" || name === "chdir") {
					reasons.push(`changes directory in a way the checker cannot follow: ${spelled}`);
				} else if (!target || args.length > 1 || target.expansion || target.text === "-" || target.text === "--") {
					reasons.push(`changes directory to a target the checker cannot resolve: ${spelled}`);
				} else {
					const resolved = posix.normalize(posix.join(cwd, target.text)).replace(/\/$/, "");
					if (!isArtifactDirectory(resolved, artifactDirectories)) {
						reasons.push(`changes directory outside the downloaded artifacts (${artifactDirectories.join(", ") || "none"}): ${spelled}`);
					}
					// Follow the change either way, so what runs next is resolved against where it really runs.
					cwd = resolved === "." ? "" : resolved;
				}
			}
		}
		for (let n = 0; n < (command.closes ?? 0); n += 1) if (stack.length > 0) cwd = stack.pop();
	}
	return reasons;
}

/**
 * Returns the reasons a simple command would execute or read repository code, or would run
 * something the checker cannot see. `input` is a command from {@link shellCommands} (or a plain
 * array of words for convenience).
 */
export function repositoryCodeReasons(input, cwd = "") {
	const { words, substitutions, redirections, piped } = asCommand(input);
	const reasons = [];
	for (const substitution of substitutions) {
		for (const inner of shellCommands(substitution)) {
			for (const reason of repositoryCodeReasons(inner, cwd)) reasons.push(`inside a command substitution: ${reason}`);
			for (const word of inner.words) {
				if (word.text.includes("://")) continue;
				if (SCRIPT_EXTENSION.test(word.text) || /^\.{1,2}\//.test(word.text)) {
					reasons.push(`a command substitution names a script the checker cannot follow: $(${substitution.trim()})`);
					break;
				}
			}
		}
	}
	const commandAt = commandIndex(words);
	for (const [position, word] of words.entries()) {
		if (word.text.includes("://")) continue; // a URL, e.g. the cosign certificate identity
		if (CHECKOUT_PATH.test(word.text) || WORKSPACE_PATH.test(word.text)) {
			reasons.push(`references the checkout: ${word.text}`);
		} else if (cwd && !(position === commandAt && !word.text.includes("/"))) {
			// A bare command name is looked up on PATH, never in the working directory.
			// The step has `cd`-ed somewhere: a bare name resolves against that directory.
			const resolved = resolveAgainst(cwd, word);
			if (resolved !== null && CHECKOUT_PATH.test(resolved)) {
				reasons.push(`references the checkout: ${word.text} resolves to ${resolved} from working directory ${cwd}`);
			}
		}
		if (/^(export\s+)?PATH=/.test(word.text)) reasons.push(`modifies PATH, so a bare command name may resolve to the checkout: ${word.text}`);
	}
	for (const redirection of redirections) {
		if (redirection.text.includes("://")) continue;
		if (CHECKOUT_PATH.test(redirection.text) || WORKSPACE_PATH.test(redirection.text)) {
			reasons.push(`redirects the checkout: ${redirection.operator}${redirection.text}`);
		} else if (cwd) {
			const resolved = resolveAgainst(cwd, redirection);
			if (resolved !== null && CHECKOUT_PATH.test(resolved)) {
				reasons.push(`redirects the checkout: ${redirection.operator}${redirection.text} resolves to ${resolved} from working directory ${cwd}`);
			}
		}
	}
	const index = commandIndex(words, reasons);
	if (index !== -1 && words[index].text === "export" && words.slice(index + 1).some((word) => /^PATH(=|$)/.test(word.text))) {
		reasons.push("modifies PATH, so a bare command name may resolve to the checkout: export PATH");
	}
	if (index === -1) return reasons;
	const commandWord = words[index];
	const command = commandWord.text;
	const args = words.slice(index + 1);
	if (commandWord.expansion) {
		reasons.push(`the command is a shell expansion the checker cannot resolve: ${command}`);
	}
	if (/^\.{1,2}\//.test(command) || (command.includes("/") && !command.startsWith("/"))) {
		reasons.push(`executes a relative path: ${command}`);
	}
	if (OPAQUE_EXECUTORS.test(command)) {
		reasons.push(`${command} runs a command the checker cannot see`);
	}
	if (RESOLUTION_BUILTINS.test(command)) {
		reasons.push(`${command} changes how commands resolve or run, which the checker cannot follow`);
	}
	if (PACKAGE_MANAGERS.test(command)) {
		const sub = args[0]?.text ?? "";
		if (command !== "npm" || !/^(publish|--version|-v|view|config)$/.test(sub)) {
			reasons.push(`runs a package manager: ${command} ${sub}`.trim());
		}
	}
	if (SOURCE_COMMANDS.test(command) && args.length > 0) {
		reasons.push(`sources a file: ${command} ${args[0].text}`);
	}
	if (command === "find" && args.some((arg) => /^-(exec|execdir|ok|okdir)$/.test(arg.text))) {
		reasons.push("find -exec runs a command the checker cannot see");
	}
	if (INTERPRETERS.test(command)) {
		// An interpreter may only run inline code that is written here, in the workflow file: a
		// heredoc (a shell heredoc is inspected by shellCommands) or a literal `-e` string.
		// Everything else - a pipe, a file, an expansion, a shell `-c` string - is code the checker
		// cannot see, and a shell `-c` string in particular defeats word splitting by design.
		if (piped) reasons.push(`${command} reads its script from a pipe the checker cannot see`);
		for (const redirection of redirections) {
			if (/^[0-9]*<$/.test(redirection.operator)) {
				reasons.push(`${command} reads its script from a file: ${redirection.text}`);
			} else if (redirection.operator.startsWith("<<<") && redirection.expansion) {
				reasons.push(`${command} reads its script from an expansion the checker cannot see: ${redirection.text}`);
			}
		}
		for (const arg of args) {
			if (arg.text === "-" || arg.text === "--") break;
			if (SHELLS.test(command) && (/^-[A-Za-z]*[cs]/.test(arg.text) || /^--(command|stdin)/.test(arg.text) || /^[-+]o$/.test(arg.text))) {
				reasons.push(`${command} ${arg.text} runs inline or piped shell code the checker cannot see`);
				break;
			}
			if (/^-[cem]$/.test(arg.text) || arg.text === "--eval" || arg.text === "--print" || arg.text === "-p") break; // inline code follows
			if (arg.text.startsWith("-")) continue;
			// The first positional argument is the script file. Whatever it is called, it is a file
			// on this runner that the checker did not write.
			reasons.push(`runs a file through ${command}: ${arg.text}`);
			break;
		}
	}
	return reasons;
}

/** The shell variables a simple command assigns: `X=1 cmd`, `for X in`, `read X`, `local X=`, `export X`, `printf -v X`. */
function assignedNames(command) {
	const names = [];
	const words = command.words;
	let index = 0;
	while (index < words.length && (ASSIGNMENT.test(words[index].text) || SHELL_KEYWORDS.test(words[index].text))) {
		if (ASSIGNMENT.test(words[index].text)) names.push(words[index].text.match(/^[A-Za-z_][A-Za-z0-9_]*/)[0]);
		if (/^(for|select)$/.test(words[index].text) && words[index + 1]) names.push(words[index + 1].text);
		index += 1;
	}
	const commandAt = commandIndex(words);
	if (commandAt !== -1 && ASSIGNING_COMMANDS.test(words[commandAt].text)) {
		for (const arg of words.slice(commandAt + 1)) {
			if (arg.text.startsWith("-")) continue;
			const name = arg.text.match(/^[A-Za-z_][A-Za-z0-9_]*/);
			if (name) names.push(name[0]);
		}
	}
	return names;
}

/**
 * Walks one `run` block for aws CLI invocations and checks every one against {@link R2_WRITERS}.
 * Returns `{ reasons, pointers }`: the violations, and the pointer keys the block writes.
 *
 * The walk is stateful: `for file in <artifact dir>/*` binds `file`, and exactly
 * `name=$(basename "$file")` then binds `name`; any other assignment to either name unbinds it,
 * and a destination ending in `${name}` is only allowed while `name` is bound.
 */
export function r2StepReasons(jobId, run, { last = false, artifactDirectories = [] } = {}) {
	const writer = R2_WRITERS[jobId];
	const reasons = [];
	const pointers = [];
	const bound = { file: false, name: false };
	const spell = (command, index) => command.words.slice(index).map((word) => word.text).join(" ");
	const inspect = (command, context) => {
		for (const substitution of command.substitutions) {
			for (const inner of shellCommands(substitution)) inspect(inner, "inside a command substitution: ");
		}
		const assigned = assignedNames(command);
		for (const variable of assigned) {
			if (writer && R2_PROTECTED_VARIABLES.test(variable)) {
				reasons.push(`${context}reassigns ${variable}, which every R2 destination is built from: ${spell(command, 0)}`);
			}
		}
		// `do`, `then`, `else` and `{` may precede the statement on the same line.
		let first = 0;
		while (first < command.words.length && /^(do|then|else|\{)$/.test(command.words[first].text)) first += 1;
		const statement = command.words.slice(first);
		const texts = statement.map((word) => word.text);
		if (texts.length === 4 && texts[0] === "for" && texts[1] === "file" && texts[2] === "in" && !statement[3].expansion && texts[3].endsWith("/*") && artifactDirectories.includes(posix.normalize(texts[3].slice(0, -2)))) {
			bound.file = true;
			bound.name = false;
		} else if (texts.length === 1 && texts[0] === 'name=$(basename "$file")') {
			bound.name = bound.file;
		} else {
			if (assigned.includes("file")) bound.file = false;
			if (assigned.includes("file") || assigned.includes("name")) bound.name = false;
		}
		const index = commandIndex(command.words);
		if (index === -1) return;
		const commandWord = command.words[index];
		if (posix.basename(commandWord.text) !== "aws") return;
		const spelled = spell(command, index);
		if (!writer) {
			reasons.push(`${context}invokes the aws CLI; only ${Object.keys(R2_WRITERS).join(", ")} may talk to R2: ${spelled}`);
			return;
		}
		if (commandWord.text !== "aws" || commandWord.expansion) {
			reasons.push(`${context}invokes aws through a path or expansion: ${spelled}`);
			return;
		}
		const args = command.words.slice(index + 1);
		const [service, operation] = args;
		if (!service || !operation || service.expansion || operation.expansion) {
			reasons.push(`${context}aws must name a literal service and operation: ${spelled}`);
			return;
		}
		if (service.text === "s3api") {
			if (AWS_S3API_READS.test(operation.text)) return;
			reasons.push(`${context}aws s3api ${operation.text} writes outside the 'aws s3 cp' allowlist: ${spelled}`);
			return;
		}
		if (service.text !== "s3") {
			reasons.push(`${context}aws ${service.text} is not an R2 object operation: ${spelled}`);
			return;
		}
		if (operation.text === "ls") return;
		if (operation.text !== "cp") {
			reasons.push(`${context}aws s3 ${operation.text} is not allowed; only 'aws s3 cp' with a spelled-out destination may write: ${spelled}`);
			return;
		}
		const positionals = [];
		for (let i = 2; i < args.length; i += 1) {
			const arg = args[i];
			if (arg.text.startsWith("-")) {
				const values = AWS_CP_OPTIONS[arg.text];
				if (values === undefined || arg.expansion) {
					reasons.push(`${context}aws s3 cp carries an option the checker does not allow: ${arg.text} (${spelled})`);
					return;
				}
				i += values;
				continue;
			}
			positionals.push(arg);
		}
		if (positionals.length !== 2) {
			reasons.push(`${context}aws s3 cp must name exactly one source and one destination: ${spelled}`);
			return;
		}
		const [source, destination] = positionals;
		if (!destination.text.startsWith("s3://")) {
			if (!source.text.startsWith("s3://")) reasons.push(`${context}aws s3 cp copies between local paths: ${spelled}`);
			return; // a download; where it lands is checked like any other path
		}
		const sourceIsLoopFile = source.text === "$file" || source.text === "${file}";
		const sourceIsArtifact = !source.expansion && !/[*?[]/.test(source.text) && artifactDirectories.some((directory) => source.text.startsWith(`${directory}/`));
		if (!(sourceIsLoopFile ? bound.file : sourceIsArtifact)) {
			reasons.push(`${context}aws s3 cp uploads something other than a downloaded artifact: ${source.text} (${spelled})`);
		}
		const bucketMatch = destination.text.match(/^s3:\/\/\$\{R2_BUCKET\}\/(.*)$/);
		if (!bucketMatch) {
			reasons.push(`${context}aws s3 cp destination must be spelled s3://\${R2_BUCKET}/<key>: ${destination.text}`);
			return;
		}
		const key = bucketMatch[1];
		if (writer.prefix) {
			const prefix = `releases/v\${${writer.prefix}}/`;
			if (key.startsWith(prefix)) {
				const object = key.slice(prefix.length);
				if (LITERAL_OBJECT_NAME.test(object)) return;
				if (object === "${name}") {
					if (bound.name) return;
					reasons.push(`${context}aws s3 cp destination uses \${name} where it is not the basename of the 'for file in artifacts/*' loop variable: ${destination.text}`);
					return;
				}
				reasons.push(`${context}aws s3 cp object name must be a literal or \${name}, never another expansion: ${destination.text}`);
				return;
			}
		}
		if (writer.pointers.includes(key)) {
			pointers.push(key);
			if (!last) reasons.push(`${context}must advance the channel pointers in its last step; '${key}' is written earlier (${spelled})`);
			return;
		}
		if (PRODUCTION_POINTERS.includes(key)) {
			if (jobId === "publish-beta-r2") reasons.push(`${context}the beta channel must never write a production pointer (${spelled})`);
			else reasons.push(`${context}only '${POINTER_JOB}' may write a production pointer; '${jobId}' does (${spelled})`);
			return;
		}
		reasons.push(`${context}aws s3 cp destination is outside the allowlist for '${jobId}' (${writer.prefix ? `releases/v\${${writer.prefix}}/<literal|\${name}>` : "no prefix"}${writer.pointers.length ? `, last step: ${writer.pointers.join(", ")}` : ""}): ${destination.text}`);
	};
	for (const command of shellCommands(run)) inspect(command, "");
	return { reasons, pointers };
}

/** True when an expression references any secret other than exactly `secrets.GITHUB_TOKEN`. */
export function referencesSecret(text) {
	return /\bsecrets\b/.test(String(text).replace(/\bsecrets\.GITHUB_TOKEN\b/g, ""));
}

/** Every `${{ ... }}` expression and `if:` condition in a job, wherever it appears (env, with, run, if). */
function expressionsOf(job) {
	const expressions = [];
	for (const match of JSON.stringify(job).matchAll(/\$\{\{([\s\S]*?)\}\}/g)) expressions.push(match[1]);
	for (const condition of [job.if, ...(job.steps ?? []).map((step) => step.if)]) {
		if (condition !== undefined) expressions.push(String(condition));
	}
	return expressions;
}

/** True when a job holds something an attacker could exfiltrate or misuse. */
export function isCredentialBearing(job) {
	if (job.environment) return true;
	for (const value of Object.values(job.permissions ?? {})) {
		if (value === "write") return true;
	}
	return expressionsOf(job).some(referencesSecret);
}

function needsOf(job) {
	if (!job?.needs) return [];
	return Array.isArray(job.needs) ? job.needs : [job.needs];
}

export function checkWorkflows(read = (path) => readFileSync(path, "utf8")) {
	const problems = [];
	const fail = (message) => problems.push(message);
	const release = parse(read(RELEASE_WORKFLOW));
	const triggers = release.on ?? release[true];

	if (triggers?.push?.tags) {
		fail(`${RELEASE_WORKFLOW}: a tag push must not start a release; the release job creates the tag.`);
	}
	if (JSON.stringify(release.permissions ?? null) !== "{}") {
		fail(`${RELEASE_WORKFLOW}: the workflow must declare 'permissions: {}' and let jobs opt in.`);
	}

	for (const [jobId, job] of Object.entries(release.jobs)) {
		if (job.permissions === undefined) {
			fail(`${RELEASE_WORKFLOW}: job '${jobId}' does not declare its own permissions.`);
		}
		for (const [name, value] of Object.entries(job.env ?? {})) {
			if (referencesSecret(value)) {
				fail(`${RELEASE_WORKFLOW}: job '${jobId}' exposes ${name} to every step; move it to the step that uses it.`);
			}
		}
		if (!isCredentialBearing(job)) continue;
		const allowed = ALLOWED_ACTIONS[jobId] ?? [];
		const artifactDirectories = artifactDirectoriesOf(job);
		// A default working directory or shell would change what every `run` line means.
		for (const [scope, defaults] of [["the workflow", release.defaults], [`job '${jobId}'`, job.defaults]]) {
			const directory = defaults?.run?.["working-directory"];
			if (directory !== undefined && !isArtifactDirectory(String(directory), artifactDirectories)) {
				fail(`${RELEASE_WORKFLOW}: credential-bearing job '${jobId}' must not run in a working directory other than a downloaded artifact directory (${scope} defaults to ${directory}).`);
			}
			const shell = defaults?.run?.shell;
			if (shell !== undefined && !PLAIN_SHELL.test(String(shell))) {
				fail(`${RELEASE_WORKFLOW}: credential-bearing job '${jobId}' must run bash; ${scope} defaults to shell '${shell}', which the checker cannot read.`);
			}
		}
		for (const name of Object.keys(job.env ?? {})) {
			if (STARTUP_ENV.test(name)) fail(`${RELEASE_WORKFLOW}: credential-bearing job '${jobId}' sets ${name}, which loads code before any command runs.`);
		}
		for (const step of job.steps ?? []) {
			const label = step.name ?? step.uses ?? "(unnamed step)";
			for (const name of Object.keys(step.env ?? {})) {
				if (STARTUP_ENV.test(name)) fail(`${RELEASE_WORKFLOW}: credential-bearing job '${jobId}' sets ${name}, which loads code before any command runs (${label}).`);
			}
			const workingDirectory = step["working-directory"] ?? job.defaults?.run?.["working-directory"] ?? release.defaults?.run?.["working-directory"];
			if (step["working-directory"] !== undefined && !isArtifactDirectory(String(step["working-directory"]), artifactDirectories)) {
				fail(`${RELEASE_WORKFLOW}: credential-bearing job '${jobId}' must not run in a working directory other than a downloaded artifact directory (${artifactDirectories.join(", ") || "none"}): working-directory: ${step["working-directory"]} (${label}).`);
			}
			if (step.shell !== undefined && !PLAIN_SHELL.test(String(step.shell))) {
				fail(`${RELEASE_WORKFLOW}: credential-bearing job '${jobId}' must run bash; shell '${step.shell}' is code the checker cannot read (${label}).`);
			}
			if (step.uses) {
				for (const pattern of FORBIDDEN_ACTIONS) {
					if (pattern.test(step.uses) && !allowed.some((entry) => entry.test(step.uses))) {
						fail(`${RELEASE_WORKFLOW}: credential-bearing job '${jobId}' must not use ${step.uses} (${label}).`);
					}
				}
				if (step.uses.startsWith("./")) {
					fail(`${RELEASE_WORKFLOW}: credential-bearing job '${jobId}' must not run a local action (${step.uses}).`);
				}
			}
			const run = String(step.run ?? "");
			const options = { artifactDirectories, workingDirectory: workingDirectory === undefined ? "" : String(workingDirectory) };
			for (const reason of credentialStepReasons(run, options)) {
				if (reason.includes("changes directory")) {
					fail(`${RELEASE_WORKFLOW}: credential-bearing job '${jobId}' ${reason} (${label}).`);
				} else {
					fail(`${RELEASE_WORKFLOW}: credential-bearing job '${jobId}' must not run repository code: ${reason} (${label}).`);
				}
			}
		}
	}

	for (const jobId of CREDENTIAL_JOBS) {
		const job = release.jobs[jobId];
		if (!job) {
			fail(`${RELEASE_WORKFLOW}: expected a credential-bearing job named '${jobId}'.`);
			continue;
		}
		if (!job.environment) {
			fail(`${RELEASE_WORKFLOW}: job '${jobId}' must run in a protected environment.`);
		}
	}

	for (const jobId of ORDERED_JOBS) {
		if (!release.jobs[jobId]) fail(`${RELEASE_WORKFLOW}: expected a job named '${jobId}'.`);
	}
	const orderedPairs = [
		["publish-r2", "github-release"],
		["verify", "publish-r2"],
		["finalize-release", "verify"],
		["publish-npm", "finalize-release"],
		["tap-bump", "finalize-release"],
	];
	for (const [later, earlier] of orderedPairs) {
		if (release.jobs[later] && !needsOf(release.jobs[later]).includes(earlier)) {
			fail(`${RELEASE_WORKFLOW}: job '${later}' must need '${earlier}'; nothing may become public before verification.`);
		}
	}
	// Every aws invocation in the workflow is checked against the R2 allowlist; the pointer keys each
	// step writes fall out of that walk and drive the ordering checks below.
	const pointerSteps = new Map();
	for (const [jobId, job] of Object.entries(release.jobs)) {
		const artifactDirectories = artifactDirectoriesOf(job);
		const steps = job.steps ?? [];
		if (R2_WRITERS[jobId]) {
			const environments = [["job", job.env ?? {}], ...steps.map((step) => [`step '${step.name ?? "(unnamed)"}'`, step.env ?? {}])];
			for (const [scope, env] of environments) {
				for (const [name, source] of Object.entries(R2_ENV_SOURCES)) {
					if (env[name] !== undefined && !source.test(String(env[name]))) {
						fail(`${RELEASE_WORKFLOW}: job '${jobId}' ${scope} sets ${name} to '${env[name]}'; the R2 destinations may only be built from the ${name.endsWith("VERSION") ? "context output" : "secret"}.`);
					}
				}
			}
		}
		steps.forEach((step, index) => {
			const label = step.name ?? step.uses ?? "(unnamed step)";
			const { reasons, pointers } = r2StepReasons(jobId, String(step.run ?? ""), { last: index === steps.length - 1, artifactDirectories });
			for (const reason of reasons) fail(`${RELEASE_WORKFLOW}: job '${jobId}' ${reason} (${label}).`);
			if (pointers.length > 0) pointerSteps.set(`${jobId}\u0000${index}`, pointers);
		});
	}
	if (release.jobs[POINTER_JOB]) {
		const steps = release.jobs[POINTER_JOB].steps ?? [];
		const pointerIndex = steps.findIndex((_, index) => pointerSteps.has(`${POINTER_JOB}\u0000${index}`));
		const written = pointerSteps.get(`${POINTER_JOB}\u0000${steps.length - 1}`) ?? [];
		const missing = PRODUCTION_POINTERS.filter((pointer) => !written.includes(pointer));
		if (missing.length > 0) {
			fail(`${RELEASE_WORKFLOW}: job '${POINTER_JOB}' must advance the production channel pointers in its last step (missing: ${missing.join(", ")}).`);
		}
		const publishIndex = steps.findIndex((step) => /gh release edit .*--draft=false/.test(String(step.run ?? "")));
		if (publishIndex === -1 || (pointerIndex !== -1 && publishIndex > pointerIndex)) {
			fail(`${RELEASE_WORKFLOW}: job '${POINTER_JOB}' must publish the GitHub release before it moves the channel pointers.`);
		}
	}

	for (const path of BUILD_WORKFLOWS) {
		const workflow = parse(read(path));
		for (const [jobId, job] of Object.entries(workflow.jobs ?? {})) {
			for (const step of job.steps ?? []) {
				if (step.uses && !SHA_PIN.test(step.uses) && !step.uses.startsWith("./")) {
					fail(`${path}: job '${jobId}' uses '${step.uses}' without a full commit SHA.`);
				}
				const run = String(step.run ?? "");
				for (const line of run.split("\n")) {
					if (/\bnpm ci\b/.test(line) && !line.includes("--ignore-scripts")) {
						fail(`${path}: job '${jobId}' runs 'npm ci' without --ignore-scripts.`);
					}
				}
				// The test signer override may be compiled in exactly one place: the standalone job's
				// end-to-end updater test. Everywhere else `build-binary.mjs` runs with the production pin.
				const text = JSON.stringify(step);
				const isTestSignerStep = path === TEST_SIGNER_STEP.workflow && jobId === TEST_SIGNER_STEP.job && step.name === TEST_SIGNER_STEP.step;
				if (text.includes(TEST_SIGNER_FLAG) && !isTestSignerStep) {
					fail(`${path}: job '${jobId}' step '${step.name ?? step.uses ?? "(unnamed)"}' uses ${TEST_SIGNER_FLAG}; only '${TEST_SIGNER_STEP.step}' in ${TEST_SIGNER_STEP.workflow} may compile a test-signer binary.`);
				}
				if (/__PRIME_AGENT_RELEASE_SIGNER_OVERRIDE__/.test(text)) {
					fail(`${path}: job '${jobId}' step '${step.name ?? "(unnamed)"}' sets __PRIME_AGENT_RELEASE_SIGNER_OVERRIDE__ directly; only build-binary.mjs may define it.`);
				}
				// The uploaded standalone artifact is what the release consumes; the test-signer build must never be in it.
				if (step.uses?.startsWith("actions/upload-artifact@")) {
					const uploadPath = String(step.with?.path ?? "");
					for (const directory of TEST_SIGNER_DIRECTORIES) {
						if (uploadPath.includes(directory)) {
							fail(`${path}: job '${jobId}' uploads '${directory}', which holds test-signer builds; the release must never consume them.`);
						}
					}
					if (/\*\*|\$\{\{\s*runner\.temp\s*\}\}\/?\s*$/m.test(uploadPath)) {
						fail(`${path}: job '${jobId}' uploads a directory tree (${uploadPath.trim().split("\n").join(" ")}); list the release files explicitly so a test-signer build cannot ride along.`);
					}
				}
			}
		}
	}
	const standalone = parse(read(STANDALONE_WORKFLOW));
	// The standalone job compiles and tests repository code AND holds id-token:write. That is only
	// safe because the identity it can mint - the called workflow path, standalone-binaries.yml -
	// is not the one the updater pins. Nothing else may be granted to it, here or by its caller.
	const trust = read(RELEASE_TRUST_SOURCE);
	const pinnedPath = trust.match(/RELEASE_SIGNER_WORKFLOW_PATH\s*=\s*"([^"]+)"/)?.[1];
	if (pinnedPath !== RELEASE_WORKFLOW) {
		fail(`${RELEASE_TRUST_SOURCE}: RELEASE_SIGNER_WORKFLOW_PATH must pin ${RELEASE_WORKFLOW}, found ${pinnedPath ?? "nothing"}; the standalone job can sign as any other path.`);
	}
	for (const [jobId, job] of Object.entries(standalone.jobs ?? {})) {
		if (job.environment) fail(`${STANDALONE_WORKFLOW}: job '${jobId}' must not run in an environment; it runs repository code.`);
		for (const [scope, value] of Object.entries(job.permissions ?? {})) {
			if (!(scope === "contents" && value === "read") && !(scope === "id-token" && value === "write")) {
				fail(`${STANDALONE_WORKFLOW}: job '${jobId}' holds '${scope}: ${value}'; only contents:read and id-token:write are allowed next to repository code.`);
			}
		}
		if (expressionsOf(job).some(referencesSecret)) {
			fail(`${STANDALONE_WORKFLOW}: job '${jobId}' references a secret; it runs repository code.`);
		}
	}
	const caller = release.jobs.standalone;
	if (caller?.uses !== `./${STANDALONE_WORKFLOW}`) {
		fail(`${RELEASE_WORKFLOW}: expected job 'standalone' to call ${STANDALONE_WORKFLOW}.`);
	} else {
		for (const [scope, value] of Object.entries(caller.permissions ?? {})) {
			if (!(scope === "contents" && value === "read") && !(scope === "id-token" && value === "write")) {
				fail(`${RELEASE_WORKFLOW}: job 'standalone' passes '${scope}: ${value}' to a workflow that runs repository code.`);
			}
		}
	}
	const buildJob = standalone.jobs?.[TEST_SIGNER_STEP.job];
	const testSignerStep = (buildJob?.steps ?? []).find((step) => step.name === TEST_SIGNER_STEP.step);
	if (testSignerStep) {
		const run = String(testSignerStep.run ?? "");
		if (!run.includes(TEST_SIGNER_FLAG)) {
			fail(`${STANDALONE_WORKFLOW}: step '${TEST_SIGNER_STEP.step}' must compile with ${TEST_SIGNER_FLAG}.`);
		}
		// Everything the test-signer step produces lives under $RUNNER_TEMP/test-release: the signer
		// JSON it compiles against and every archive it assembles.
		for (const line of run.split("\n")) {
			if (line.includes(TEST_SIGNER_FLAG) && !new RegExp(`${TEST_SIGNER_FLAG} "\\$RUNNER_TEMP/test-release/`).test(line)) {
				fail(`${STANDALONE_WORKFLOW}: step '${TEST_SIGNER_STEP.step}' must read the signer JSON from under $RUNNER_TEMP/test-release: ${line.trim()}`);
			}
			if (line.includes("assemble-release-archives.mjs") && !/assemble-release-archives\.mjs \S+ "\$RUNNER_TEMP\/test-release\//.test(line)) {
				fail(`${STANDALONE_WORKFLOW}: step '${TEST_SIGNER_STEP.step}' must keep the test-signer build under $RUNNER_TEMP/test-release: ${line.trim()}`);
			}
		}
		const upload = (buildJob.steps ?? []).find((step) => step.uses?.startsWith("actions/upload-artifact@"));
		const assemble = (buildJob.steps ?? []).findIndex((step) => /assemble-release-archives\.mjs[^\n]*\$RUNNER_TEMP\/standalone\b/.test(String(step.run ?? "")));
		const compile = (buildJob.steps ?? []).indexOf(testSignerStep);
		if (upload && !/\$\{\{\s*runner\.temp\s*\}\}\/standalone\//.test(String(upload.with?.path ?? ""))) {
			fail(`${STANDALONE_WORKFLOW}: the standalone artifact must upload only from $RUNNER_TEMP/standalone/.`);
		}
		if (assemble !== -1 && compile < assemble) {
			fail(`${STANDALONE_WORKFLOW}: the release archive must be assembled before '${TEST_SIGNER_STEP.step}' compiles anything else.`);
		}
	}

	return problems;
}

const invokedDirectly = process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;
if (invokedDirectly) {
	const problems = checkWorkflows();
	for (const problem of problems) console.error(`error: ${problem}`);
	if (problems.length > 0) process.exit(1);
	console.log("release workflow invariants hold");
}
