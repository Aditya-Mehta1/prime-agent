import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { parse } from "yaml";

import { CREDENTIAL_JOBS, checkWorkflows, isCredentialBearing, repositoryCodeReasons, shellCommands } from "../check-release-workflow.mjs";

const RELEASE = ".github/workflows/build-binaries.yml";
const STANDALONE = ".github/workflows/standalone-binaries.yml";

function reader(overrides = {}) {
	return (path) => overrides[path] ?? readFileSync(path, "utf8");
}

function mutate(path, mutateText) {
	const original = readFileSync(path, "utf8");
	const mutated = mutateText(original);
	assert.notEqual(mutated, original, "the mutation did not change the workflow; the anchor text moved");
	return mutated;
}

/** Appends a `run` step to the named job (which must already have at least one step). */
function appendStep(text, jobId, stepYaml) {
	const document = parse(text);
	const job = document.jobs[jobId];
	assert.ok(job, `job ${jobId} exists`);
	const lastStep = job.steps[job.steps.length - 1];
	const nextJob = Object.keys(document.jobs)[Object.keys(document.jobs).indexOf(jobId) + 1];
	const anchor = nextJob ? `\n  ${nextJob}:\n` : null;
	const insertAt = anchor ? text.indexOf(anchor) : text.length;
	assert.ok(insertAt > 0, `found the end of job ${jobId} (${lastStep.name})`);
	return `${text.slice(0, insertAt)}\n${stepYaml}${text.slice(insertAt)}`;
}

function runStep(name, script) {
	const body = script
		.split("\n")
		.map((line) => `          ${line}`)
		.join("\n");
	return `      - name: ${name}\n        run: |\n${body}\n`;
}

test("the checked-in workflows satisfy every invariant", () => {
	assert.deepEqual(checkWorkflows(), []);
});

test("every job that publishes holds its credential in a protected environment", () => {
	const release = parse(readFileSync(RELEASE, "utf8"));
	for (const jobId of CREDENTIAL_JOBS) {
		assert.ok(release.jobs[jobId], `${jobId} exists`);
		assert.ok(release.jobs[jobId].environment, `${jobId} runs in an environment`);
		assert.ok(isCredentialBearing(release.jobs[jobId]), `${jobId} is recognised as credential-bearing`);
	}
	// A contents:write GITHUB_TOKEN can push tags and publish releases: it is a credential.
	assert.ok(isCredentialBearing(release.jobs["github-release"]));
	assert.ok(isCredentialBearing(release.jobs["github-release-beta"]));
	// OIDC minting is a credential too.
	assert.ok(isCredentialBearing(release.jobs.sign));
	// Jobs that run repository code hold nothing.
	for (const jobId of ["build", "assemble", "validate-macos", "pack-npm", "context", "verify"]) {
		assert.equal(isCredentialBearing(release.jobs[jobId]), false, `${jobId} holds no credential`);
	}
});

test("a tag trigger is rejected", () => {
	const broken = mutate(RELEASE, (text) => text.replace("  push:\n    branches:\n      - main\n", "  push:\n    branches:\n      - main\n    tags:\n      - 'v*'\n"));
	const problems = checkWorkflows(reader({ [RELEASE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("tag push")));
});

test("a wide workflow token is rejected", () => {
	const broken = mutate(RELEASE, (text) => text.replace("permissions: {}", "permissions:\n  contents: write"));
	const problems = checkWorkflows(reader({ [RELEASE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("permissions: {}")));
});

test("a job-level R2 secret is rejected", () => {
	const broken = mutate(RELEASE, (text) =>
		text.replace(
			"    env:\n      PRODUCTION_VERSION: ${{ needs.context.outputs.production_version }}\n    steps:\n      # No checkout, no dependency install",
			"    env:\n      PRODUCTION_VERSION: ${{ needs.context.outputs.production_version }}\n      AWS_ACCESS_KEY_ID: ${{ secrets.R2_ACCESS_KEY_ID }}\n    steps:\n      # No checkout, no dependency install",
		),
	);
	const problems = checkWorkflows(reader({ [RELEASE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("exposes AWS_ACCESS_KEY_ID")));
});

const CHECKOUT_STEP = "      - name: Sneak in a checkout\n        uses: actions/checkout@9c091bb21b7c1c1d1991bb908d89e4e9dddfe3e0 # v7.0.0\n";
const SETUP_NODE_STEP = "      - name: Sneak in a toolchain\n        uses: actions/setup-node@820762786026740c76f36085b0efc47a31fe5020 # v7.0.0\n";

for (const jobId of ["publish-r2", "finalize-release", "publish-beta-r2", "publish-npm", "tap-bump", "github-release"]) {
	test(`actions/checkout inside credential-bearing job ${jobId} is rejected`, () => {
		const broken = mutate(RELEASE, (text) => appendStep(text, jobId, CHECKOUT_STEP));
		const problems = checkWorkflows(reader({ [RELEASE]: broken }));
		assert.ok(
			problems.some((problem) => problem.includes(`'${jobId}'`) && problem.includes("actions/checkout")),
			problems.join("\n"),
		);
	});
}

test("actions/setup-node is rejected in R2 jobs but allowed for npm trusted publishing", () => {
	const broken = mutate(RELEASE, (text) => appendStep(text, "finalize-release", SETUP_NODE_STEP));
	const problems = checkWorkflows(reader({ [RELEASE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("'finalize-release'") && problem.includes("setup-node")));
	const release = parse(readFileSync(RELEASE, "utf8"));
	assert.ok(release.jobs["publish-npm"].steps.some((step) => step.uses?.startsWith("actions/setup-node@")));
	assert.deepEqual(checkWorkflows(), []);
});

const EVASIONS = [
	["node scripts/x.mjs", "node scripts/pack-npm-packages.mjs --out-dir npm-packages"],
	["bash scripts/x.sh", "bash scripts/publish.sh"],
	["sh with a relative script", "sh ./publish.sh"],
	["./scripts/x.sh in command position", "./scripts/publish.sh --yes"],
	["python3 scripts/x.py", "python3 scripts/publish.py"],
	["source scripts/x", "source scripts/env.sh"],
	[". scripts/x", ". scripts/env.sh"],
	["a script under packages/", "node packages/coding-agent/scripts/publish.mjs"],
	["a script under .github/", "bash .github/scripts/publish.sh"],
	["$GITHUB_WORKSPACE/scripts", 'bash "$GITHUB_WORKSPACE/scripts/publish.sh"'],
	["${GITHUB_WORKSPACE}/packages", 'node "${GITHUB_WORKSPACE}/packages/coding-agent/dist/cli.js"'],
	["an interpreter after a pipe", 'echo hi | node scripts/publish.mjs'],
	["an interpreter after &&", 'test -f x && node scripts/publish.mjs'],
	["a continued line", 'node \\\n  scripts/publish.mjs'],
	["a variable assignment prefix", 'FOO=bar node scripts/publish.mjs'],
	["npm ci", "npm ci --ignore-scripts"],
	["npm install", "npm install"],
	["npm rebuild", "npm rebuild esbuild"],
	["npm run", "npm run build"],
	["npm exec", "npm exec -- prime-agent"],
	["npx", "npx tsx scripts/publish.ts"],
	["pnpm", "pnpm install"],
	["yarn", "yarn"],
	["bun run", "bun run scripts/publish.ts"],
	["a relative executable", "extracted/prime-agent --version"],
	["a local composite action", null],
];

for (const jobId of ["publish-r2", "finalize-release", "publish-npm"]) {
	for (const [label, script] of EVASIONS) {
		test(`${label} inside credential-bearing job ${jobId} is rejected`, () => {
			const step =
				script === null
					? "      - name: Sneak in a local action\n        uses: ./.github/actions/publish\n"
					: runStep("Sneak in repository code", `set -euo pipefail\n${script}`);
			const broken = mutate(RELEASE, (text) => appendStep(text, jobId, step));
			const problems = checkWorkflows(reader({ [RELEASE]: broken }));
			assert.ok(
				problems.some((problem) => problem.includes(`'${jobId}'`) && /must not run repository code|local action/.test(problem)),
				`expected a repository-code finding for ${jobId}, got:\n${problems.join("\n")}`,
			);
		});
	}
}

test("the shell matcher understands continuations, heredocs, pipes and comments", () => {
	const commands = [...shellCommands("digest=$(grep -E 'x' \\\n  \"$GITHUB_WORKSPACE/artifacts/SHA256SUMS\" | cut -d' ' -f1)\npython3 - \"$formula\" <<'PY'\nimport scripts/evil\nPY\n# node scripts/comment.mjs\necho ok # node scripts/comment.mjs\n")];
	assert.deepEqual(commands, [
		["digest=$"],
		["grep", "-E", "'x'", '"$GITHUB_WORKSPACE/artifacts/SHA256SUMS"'],
		["cut", "-d'", "'", "-f1"],
		["python3", "-", '"$formula"', "<<'PY'"],
		["echo", "ok"],
	]);
	for (const tokens of commands) assert.deepEqual(repositoryCodeReasons(tokens), [], tokens.join(" "));
});

test("the shell matcher leaves legitimate credential-job commands alone", () => {
	const fine = [
		"aws s3 cp artifacts/latest.json s3://bucket/latest.json --quiet",
		"gh release edit v1.2.3 --draft=false --latest",
		'cosign verify-blob --certificate-identity "https://github.com/o/r/.github/workflows/build-binaries.yml@refs/heads/main" SHA256SUMS',
		"npm publish npm-packages/artifacts/x.tgz --provenance --access public --ignore-scripts",
		"jq -r '.publishOrder[]' npm-packages/manifest.json",
		"python3 - \"$formula\" \"$platform\"",
		"bash -c 'echo hi'",
		"node -e 'console.log(1)'",
		"sha256sum --check SHA256SUMS",
		"test -f artifacts/install.sh",
		"grep -E 'x' \"$GITHUB_WORKSPACE/artifacts/SHA256SUMS\"",
		"/usr/bin/env true",
	];
	for (const line of fine) {
		for (const tokens of shellCommands(line)) assert.deepEqual(repositoryCodeReasons(tokens), [], line);
	}
});

test("the pointers may only move in finalize-release, after the release is published", () => {
	// publish-r2 moving stable is exactly the ordering bug this file exists to prevent.
	const broken = mutate(RELEASE, (text) =>
		appendStep(text, "publish-r2", runStep("Advance early", 'aws s3 cp artifacts/stable "s3://${R2_BUCKET}/stable" --quiet')),
	);
	let problems = checkWorkflows(reader({ [RELEASE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("only 'finalize-release' may write a production pointer")));

	const beta = mutate(RELEASE, (text) =>
		text.replace(
			'          aws s3 cp artifacts/beta "s3://${R2_BUCKET}/beta" \\',
			'          aws s3 cp artifacts/install.sh "s3://${R2_BUCKET}/install.sh"\n          aws s3 cp artifacts/beta "s3://${R2_BUCKET}/beta" \\',
		),
	);
	problems = checkWorkflows(reader({ [RELEASE]: beta }));
	assert.ok(problems.some((problem) => problem.includes("beta channel must never write a production pointer")));

	// A step after the pointer step in finalize-release is rejected: pointers move last.
	const late = mutate(RELEASE, (text) => appendStep(text, "finalize-release", runStep("Afterthought", "echo done")));
	problems = checkWorkflows(reader({ [RELEASE]: late }));
	assert.ok(problems.some((problem) => problem.includes("in its last step")));

	// Moving the pointers before the release is undrafted is rejected.
	const early = mutate(RELEASE, (text) =>
		text.replace(
			"      - name: Publish the release and prove the tag points at BUILD_REF\n",
			`${runStep("Advance first", 'publish_pointer artifacts/stable stable text/plain')}\n      - name: Publish the release and prove the tag points at BUILD_REF\n`,
		),
	);
	problems = checkWorkflows(reader({ [RELEASE]: early }));
	assert.ok(problems.some((problem) => problem.includes("must publish the GitHub release before it moves the channel pointers")));
});

test("the publication order is enforced: github-release -> publish-r2 -> verify -> finalize-release -> npm/tap", () => {
	for (const [later, earlier, replacement] of [
		["verify", "publish-r2", "needs: [context, github-release]"],
		["finalize-release", "verify", "needs: [context, publish-r2]"],
		["publish-npm", "finalize-release", "needs: [context, pack-npm]"],
		["tap-bump", "finalize-release", "needs: [context, verify]"],
	]) {
		const release = parse(readFileSync(RELEASE, "utf8"));
		const current = `needs: [${release.jobs[later].needs.join(", ")}]`;
		const broken = mutate(RELEASE, (text) => {
			const start = text.indexOf(`\n  ${later}:\n`);
			const index = text.indexOf(current, start);
			assert.ok(index > start, `${later} declares ${current}`);
			return `${text.slice(0, index)}${replacement}${text.slice(index + current.length)}`;
		});
		const problems = checkWorkflows(reader({ [RELEASE]: broken }));
		assert.ok(
			problems.some((problem) => problem.includes(`'${later}' must need '${earlier}'`)),
			`${later} without ${earlier}:\n${problems.join("\n")}`,
		);
	}
});

test("a credential job outside a protected environment is rejected", () => {
	const broken = mutate(RELEASE, (text) =>
		text.replace(
			"    environment:\n      name: release-npm\n",
			"",
		),
	);
	const problems = checkWorkflows(reader({ [RELEASE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("'publish-npm' must run in a protected environment")));
});

test("npm ci without --ignore-scripts is rejected", () => {
	const broken = mutate(STANDALONE, (text) => text.replace("npm ci --ignore-scripts", "npm ci"));
	const problems = checkWorkflows(reader({ [STANDALONE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("--ignore-scripts")));
});

test("an unpinned action is rejected", () => {
	const broken = mutate(STANDALONE, (text) =>
		text.replace("oven-sh/setup-bun@0c5077e51419868618aeaa5fe8019c62421857d6 # v2", "oven-sh/setup-bun@v2"),
	);
	const problems = checkWorkflows(reader({ [STANDALONE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("without a full commit SHA")));
});
