// Check the bundled Python skills: compile every skill source and run the
// skills' unittest fixture tests when present. Wired into `npm run check`
// (the CI build-check job runs it); mirrors the style of check-installer.mjs
// and check-browser-smoke.mjs. Requires python3 on PATH (CI has it).
import { spawnSync } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { join } from "node:path";

const PYTHON = process.env.PRIME_AGENT_PYTHON || "python3";
const skillsDir = join("packages", "coding-agent", "skills");

function runPython(args, cwd) {
	return spawnSync(PYTHON, args, {
		cwd,
		stdio: ["ignore", "pipe", "pipe"],
		encoding: "utf8",
		maxBuffer: 32 * 1024 * 1024,
	});
}

function pythonSkills(dir) {
	if (!existsSync(dir)) {
		console.error(`Skills directory not found: ${dir}`);
		process.exit(1);
	}
	return readdirSync(dir, { withFileTypes: true })
		.filter((entry) => entry.isDirectory() && existsSync(join(dir, entry.name, "pyproject.toml")))
		.map((entry) => entry.name)
		.sort();
}

const failures = [];
let compiled = 0;
let tested = 0;

for (const skill of pythonSkills(skillsDir)) {
	const skillDir = join(skillsDir, skill);

	if (existsSync(join(skillDir, "src"))) {
		const compile = runPython(["-m", "compileall", "-q", "src"], skillDir);
		compiled += 1;
		if (compile.status !== 0) {
			console.error(`✗ ${skill}: compileall failed`);
			console.error((compile.stderr || compile.stdout || "").trim());
			failures.push(`${skill}: compileall`);
			continue;
		}
	}

	if (!existsSync(join(skillDir, "tests"))) {
		continue;
	}
	const env = { ...process.env, PYTHONPATH: join(skillDir, "src") };
	const test = spawnSync(PYTHON, ["-m", "unittest", "discover", "-s", "tests", "-v"], {
		cwd: skillDir,
		env,
		stdio: ["ignore", "pipe", "pipe"],
		encoding: "utf8",
		maxBuffer: 32 * 1024 * 1024,
	});
	tested += 1;
	if (test.status !== 0) {
		console.error(`✗ ${skill}: tests failed`);
		console.error((test.stderr || test.stdout || "").trim());
		failures.push(`${skill}: tests`);
		continue;
	}
	console.log(`✓ ${skill}: tests passed`);
}

if (failures.length > 0) {
	console.error(`Python skills check failed: ${failures.join(", ")}`);
	process.exit(1);
}

console.log(`Python skills check passed (${compiled} compiled, ${tested} tested).`);
