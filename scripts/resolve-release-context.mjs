#!/usr/bin/env node
/**
 * Resolves the release context for .github/workflows/build-binaries.yml.
 *
 * Besides the version/ref bookkeeping the release workflow has always done, this
 * script decides whether a production publish may run unattended. Production
 * publishing is unattended ONLY when the head commit resolves to a merged pull
 * request that bumped the version and carried at least one approving review from
 * a human. Everything else (direct push, workflow_dispatch, a retry riding an
 * unrelated merge, bot-only approvals) is routed to the `release-manual`
 * environment, which has required reviewers.
 *
 * The module exports pure-ish functions so the decision table can be unit tested
 * without a runner; `main()` wires the real git/gh/package.json implementations.
 */

import { execFileSync } from "node:child_process";
import { appendFileSync, readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const SEMVER_RE = /^[0-9]+\.[0-9]+\.[0-9]+$/;

export const PRODUCTION_ENVIRONMENT = "release-r2";
export const MANUAL_ENVIRONMENT = "release-manual";

export class ReleaseContextError extends Error {}

function isHumanReviewer(user) {
	if (!user || typeof user.login !== "string") return false;
	if (user.type && user.type.toLowerCase() === "bot") return false;
	return !user.login.endsWith("[bot]");
}

/**
 * Looks for a merged pull request whose merge commit is exactly `sha`, with at
 * least one APPROVED review left by a human.
 */
export function findApprovingPullRequest(deps, repository, sha) {
	const pulls = deps.ghJson(["api", "-H", "Accept: application/vnd.github+json", `repos/${repository}/commits/${sha}/pulls`]);
	if (!Array.isArray(pulls)) {
		return { approved: false, reason: `Could not list pull requests for ${sha}.` };
	}
	const merged = pulls.filter((pull) => pull && pull.merged_at && pull.merge_commit_sha === sha);
	if (merged.length === 0) {
		return { approved: false, reason: `No merged pull request has ${sha} as its merge commit.` };
	}
	for (const pull of merged) {
		const reviews = deps.ghJson([
			"api",
			"--paginate",
			"-H",
			"Accept: application/vnd.github+json",
			`repos/${repository}/pulls/${pull.number}/reviews`,
		]);
		if (!Array.isArray(reviews)) continue;
		const approval = reviews.find((review) => review && review.state === "APPROVED" && isHumanReviewer(review.user));
		if (approval) {
			return {
				approved: true,
				reason: `Pull request #${pull.number} was approved by @${approval.user.login}.`,
				pullNumber: pull.number,
			};
		}
	}
	const numbers = merged.map((pull) => `#${pull.number}`).join(", ");
	return { approved: false, reason: `Merged pull request ${numbers} has no approving review from a human.` };
}

export function resolveReleaseContext(env, deps) {
	const logs = [];
	const log = (message) => logs.push(message);

	const eventName = env.EVENT_NAME;
	const sha = env.GITHUB_SHA_VALUE;
	const repository = env.GITHUB_REPOSITORY;
	const defaultBranch = env.DEFAULT_BRANCH;
	const packageVersion = deps.readPackageVersion();

	let betaVersion = "";
	let buildRef = "";
	let productionVersion = "";
	let publishBeta = false;
	let publishProduction = false;
	let requiresApproval = true;
	let approvalReason = "Production publishing requires a reviewer by default.";

	const betaFor = (version) =>
		`${version}-beta.${env.RUN_NUMBER}.${env.RUN_ATTEMPT}.${String(sha).slice(0, 7)}`;

	if (eventName === "pull_request") {
		// Exercise both packer paths without release credentials or publication.
		productionVersion = packageVersion;
		buildRef = sha;
		betaVersion = betaFor(productionVersion);
		publishBeta = true;
		publishProduction = true;
		requiresApproval = false;
		approvalReason = "Pull request validation never publishes.";
	} else if (eventName === "workflow_dispatch") {
		if (env.REF_NAME !== defaultBranch) {
			throw new ReleaseContextError(
				`Manual releases must run from the default branch (${defaultBranch}), not ${env.REF_NAME}.`,
			);
		}
		productionVersion = String(env.INPUT_RELEASE_TAG || "").replace(/^v/, "");
		if (productionVersion !== packageVersion) {
			throw new ReleaseContextError(
				`Manual release tag v${productionVersion} does not match package.json (${packageVersion}).`,
			);
		}
		buildRef = sha;
		publishProduction = true;
		requiresApproval = true;
		approvalReason = "workflow_dispatch is the break-glass path and always needs a reviewer.";
	} else if (env.REF_TYPE === "tag") {
		throw new ReleaseContextError(
			`Tag pushes no longer release. Tag ${env.REF_NAME} was created outside the release job; delete it or dispatch a manual release.`,
		);
	} else {
		productionVersion = packageVersion;
		buildRef = sha;
		betaVersion = betaFor(productionVersion);
		publishBeta = true;

		let previousVersion = "";
		const beforeSha = env.BEFORE_SHA || "";
		if (beforeSha && !/^0+$/.test(beforeSha) && deps.gitHasPath(beforeSha, "package.json")) {
			previousVersion = deps.readPackageVersionAt(beforeSha);
		}

		const versionChanged = Boolean(previousVersion) && previousVersion !== productionVersion;
		let retry = false;
		if (versionChanged || !previousVersion) {
			if (deps.gitTagExists(`v${productionVersion}`)) {
				const taggedCommit = deps.gitTagCommit(`v${productionVersion}`);
				if (taggedCommit !== sha) {
					throw new ReleaseContextError(
						`Production v${productionVersion} already points to ${taggedCommit}, not ${sha}.`,
					);
				}
				log(`Retrying production v${productionVersion} for ${sha}.`);
			}
			publishProduction = true;
		} else if (!deps.gitTagExists(`v${productionVersion}`)) {
			log(`Production v${productionVersion} has no tag; retrying the failed release.`);
			publishProduction = true;
			retry = true;
		} else {
			log(`Package version is unchanged at ${productionVersion}; only beta will advance.`);
		}

		if (publishProduction) {
			if (retry) {
				requiresApproval = true;
				approvalReason = `${sha} did not bump the version; a retry needs a reviewer.`;
			} else if (productionVersion !== packageVersion) {
				requiresApproval = true;
				approvalReason = `Resolved version ${productionVersion} does not match package.json (${packageVersion}).`;
			} else {
				const verdict = findApprovingPullRequest(deps, repository, sha);
				requiresApproval = !verdict.approved;
				approvalReason = verdict.reason;
			}
		} else {
			requiresApproval = false;
			approvalReason = "No production publish is scheduled.";
		}
	}

	if (publishProduction && !SEMVER_RE.test(productionVersion)) {
		throw new ReleaseContextError(`Production version must be plain semver like 0.0.1: ${productionVersion}`);
	}

	// Fail safe: anything that is not a verified, approved production release
	// is routed to the environment that has required reviewers.
	const publishEnvironment = publishProduction && !requiresApproval ? PRODUCTION_ENVIRONMENT : MANUAL_ENVIRONMENT;

	log(`Build ref: ${buildRef}`);
	log(`Production: ${publishProduction}${productionVersion ? ` v${productionVersion}` : ""}`);
	log(`Beta: ${publishBeta}${betaVersion ? ` v${betaVersion}` : ""}`);
	log(`Requires approval: ${requiresApproval} (${approvalReason})`);
	log(`Publish environment: ${publishEnvironment}`);

	return {
		outputs: {
			beta_version: betaVersion,
			build_ref: buildRef,
			production_version: productionVersion,
			publish_beta: String(publishBeta),
			publish_production: String(publishProduction),
			requires_approval: String(requiresApproval),
			publish_environment: publishEnvironment,
			approval_reason: approvalReason,
		},
		logs,
	};
}

function realDeps() {
	const git = (args) => execFileSync("git", args, { encoding: "utf8" }).trim();
	const gitOk = (args) => {
		try {
			execFileSync("git", args, { stdio: "ignore" });
			return true;
		} catch {
			return false;
		}
	};
	return {
		readPackageVersion: () => JSON.parse(readFileSync("package.json", "utf8")).version,
		readPackageVersionAt: (ref) => JSON.parse(git(["show", `${ref}:package.json`])).version,
		gitHasPath: (ref, path) => gitOk(["cat-file", "-e", `${ref}:${path}`]),
		gitTagExists: (tag) => gitOk(["show-ref", "--verify", "--quiet", `refs/tags/${tag}`]),
		gitTagCommit: (tag) => git(["rev-list", "-n", "1", tag]),
		ghJson: (args) => {
			const stdout = execFileSync("gh", args, { encoding: "utf8", maxBuffer: 32 * 1024 * 1024 });
			return JSON.parse(stdout);
		},
	};
}

export function main(env = process.env, deps = realDeps(), write = defaultWrite) {
	const { outputs, logs } = resolveReleaseContext(env, deps);
	for (const line of logs) console.log(line);
	write(env, outputs);
	return outputs;
}

function defaultWrite(env, outputs) {
	if (!env.GITHUB_OUTPUT) return;
	const body = Object.entries(outputs)
		.map(([key, value]) => `${key}=${value}`)
		.join("\n");
	appendFileSync(env.GITHUB_OUTPUT, `${body}\n`);
}

const invokedDirectly = process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;
if (invokedDirectly) {
	try {
		main();
	} catch (error) {
		if (error instanceof ReleaseContextError) {
			console.error(error.message);
			process.exit(1);
		}
		throw error;
	}
}
