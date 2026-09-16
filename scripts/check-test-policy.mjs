#!/usr/bin/env node
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { relative, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const testFilePattern =
	/(?:^|\/)(?:test|tests|__tests__)(?:\/|$)|\.(?:test|spec)\.[cm]?[jt]sx?$|^prime-agent-runtime\/test\/.*\.py$/;

function git(args, allowFailure = false) {
	try {
		return execFileSync("git", args, { cwd: root, encoding: "utf8", stdio: ["ignore", "pipe", allowFailure ? "ignore" : "inherit"] }).trim();
	} catch (error) {
		if (allowFailure) return "";
		throw error;
	}
}

function resolveBase() {
	if (process.env.TEST_POLICY_BASE && git(["rev-parse", "--verify", process.env.TEST_POLICY_BASE], true)) {
		return process.env.TEST_POLICY_BASE;
	}
	if (process.env.GITHUB_BASE_REF) {
		const remote = `origin/${process.env.GITHUB_BASE_REF}`;
		if (git(["rev-parse", "--verify", remote], true)) return remote;
	}
	const originMain = git(["rev-parse", "--verify", "origin/main"], true);
	const head = git(["rev-parse", "HEAD"]);
	if (originMain && originMain !== head) return "origin/main";
	return git(["rev-parse", "--verify", "HEAD^"], true) ? "HEAD^" : undefined;
}

function walkFiles(dir, out = []) {
	for (const entry of readdirSync(dir)) {
		if (["node_modules", ".git", "dist"].includes(entry)) continue;
		const path = resolve(dir, entry);
		const stat = statSync(path);
		if (stat.isDirectory()) walkFiles(path, out);
		else {
			const rel = relative(root, path).replaceAll("\\", "/");
			if (testFilePattern.test(rel)) out.push(rel);
		}
	}
	return out;
}

function changedTestFiles(base) {
	if (!base) return [...walkFiles(resolve(root, "packages")), ...walkFiles(resolve(root, "prime-agent-runtime", "test"))];
	const roots = ["packages", "prime-agent-runtime/test", "scripts"];
	const tracked = git(["diff", "--name-only", "--diff-filter=ACMR", base, "--", ...roots]);
	const untracked = git(["ls-files", "--others", "--exclude-standard", "--", ...roots], true);
	return [...new Set(`${tracked}\n${untracked}`.split("\n"))].filter(
		(path) => path && testFilePattern.test(path) && existsSync(resolve(root, path)),
	);
}

function scan(content) {
	const lines = content.split("\n");
	const violations = [];
	let title = "<module>";
	const add = (category, line, detail, explicitTitle = title) => {
		const previous = lines[line - 2]?.trim() ?? "";
		const suppression = previous.match(/^(?:\/\/|#) test-policy: allow ([a-z-]+) -- (.+)$/);
		if (suppression?.[1] === category && suppression[2].trim().length >= 12) return;
		violations.push({ category, detail, identity: `${category}\0${explicitTitle}\0${detail}`, line, title: explicitTitle });
	};

	for (let index = 0; index < lines.length; index += 1) {
		const line = lines[index];
		const titleMatch = line.match(/\b(?:it|test)(?:\.[A-Za-z]+|\([^)]*\))*\(\s*["'`]([^"'`]+)/);
		const pythonTitleMatch = line.match(/^\s*(?:async\s+)?def\s+(test_[A-Za-z0-9_]+)/);
		if (titleMatch) title = titleMatch[1];
		else if (pythonTitleMatch) title = pythonTitleMatch[1];
		const modifier = line.match(
			/\b(?:it|test|describe|suite)(?:\.[A-Za-z]+|\([^)]*\))*\.(skipIf|runIf|skip|todo|only)\b|\b(xit|xtest|xdescribe)\s*\(/,
		);
		if (modifier) {
			const callStart = lines.slice(index, index + 4).join(" ");
			const ownTitle = callStart.match(/["'`]([^"'`]+)["'`]/)?.[1] ?? title;
			add("conditional-or-disabled-test", index + 1, `.${modifier[1] ?? modifier[2]}`, ownTitle);
		}
		const pythonModifier = line.match(/@(?:unittest\.)?(skipIf|skipUnless|skip)\b|@pytest\.mark\.(skipif|skip)\b/);
		const pythonDecoratorTitle =
			lines
				.slice(index, index + 4)
				.join(" ")
				.match(/\bdef\s+(test_[A-Za-z0-9_]+)/)?.[1] ?? title;
		if (pythonModifier) add("conditional-or-disabled-test", index + 1, pythonModifier[0], pythonDecoratorTitle);
		if (/@pytest\.mark\.(?:flaky|repeat)\b/.test(line)) add("test-retry", index + 1, line.trim(), pythonDecoratorTitle);
		if (/@pytest\.mark\.timeout\b/.test(line)) add("explicit-test-timeout", index + 1, "pytest timeout marker", pythonDecoratorTitle);
		if (/\b(?:it|test)(?:\.[A-Za-z]+(?:\([^)]*\))?)*\s*\([^\n]{0,240}\bretry\s*:/.test(line)) {
			add("test-retry", index + 1, "retry option");
		}
		if (/\b(?:testTimeout|hookTimeout)\s*:/.test(line)) add("explicit-test-timeout", index + 1, "timeout option");
		if (/^\s*}\s*,\s*\d[\d_]*\s*\)\s*;?\s*$/.test(line)) add("explicit-test-timeout", index + 1, "numeric timeout argument");
		if (/\b(?:expect\.poll|vi\.waitFor|waitForTimeout)\s*\(/.test(line)) add("wall-clock-poll", index + 1, line.match(/(?:expect\.poll|vi\.waitFor|waitForTimeout)/)?.[0] ?? "poll");
		if (/\b(?:sleep|delay)\s*\(/.test(line) && !/\b(?:sleep|delay)\s*[:=]/.test(line)) add("wall-clock-sleep", index + 1, "sleep/delay call");
		if (/\b(?:setTimeout|setInterval)\s*\(/.test(line)) add("wall-clock-timer", index + 1, "setTimeout/setInterval");
		if (/\bAtomics\.wait\s*\(/.test(line)) add("wall-clock-timer", index + 1, "Atomics.wait");
		if (/\.(?:listen|bind)\s*\(\s*[1-9][0-9_]*\b/.test(line)) add("fixed-resource", index + 1, "fixed bind/listen port");
		const controlWindow = lines.slice(index, index + 4).join(" ");
		if (
			/\bif\s*\([^)]*(?:process\.env|apiKey|credential|token|process\.platform|os\.(?:environ|getenv)|sys\.platform)/i.test(line) &&
			/\b(?:return|continue)\b/.test(controlWindow)
		) {
			add("environment-gated-path", index + 1, "conditional early exit");
		}
		if (/\b[A-Za-z_$][\w$.[\]]*\s*&&\s*expect\s*\(/.test(line)) add("optional-assertion", index + 1, "short-circuited assertion");
		if (/\bexpect\s*\(\s*(?:true|false|[-+]?\d+(?:\.\d+)?|["'][^"']*["'])\s*\)/.test(line)) {
			add("vacuous-assertion", index + 1, "literal expect");
		}
	}

	return violations;
}

function counts(violations) {
	const result = new Map();
	for (const violation of violations) result.set(violation.identity, (result.get(violation.identity) ?? 0) + 1);
	return result;
}

const base = resolveBase();
const failures = [];
for (const path of changedTestFiles(base)) {
	const current = scan(readFileSync(resolve(root, path), "utf8"));
	const oldContent = base ? git(["show", `${base}:${path}`], true) : "";
	const allowed = counts(oldContent ? scan(oldContent) : []);
	const seen = new Map();
	for (const violation of current) {
		const count = (seen.get(violation.identity) ?? 0) + 1;
		seen.set(violation.identity, count);
		if (count > (allowed.get(violation.identity) ?? 0)) failures.push({ path, ...violation });
	}
}

if (failures.length > 0) {
	console.error("New test-policy violations:\n");
	for (const failure of failures) console.error(`${failure.path}:${failure.line} [${failure.category}] ${failure.title}: ${failure.detail}`);
	console.error("\nUse a deterministic signal, deferred promise, fake timer, or unconditional local fixture instead.");
	process.exit(1);
}
console.log(`Test policy check passed${base ? ` against ${base}` : ""}.`);
