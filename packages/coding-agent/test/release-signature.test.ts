import nodeCrypto from "node:crypto";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { crypto as sigstoreCrypto } from "@sigstore/core";
import { describe, expect, test } from "vitest";
import {
	fetchVerifiedReleaseArtifactDigest,
	parseChecksums,
	ReleaseSignatureError,
	verifyReleaseChecksums,
} from "../src/utils/release-signature.js";
import {
	buildExpectedSignerIdentity,
	PINNED_RELEASE_SIGNER,
	type PinnedSignerIdentity,
	RELEASE_SIGNER_OIDC_ISSUER,
} from "../src/utils/release-trust.js";

const fixtures = join(dirname(fileURLToPath(import.meta.url)), "fixtures", "release-signature");

const checksums = readFileSync(join(fixtures, "SHA256SUMS"));
const bundle = JSON.parse(readFileSync(join(fixtures, "SHA256SUMS.sigstore.json"), "utf8"));
const bundleWithoutTsa = JSON.parse(readFileSync(join(fixtures, "SHA256SUMS.no-tsa.sigstore.json"), "utf8"));

/**
 * The fixture's real signer. Pinning to it proves the whole offline chain verifies; pinning to
 * {@link PINNED_RELEASE_SIGNER} instead proves a valid signature from the wrong project is refused.
 * The fixture was signed through a reusable workflow, so its subject repository (`charmbracelet/meta`)
 * and its source repository (`charmbracelet/crush`) differ - which is exactly why the two are
 * pinned separately.
 */
const FIXTURE_SIGNER: PinnedSignerIdentity = {
	repositoryUri: "https://github.com/charmbracelet/crush",
	workflowRepositoryUri: "https://github.com/charmbracelet/meta",
	workflowPath: ".github/workflows/goreleaser.yml",
	oidcIssuer: RELEASE_SIGNER_OIDC_ISSUER,
	runnerEnvironment: "github-hosted",
	refPattern: /^refs\/heads\/main$/,
};

function fixturePolicy(overrides: Partial<PinnedSignerIdentity> = {}): PinnedSignerIdentity {
	return { ...FIXTURE_SIGNER, ...overrides };
}

const ARTIFACT = "crush_0.94.2_Linux_x86_64.tar.gz";

describe("verifyReleaseChecksums", () => {
	test("accepts a valid bundle and returns only signature-covered digests", () => {
		const verified = verifyReleaseChecksums(checksums, bundle, { identity: fixturePolicy() });

		expect(verified.signerIdentity).toBe(buildExpectedSignerIdentity(FIXTURE_SIGNER, "refs/heads/main"));
		expect(verified.signerRef).toBe("refs/heads/main");
		expect(verified.entries.get(ARTIFACT)).toMatch(/^[a-f0-9]{64}$/);
	});

	test("accepts the bundle shape our release lane emits (no timestamp authority)", () => {
		const verified = verifyReleaseChecksums(checksums, bundleWithoutTsa, { identity: fixturePolicy() });

		expect(verified.signerRef).toBe("refs/heads/main");
	});

	test("rejects a tampered checksum document", () => {
		const tampered = Buffer.from(checksums.toString("utf8").replace(/^[a-f0-9]{64}/m, "0".repeat(64)), "utf8");

		expect(() => verifyReleaseChecksums(tampered, bundle, { identity: fixturePolicy() })).toThrow(
			ReleaseSignatureError,
		);
	});

	test("rejects extra bytes appended to the signed document", () => {
		const appended = Buffer.concat([checksums, Buffer.from("\n")]);

		expect(() => verifyReleaseChecksums(appended, bundle, { identity: fixturePolicy() })).toThrow(
			/signature could not be verified/,
		);
	});

	test("rejects a missing bundle", () => {
		expect(() => verifyReleaseChecksums(checksums, undefined, { identity: fixturePolicy() })).toThrow(
			/No SHA256SUMS.sigstore.json signature was published/,
		);
	});

	test("rejects a structurally broken bundle", () => {
		expect(() => verifyReleaseChecksums(checksums, { mediaType: "nonsense" }, { identity: fixturePolicy() })).toThrow(
			ReleaseSignatureError,
		);
	});

	test("rejects a bundle whose transparency-log entry was stripped", () => {
		const stripped = structuredClone(bundle);
		stripped.verificationMaterial.tlogEntries = [];

		expect(() => verifyReleaseChecksums(checksums, stripped, { identity: fixturePolicy() })).toThrow(
			ReleaseSignatureError,
		);
	});

	test("rejects a cryptographically valid signature from the wrong signer", () => {
		expect(() => verifyReleaseChecksums(checksums, bundle)).toThrow(`not by ${PINNED_RELEASE_SIGNER.repositoryUri}`);
	});

	test("rejects the right workflow on a ref outside the allowed pattern", () => {
		expect(() =>
			verifyReleaseChecksums(checksums, bundle, {
				identity: fixturePolicy({ refPattern: /^refs\/tags\/v\d+\.\d+\.\d+$/ }),
			}),
		).toThrow(/not by /);
	});

	test("rejects the right repository signed by the wrong workflow file", () => {
		expect(() =>
			verifyReleaseChecksums(checksums, bundle, {
				identity: fixturePolicy({ workflowPath: ".github/workflows/build-binaries.yml" }),
			}),
		).toThrow(/not by /);
	});

	test("rejects a signature from an unexpected OIDC issuer", () => {
		expect(() =>
			verifyReleaseChecksums(checksums, bundle, {
				identity: fixturePolicy({ oidcIssuer: "https://accounts.google.com" }),
			}),
		).toThrow(/issued by /);
	});

	test("rejects a signature produced on a self-hosted runner", () => {
		expect(() =>
			verifyReleaseChecksums(checksums, bundle, {
				identity: fixturePolicy({ runnerEnvironment: "self-hosted" }),
			}),
		).toThrow(/runner/);
	});

	test("rejects a source repository that is not the pinned one", () => {
		expect(() =>
			verifyReleaseChecksums(checksums, bundle, {
				identity: fixturePolicy({ repositoryUri: "https://github.com/charmbracelet/meta" }),
			}),
		).toThrow(/source repository/);
	});
});

describe("parseChecksums", () => {
	test("rejects a malformed line rather than skipping it", () => {
		expect(() => parseChecksums(Buffer.from(`${"a".repeat(64)}  ok.tar.gz\ngarbage\n`, "utf8"))).toThrow(
			/Malformed line/,
		);
	});

	test("rejects duplicate file names", () => {
		const duplicated = `${"a".repeat(64)}  x.tar.gz\n${"b".repeat(64)}  x.tar.gz\n`;

		expect(() => parseChecksums(Buffer.from(duplicated, "utf8"))).toThrow(/Duplicate entry/);
	});

	test("rejects an empty document", () => {
		expect(() => parseChecksums(Buffer.from("\n\n", "utf8"))).toThrow(/is empty/);
	});
});

describe("fetchVerifiedReleaseArtifactDigest", () => {
	function stubFetch(responses: Record<string, Uint8Array | number>): {
		fetchImpl: typeof fetch;
		requested: string[];
	} {
		const requested: string[] = [];
		const fetchImpl = (async (input: string | URL) => {
			const url = String(input);
			requested.push(url);
			const body = responses[url];
			if (body === undefined) return new Response(null, { status: 404 });
			if (typeof body === "number") return new Response(null, { status: body });
			return new Response(Buffer.from(body), { status: 200 });
		}) as unknown as typeof fetch;
		return { fetchImpl, requested };
	}

	const OTHER_ORIGIN = "https://downloads.example.dev";
	const base = `${OTHER_ORIGIN}/releases/v0.94.2`;

	test("verifies against the pinned identity even on an overridden origin", async () => {
		const { fetchImpl, requested } = stubFetch({
			[`${base}/SHA256SUMS`]: checksums,
			[`${base}/SHA256SUMS.sigstore.json`]: Buffer.from(JSON.stringify(bundle), "utf8"),
		});

		const result = await fetchVerifiedReleaseArtifactDigest({
			baseUrl: OTHER_ORIGIN,
			version: "0.94.2",
			file: ARTIFACT,
			fetchImpl,
			identity: fixturePolicy(),
		});

		expect(result.digest).toMatch(/^[a-f0-9]{64}$/);
		expect(requested).toEqual([`${base}/SHA256SUMS`, `${base}/SHA256SUMS.sigstore.json`]);
	});

	test("an overridden origin cannot skip verification by omitting the bundle", async () => {
		const { fetchImpl } = stubFetch({ [`${base}/SHA256SUMS`]: checksums });

		await expect(
			fetchVerifiedReleaseArtifactDigest({
				baseUrl: OTHER_ORIGIN,
				version: "0.94.2",
				file: ARTIFACT,
				fetchImpl,
				identity: fixturePolicy(),
			}),
		).rejects.toThrow(/Could not download .*SHA256SUMS.sigstore.json \(HTTP 404\)/);
	});

	test("an overridden origin cannot substitute its own identity", async () => {
		const { fetchImpl } = stubFetch({
			[`${base}/SHA256SUMS`]: checksums,
			[`${base}/SHA256SUMS.sigstore.json`]: Buffer.from(JSON.stringify(bundle), "utf8"),
		});

		await expect(
			fetchVerifiedReleaseArtifactDigest({
				baseUrl: OTHER_ORIGIN,
				version: "0.94.2",
				file: ARTIFACT,
				fetchImpl,
			}),
		).rejects.toThrow(ReleaseSignatureError);
	});

	test("rejects a bundle that is not JSON", async () => {
		const { fetchImpl } = stubFetch({
			[`${base}/SHA256SUMS`]: checksums,
			[`${base}/SHA256SUMS.sigstore.json`]: Buffer.from("<html>nope</html>", "utf8"),
		});

		await expect(
			fetchVerifiedReleaseArtifactDigest({
				baseUrl: OTHER_ORIGIN,
				version: "0.94.2",
				file: ARTIFACT,
				fetchImpl,
				identity: fixturePolicy(),
			}),
		).rejects.toThrow(/not valid JSON/);
	});

	test("rejects an artifact the signed document does not cover", async () => {
		const { fetchImpl } = stubFetch({
			[`${base}/SHA256SUMS`]: checksums,
			[`${base}/SHA256SUMS.sigstore.json`]: Buffer.from(JSON.stringify(bundle), "utf8"),
		});

		await expect(
			fetchVerifiedReleaseArtifactDigest({
				baseUrl: OTHER_ORIGIN,
				version: "0.94.2",
				file: "prime-agent-0.94.2-darwin-arm64.tar.gz",
				fetchImpl,
				identity: fixturePolicy(),
			}),
		).rejects.toThrow(/does not cover/);
	});
});

describe("Bun/BoringSSL compatibility", () => {
	/**
	 * Bun links BoringSSL, which raises ERR_OSSL_NO_DEFAULT_DIGEST for a `crypto.verify` call that
	 * names no digest, where Node's OpenSSL defaults to SHA-256. `@sigstore/core` reports that throw
	 * as "signature invalid", so without naming the digest the compiled binary would refuse every
	 * update. Simulate BoringSSL on Node so the regression is caught by ordinary CI.
	 */
	test("verifies on a runtime that refuses a default digest, and restores crypto.verify", () => {
		const target = nodeCrypto as unknown as { verify: typeof nodeCrypto.verify };
		const real = target.verify;
		const boringSsl = ((algorithm: unknown, ...rest: unknown[]) => {
			if (algorithm === undefined || algorithm === null) {
				const error = new Error("no default digest") as NodeJS.ErrnoException;
				error.code = "ERR_OSSL_NO_DEFAULT_DIGEST";
				throw error;
			}
			return (real as unknown as (...args: unknown[]) => unknown)(algorithm, ...rest);
		}) as typeof nodeCrypto.verify;
		target.verify = boringSsl;
		try {
			const verified = verifyReleaseChecksums(checksums, bundle, { identity: fixturePolicy() });

			expect(verified.signerRef).toBe("refs/heads/main");
		} finally {
			target.verify = real;
		}
	});

	test("leaves @sigstore/core untouched once verification returns", () => {
		const before = sigstoreCrypto.verify;

		expect(() => verifyReleaseChecksums(checksums, { mediaType: "nonsense" })).toThrow(ReleaseSignatureError);
		verifyReleaseChecksums(checksums, bundle, { identity: fixturePolicy() });

		expect(sigstoreCrypto.verify).toBe(before);
	});
});
