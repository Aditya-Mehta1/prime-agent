import assert from "node:assert/strict";
import { test } from "node:test";

import {
	MANUAL_ENVIRONMENT,
	PRODUCTION_ENVIRONMENT,
	ReleaseContextError,
	resolveReleaseContext,
} from "../resolve-release-context.mjs";

const SHA = "1111111111111111111111111111111111111111";
const BEFORE = "2222222222222222222222222222222222222222";

const human = { login: "kevin", type: "User" };
const bot = { login: "github-actions[bot]", type: "Bot" };

function makeDeps(overrides = {}) {
	const state = {
		packageVersion: "0.9.5",
		previousVersions: { [BEFORE]: "0.9.4" },
		tags: new Set(),
		tagCommits: {},
		pulls: [],
		reviews: {},
		...overrides,
	};
	return {
		state,
		readPackageVersion: () => state.packageVersion,
		readPackageVersionAt: (ref) => state.previousVersions[ref],
		gitHasPath: (ref) => Object.hasOwn(state.previousVersions, ref),
		gitTagExists: (tag) => state.tags.has(tag),
		gitTagCommit: (tag) => state.tagCommits[tag],
		ghJson: (args) => {
			const route = args[args.length - 1];
			if (route.endsWith("/pulls")) return state.pulls;
			const match = route.match(/pulls\/(\d+)\/reviews$/);
			if (match) return state.reviews[match[1]] ?? [];
			throw new Error(`unexpected gh route ${route}`);
		},
	};
}

function pushEnv(overrides = {}) {
	return {
		EVENT_NAME: "push",
		REF_NAME: "main",
		REF_TYPE: "branch",
		DEFAULT_BRANCH: "main",
		GITHUB_SHA_VALUE: SHA,
		GITHUB_REPOSITORY: "PrimeIntellect-ai/prime-agent",
		BEFORE_SHA: BEFORE,
		RUN_NUMBER: "42",
		RUN_ATTEMPT: "1",
		...overrides,
	};
}

test("approved release pull request publishes without a reviewer", () => {
	const deps = makeDeps({
		pulls: [{ number: 7, merged_at: "2026-09-15T00:00:00Z", merge_commit_sha: SHA }],
		reviews: { 7: [{ state: "APPROVED", user: human }] },
	});
	const { outputs } = resolveReleaseContext(pushEnv(), deps);
	assert.equal(outputs.publish_production, "true");
	assert.equal(outputs.publish_beta, "true");
	assert.equal(outputs.production_version, "0.9.5");
	assert.equal(outputs.requires_approval, "false");
	assert.equal(outputs.publish_environment, PRODUCTION_ENVIRONMENT);
});

test("bot approval does not count as a human approval", () => {
	const deps = makeDeps({
		pulls: [{ number: 7, merged_at: "2026-09-15T00:00:00Z", merge_commit_sha: SHA }],
		reviews: { 7: [{ state: "APPROVED", user: bot }, { state: "COMMENTED", user: human }] },
	});
	const { outputs } = resolveReleaseContext(pushEnv(), deps);
	assert.equal(outputs.requires_approval, "true");
	assert.equal(outputs.publish_environment, MANUAL_ENVIRONMENT);
});

test("pull request without an approving review needs a reviewer", () => {
	const deps = makeDeps({
		pulls: [{ number: 9, merged_at: "2026-09-15T00:00:00Z", merge_commit_sha: SHA }],
		reviews: { 9: [{ state: "CHANGES_REQUESTED", user: human }] },
	});
	const { outputs } = resolveReleaseContext(pushEnv(), deps);
	assert.equal(outputs.requires_approval, "true");
	assert.match(outputs.approval_reason, /no approving review/);
});

test("direct push with no pull request needs a reviewer", () => {
	const deps = makeDeps({ pulls: [] });
	const { outputs } = resolveReleaseContext(pushEnv(), deps);
	assert.equal(outputs.publish_production, "true");
	assert.equal(outputs.requires_approval, "true");
	assert.equal(outputs.publish_environment, MANUAL_ENVIRONMENT);
});

test("a pull request that only contains the commit is not the merge commit", () => {
	const deps = makeDeps({
		pulls: [{ number: 11, merged_at: "2026-09-15T00:00:00Z", merge_commit_sha: "cafe" }],
		reviews: { 11: [{ state: "APPROVED", user: human }] },
	});
	const { outputs } = resolveReleaseContext(pushEnv(), deps);
	assert.equal(outputs.requires_approval, "true");
});

test("retry riding an unrelated approved merge needs a reviewer", () => {
	// The version did not change on this commit; the release is only retried
	// because v0.9.5 has no tag yet.
	const deps = makeDeps({
		previousVersions: { [BEFORE]: "0.9.5" },
		pulls: [{ number: 12, merged_at: "2026-09-15T00:00:00Z", merge_commit_sha: SHA }],
		reviews: { 12: [{ state: "APPROVED", user: human }] },
	});
	const { outputs } = resolveReleaseContext(pushEnv(), deps);
	assert.equal(outputs.publish_production, "true");
	assert.equal(outputs.requires_approval, "true");
	assert.match(outputs.approval_reason, /did not bump the version/);
});

test("unchanged version with an existing tag only advances beta", () => {
	const deps = makeDeps({
		previousVersions: { [BEFORE]: "0.9.5" },
		tags: new Set(["v0.9.5"]),
		tagCommits: { "v0.9.5": SHA },
	});
	const { outputs } = resolveReleaseContext(pushEnv(), deps);
	assert.equal(outputs.publish_production, "false");
	assert.equal(outputs.publish_beta, "true");
	assert.equal(outputs.requires_approval, "false");
});

test("a re-run of the release commit keeps its approval", () => {
	const deps = makeDeps({
		tags: new Set(["v0.9.5"]),
		tagCommits: { "v0.9.5": SHA },
		pulls: [{ number: 7, merged_at: "2026-09-15T00:00:00Z", merge_commit_sha: SHA }],
		reviews: { 7: [{ state: "APPROVED", user: human }] },
	});
	const { outputs } = resolveReleaseContext(pushEnv({ RUN_ATTEMPT: "2" }), deps);
	assert.equal(outputs.publish_production, "true");
	assert.equal(outputs.requires_approval, "false");
});

test("a version already tagged at another commit is refused", () => {
	const deps = makeDeps({
		tags: new Set(["v0.9.5"]),
		tagCommits: { "v0.9.5": "9999999999999999999999999999999999999999" },
	});
	assert.throws(() => resolveReleaseContext(pushEnv(), deps), ReleaseContextError);
});

test("workflow_dispatch always requires a reviewer", () => {
	const deps = makeDeps();
	const env = pushEnv({ EVENT_NAME: "workflow_dispatch", INPUT_RELEASE_TAG: "v0.9.5" });
	const { outputs } = resolveReleaseContext(env, deps);
	assert.equal(outputs.publish_production, "true");
	assert.equal(outputs.publish_beta, "false");
	assert.equal(outputs.requires_approval, "true");
	assert.equal(outputs.publish_environment, MANUAL_ENVIRONMENT);
});

test("workflow_dispatch refuses a version that is not package.json", () => {
	const deps = makeDeps();
	const env = pushEnv({ EVENT_NAME: "workflow_dispatch", INPUT_RELEASE_TAG: "v9.9.9" });
	assert.throws(() => resolveReleaseContext(env, deps), ReleaseContextError);
});

test("workflow_dispatch refuses a non-default branch", () => {
	const deps = makeDeps();
	const env = pushEnv({ EVENT_NAME: "workflow_dispatch", INPUT_RELEASE_TAG: "v0.9.5", REF_NAME: "feature" });
	assert.throws(() => resolveReleaseContext(env, deps), ReleaseContextError);
});

test("pull requests validate both packers and publish nothing", () => {
	const deps = makeDeps();
	const env = pushEnv({ EVENT_NAME: "pull_request" });
	const { outputs } = resolveReleaseContext(env, deps);
	assert.equal(outputs.publish_production, "true");
	assert.equal(outputs.publish_beta, "true");
	assert.equal(outputs.requires_approval, "false");
	assert.equal(outputs.beta_version, "0.9.5-beta.42.1.1111111");
});

test("a pushed tag no longer releases", () => {
	const deps = makeDeps();
	const env = pushEnv({ REF_TYPE: "tag", REF_NAME: "v0.9.5" });
	assert.throws(() => resolveReleaseContext(env, deps), ReleaseContextError);
});

test("a non-semver production version is refused", () => {
	const deps = makeDeps({ packageVersion: "0.9.5-rc.1" });
	assert.throws(() => resolveReleaseContext(pushEnv(), deps), ReleaseContextError);
});
