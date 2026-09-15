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
 * tokens) or reads any secret other than GITHUB_TOKEN. Such a job may not check
 * the repository out, install dependencies, or execute anything that lives in
 * the checkout - through any interpreter, not just `node scripts/...`.
 */

import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

import { parse } from "yaml";

const RELEASE_WORKFLOW = ".github/workflows/build-binaries.yml";
const BUILD_WORKFLOWS = [RELEASE_WORKFLOW, ".github/workflows/standalone-binaries.yml"];
const SHA_PIN = /@[0-9a-f]{40}$/;

/** Jobs that hold a deployment credential and therefore must run in a protected environment. */
export const CREDENTIAL_JOBS = ["publish-r2", "finalize-release", "publish-beta-r2", "publish-npm", "tap-bump"];
/** Jobs that must exist so the ordering invariants below have something to hold on to. */
const ORDERED_JOBS = ["github-release", "publish-r2", "verify", "finalize-release"];
/** The only job allowed to move a production channel pointer. */
const POINTER_JOB = "finalize-release";

/** Actions that bring repository code or a toolchain onto a credential-bearing runner. */
const FORBIDDEN_ACTIONS = [/(^|\/)checkout(@|$)/, /actions\/setup-node/, /oven-sh\/setup-bun/, /astral-sh\/setup-uv/, /setup-python/];
/** publish-npm needs npm >= 11.5.1 for OIDC trusted publishing; setup-node is a pinned third-party action, not repository code. */
const ALLOWED_ACTIONS = { "publish-npm": [/actions\/setup-node/] };

const PACKAGE_MANAGERS = /^(npm|npx|yarn|pnpm|bun|bunx|corepack|uv|uvx|pip|pip3)$/;
const INTERPRETERS = /^(node|nodejs|bash|sh|zsh|dash|ksh|python|python3|perl|ruby|tsx|ts-node|deno|exec|eval)$/;
const SOURCE_COMMANDS = /^(source|\.)$/;
const SHELL_KEYWORDS = /^(if|then|else|elif|fi|do|done|while|until|for|in|case|esac|!|time|sudo|env|nohup|nice|command|builtin|\{|\}|\[\[|\]\])$/;
const ASSIGNMENT = /^[A-Za-z_][A-Za-z0-9_]*(\[[^\]]*\])?[+]?=/;
const CHECKOUT_PATH = /(^|[\/"'=:])(?:\.\/)?(scripts|\.github|packages|node_modules|test|src)\//;
const WORKSPACE_PATH = /\$\{?(GITHUB_WORKSPACE|RUNNER_WORKSPACE)\}?\/(scripts|\.github|packages|node_modules|test|src)\//;
const SCRIPT_EXTENSION = /\.(m?[jt]s|c[jt]s|sh|bash|zsh|py|rb|pl)$/;

function stripQuotes(token) {
	return token.replace(/^["']+|["']+$/g, "");
}

function looksLikePath(token) {
	const plain = stripQuotes(token);
	if (plain === "" || plain === "-") return false;
	if (plain.includes("/")) return true;
	return SCRIPT_EXTENSION.test(plain);
}

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

/** Splits a `run` block into simple commands, skipping heredoc bodies and comments. */
export function* shellCommands(script) {
	let heredoc = null;
	for (const rawLine of joinContinuations(script)) {
		if (heredoc !== null) {
			if (rawLine.trim() === heredoc) heredoc = null;
			continue;
		}
		const marker = rawLine.match(/<<-?\s*(?:'([^']+)'|"([^"]+)"|([A-Za-z_][A-Za-z0-9_]*))/);
		if (marker) heredoc = marker[1] ?? marker[2] ?? marker[3];
		for (const segment of rawLine.split(/;|&&|\|\||\||\(|\)/)) {
			const tokens = [];
			for (const token of segment.trim().split(/\s+/)) {
				if (token === "") continue;
				if (token.startsWith("#")) break;
				tokens.push(token);
			}
			if (tokens.length > 0) yield tokens;
		}
	}
}

/** Returns the reasons a simple command would execute or read repository code. */
export function repositoryCodeReasons(tokens) {
	const reasons = [];
	for (const token of tokens) {
		if (token.includes("://")) continue; // a URL, e.g. the cosign certificate identity
		if (CHECKOUT_PATH.test(stripQuotes(token)) || WORKSPACE_PATH.test(token)) {
			reasons.push(`references the checkout: ${token}`);
		}
	}
	let index = 0;
	while (index < tokens.length && (ASSIGNMENT.test(tokens[index]) || SHELL_KEYWORDS.test(tokens[index]))) index += 1;
	if (index >= tokens.length) return reasons;
	const command = stripQuotes(tokens[index]);
	const args = tokens.slice(index + 1);
	if (/^\.{1,2}\//.test(command) || (command.includes("/") && !command.startsWith("/"))) {
		reasons.push(`executes a relative path: ${command}`);
	}
	if (PACKAGE_MANAGERS.test(command)) {
		const sub = stripQuotes(args[0] ?? "");
		if (command !== "npm" || !/^(publish|--version|-v|view|config)$/.test(sub)) {
			reasons.push(`runs a package manager: ${command} ${sub}`.trim());
		}
	}
	if (SOURCE_COMMANDS.test(command) && args.length > 0) {
		reasons.push(`sources a file: ${command} ${args[0]}`);
	}
	if (INTERPRETERS.test(command)) {
		for (const arg of args) {
			if (arg === "-" || arg === "--") break;
			if (/^-[cem]$/.test(arg) || arg === "--eval" || arg === "--print" || arg === "-p") break; // inline code follows
			if (arg.startsWith("-")) continue;
			if (looksLikePath(arg)) reasons.push(`runs a file through ${command}: ${arg}`);
			break;
		}
	}
	return reasons;
}

/** True when a job holds something an attacker could exfiltrate or misuse. */
export function isCredentialBearing(job) {
	if (job.environment) return true;
	for (const value of Object.values(job.permissions ?? {})) {
		if (value === "write") return true;
	}
	const envs = [job.env ?? {}, ...(job.steps ?? []).map((step) => step.env ?? {})];
	for (const env of envs) {
		for (const value of Object.values(env)) {
			const text = String(value);
			if (text.includes("secrets.") && !/secrets\.GITHUB_TOKEN\b/.test(text)) return true;
		}
	}
	return false;
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
			const text = String(value);
			if (text.includes("secrets.") && !text.includes("secrets.GITHUB_TOKEN")) {
				fail(`${RELEASE_WORKFLOW}: job '${jobId}' exposes ${name} to every step; move it to the step that uses it.`);
			}
		}
		if (!isCredentialBearing(job)) continue;
		const allowed = ALLOWED_ACTIONS[jobId] ?? [];
		for (const step of job.steps ?? []) {
			const label = step.name ?? step.uses ?? "(unnamed step)";
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
			for (const tokens of shellCommands(step.run ?? "")) {
				for (const reason of repositoryCodeReasons(tokens)) {
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
	const POINTER_WRITE = /aws s3 cp[^\n]*\b(stable|latest\.json|install\.sh|install-beta\.sh)\b|s3:\/\/\$\{R2_BUCKET\}\/(stable|latest\.json|install\.sh|install-beta\.sh)\b|publish_pointer\s/;
	if (release.jobs[POINTER_JOB]) {
		const steps = release.jobs[POINTER_JOB].steps ?? [];
		const pointerIndex = steps.findIndex((step) => POINTER_WRITE.test(String(step.run ?? "")));
		if (pointerIndex === -1) {
			fail(`${RELEASE_WORKFLOW}: job '${POINTER_JOB}' must advance the production channel pointers.`);
		} else if (pointerIndex !== steps.length - 1) {
			fail(`${RELEASE_WORKFLOW}: job '${POINTER_JOB}' must advance the channel pointers in its last step.`);
		}
		const publishIndex = steps.findIndex((step) => /gh release edit .*--draft=false/.test(String(step.run ?? "")));
		if (publishIndex === -1 || publishIndex > pointerIndex) {
			fail(`${RELEASE_WORKFLOW}: job '${POINTER_JOB}' must publish the GitHub release before it moves the channel pointers.`);
		}
	}

	for (const [jobId, job] of Object.entries(release.jobs)) {
		if (jobId === POINTER_JOB) continue;
		for (const step of job.steps ?? []) {
			const run = String(step.run ?? "");
			if (!POINTER_WRITE.test(run)) continue;
			const written = run.match(POINTER_WRITE)[0];
			if (jobId === "publish-beta-r2") {
				fail(`${RELEASE_WORKFLOW}: the beta channel must never write a production pointer (${written.trim()}).`);
			} else {
				fail(`${RELEASE_WORKFLOW}: only '${POINTER_JOB}' may write a production pointer; '${jobId}' does (${written.trim()}).`);
			}
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
			}
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
