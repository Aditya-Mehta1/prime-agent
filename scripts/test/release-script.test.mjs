import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const script = readFileSync(join(root, "scripts", "release.mjs"), "utf8");

// A release is a reviewed pull request. The preparation script must never be able to publish one
// on its own, so it may not push the default branch and may not create the release tag: the tag is
// created by the release workflow only after the artifacts are published.
test("the release script never pushes main", () => {
	assert.equal(/git push origin main/.test(script), false);
	assert.equal(/git push\s+(-u\s+)?origin\s+main/.test(script), false);
});

test("the release script never creates the release tag", () => {
	assert.equal(/git tag/.test(script), false);
	assert.equal(/git push origin v\$/.test(script), false);
});

test("the release script pushes a release branch instead", () => {
	assert.match(script, /release\/v\$\{version\}/);
	assert.match(script, /git push -u origin \$\{releaseBranch\}/);
});

test("the release script opens the pull request but cannot approve or merge it", () => {
	assert.match(script, /gh pr create --base main --head \$\{branch\}/);
	assert.equal(/gh pr merge/.test(script), false);
	assert.equal(/gh pr review/.test(script), false);
	assert.equal(/--admin/.test(script), false);
});
