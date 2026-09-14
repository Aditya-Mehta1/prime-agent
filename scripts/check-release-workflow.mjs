#!/usr/bin/env node
/**
 * Release pipeline invariants.
 *
 * These are the properties that make the release safe to run with 45 people
 * holding write access. They are cheap to break by accident in a YAML edit, so
 * they are asserted in CI instead of in a review checklist.
 */

import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

import { parse } from "yaml";

const RELEASE_WORKFLOW = ".github/workflows/build-binaries.yml";
const BUILD_WORKFLOWS = [RELEASE_WORKFLOW, ".github/workflows/standalone-binaries.yml"];
const SHA_PIN = /@[0-9a-f]{40}$/;
const CREDENTIAL_JOBS = ["publish-r2", "publish-beta-r2"];
const FORBIDDEN_IN_CREDENTIAL_JOBS = [
	/actions\/checkout/,
	/actions\/setup-node/,
	/\bnpm\s+(ci|install|rebuild)\b/,
	/\bnode\s+scripts\//,
	/\bnpx\b/,
];
const PRODUCTION_ONLY_OBJECTS = ["install.sh", "install-beta.sh", "stable", "latest.json"];

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
		const body = JSON.stringify(job.steps ?? []);
		for (const pattern of FORBIDDEN_IN_CREDENTIAL_JOBS) {
			if (pattern.test(body)) {
				fail(`${RELEASE_WORKFLOW}: job '${jobId}' must not run repository code (${pattern}).`);
			}
		}
	}

	const betaBody = JSON.stringify(release.jobs["publish-beta-r2"]?.steps ?? []);
	for (const object of PRODUCTION_ONLY_OBJECTS) {
		const uploads = new RegExp(`s3://\\$\\{R2_BUCKET\\}/${object.replace(".", "\\.")}`);
		if (uploads.test(betaBody)) {
			fail(`${RELEASE_WORKFLOW}: the beta channel must never write ${object}.`);
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
