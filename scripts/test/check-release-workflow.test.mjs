import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { parse } from "yaml";

import {
	CREDENTIAL_JOBS,
	TEST_SIGNER_FLAG,
	TEST_SIGNER_STEP,
	checkWorkflows,
	isCredentialBearing,
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
