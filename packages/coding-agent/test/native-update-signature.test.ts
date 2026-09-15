import {
	mkdirSync,
	mkdtempSync,
	readFileSync,
	readlinkSync,
	realpathSync,
	rmSync,
	symlinkSync,
	writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { getNativeUpdatePlan } from "../src/cli/native-update.js";
import { NATIVE_RELEASE_ASSETS } from "../src/utils/native-installation.js";
import { parseDownloadBaseUrl, ReleaseSignatureError, releaseAssetUrl } from "../src/utils/release-signature.js";

/**
 * End-to-end fail-closed behaviour with NOTHING stubbed except the network.
 *
 * The updater runs its real, production-pinned verifier here. The fixtures are a genuine Sigstore
 * bundle from another project, so the bytes and the signature agree perfectly and only the identity
 * is wrong - which is exactly the shape of an attacker who owns the download origin and can also
 * produce their own valid Sigstore signature.
 */

const fixtures = join(dirname(fileURLToPath(import.meta.url)), "fixtures", "release-signature");
const foreignChecksums = readFileSync(join(fixtures, "SHA256SUMS"), "utf8");
const foreignBundle = readFileSync(join(fixtures, "SHA256SUMS.sigstore.json"), "utf8");

const PLATFORM = "linux-x64";
const VERSION = "1.2.4";
const FILE = `prime-agent-${VERSION}-${PLATFORM}.tar.gz`;
const DIGEST = "b".repeat(64);
const baseUrl = "https://releases.example";

describe("self-update signature enforcement", () => {
	let root: string;
	let executable: string;
	let target: string;

	beforeEach(() => {
		vi.stubEnv("PI_SKIP_VERSION_CHECK", "");
		vi.stubEnv("PI_OFFLINE", "");
		vi.stubEnv("PRIME_AGENT_DOWNLOAD_BASE_URL", baseUrl);
		root = realpathSync(mkdtempSync(join(tmpdir(), "prime-native-signature-")));
		const checksum = "a".repeat(64);
		const releaseName = `1.2.3-${PLATFORM}-${checksum}`;
		const releaseDir = join(root, "releases", releaseName);
		mkdirSync(releaseDir, { recursive: true });
		mkdirSync(join(root, "bin"));
		writeFileSync(join(root, ".managed"), "prime-agent-native-v1\n");
		for (const asset of NATIVE_RELEASE_ASSETS) {
			mkdirSync(dirname(join(releaseDir, asset)), { recursive: true });
			writeFileSync(join(releaseDir, asset), "fixture\n");
		}
		writeFileSync(join(releaseDir, ".archive-sha256"), checksum);
		writeFileSync(join(releaseDir, ".install-source"), baseUrl);
		writeFileSync(join(releaseDir, "package.json"), JSON.stringify({ version: "1.2.3" }));
		executable = join(releaseDir, "prime-agent");
		writeFileSync(executable, "fixture executable");
		target = `../releases/${releaseName}/prime-agent`;
		symlinkSync(target, join(root, "bin", "prime-agent"));
	});

	afterEach(() => {
		vi.unstubAllGlobals();
		vi.unstubAllEnvs();
		rmSync(root, { recursive: true, force: true });
	});

	function serve(assets: Record<string, string>): void {
		vi.stubGlobal(
			"fetch",
			vi.fn(async (input: string | URL) => {
				const url = String(input);
				if (url.endsWith("/latest.json"))
					return Response.json({
						version: VERSION,
						binaries: [{ platform: PLATFORM, file: FILE, sha256: DIGEST }],
					});
				const body = assets[url];
				return body === undefined ? new Response(null, { status: 404 }) : new Response(body, { status: 200 });
			}),
		);
	}

	const signatureUrl = `${baseUrl}/releases/v${VERSION}/SHA256SUMS.sigstore.json`;
	const checksumsUrl = `${baseUrl}/releases/v${VERSION}/SHA256SUMS`;
	const signedForThisRelease = `${DIGEST}  ${FILE}\n`;

	it("refuses an update when no signature bundle is published", async () => {
		serve({ [checksumsUrl]: signedForThisRelease });

		await expect(getNativeUpdatePlan({ force: true, rollback: false, executable })).rejects.toThrow(
			ReleaseSignatureError,
		);
		expect(readlinkSync(join(root, "bin", "prime-agent"))).toBe(target);
	});

	it("refuses an update when the signature comes from another project's release workflow", async () => {
		serve({ [checksumsUrl]: foreignChecksums, [signatureUrl]: foreignBundle });

		await expect(getNativeUpdatePlan({ force: true, rollback: false, executable })).rejects.toThrow(
			/not by https:\/\/github\.com\/PrimeIntellect-ai\/prime-agent/,
		);
		expect(readlinkSync(join(root, "bin", "prime-agent"))).toBe(target);
	});

	it("refuses an update when a valid signature covers different bytes", async () => {
		// The signature is real and its identity check would run, but it does not cover this
		// document, so verification fails before the identity is even considered.
		serve({ [checksumsUrl]: signedForThisRelease, [signatureUrl]: foreignBundle });

		await expect(getNativeUpdatePlan({ force: true, rollback: false, executable })).rejects.toThrow(
			/signature could not be verified/,
		);
		expect(readlinkSync(join(root, "bin", "prime-agent"))).toBe(target);
	});

	it("refuses an update when the bundle is not a Sigstore bundle at all", async () => {
		serve({ [checksumsUrl]: signedForThisRelease, [signatureUrl]: JSON.stringify({ hello: "world" }) });

		await expect(getNativeUpdatePlan({ force: true, rollback: false, executable })).rejects.toThrow(
			ReleaseSignatureError,
		);
		expect(readlinkSync(join(root, "bin", "prime-agent"))).toBe(target);
	});

	it("fetches the manifest, checksums and bundle from URL-API-built paths under the origin", async () => {
		// An origin with a path and stray trailing slashes must still produce exactly one `/` per join.
		vi.stubEnv("PRIME_AGENT_DOWNLOAD_BASE_URL", "https://mirror.example/prime//");
		serve({
			[`https://mirror.example/prime/releases/v${VERSION}/SHA256SUMS`]: signedForThisRelease,
			[`https://mirror.example/prime/releases/v${VERSION}/SHA256SUMS.sigstore.json`]: foreignBundle,
		});

		await expect(getNativeUpdatePlan({ force: true, rollback: false, executable })).rejects.toThrow(
			/signature could not be verified/,
		);
		const requested = (fetch as unknown as ReturnType<typeof vi.fn>).mock.calls.map((call) => String(call[0]));
		expect(requested).toEqual([
			"https://mirror.example/prime/latest.json",
			`https://mirror.example/prime/releases/v${VERSION}/SHA256SUMS`,
			`https://mirror.example/prime/releases/v${VERSION}/SHA256SUMS.sigstore.json`,
		]);
	});

	it("an overridden origin gets no relaxation: the same signature is still demanded", async () => {
		vi.stubEnv("PRIME_AGENT_DOWNLOAD_BASE_URL", "https://mirror.example");
		serve({
			[`https://mirror.example/releases/v${VERSION}/SHA256SUMS`]: foreignChecksums,
			[`https://mirror.example/releases/v${VERSION}/SHA256SUMS.sigstore.json`]: foreignBundle,
		});

		await expect(getNativeUpdatePlan({ force: true, rollback: false, executable })).rejects.toThrow(
			/not by https:\/\/github\.com\/PrimeIntellect-ai\/prime-agent/,
		);
		expect(readlinkSync(join(root, "bin", "prime-agent"))).toBe(target);
	});
});

describe("release URL construction", () => {
	it("canonicalises a download origin", () => {
		expect(parseDownloadBaseUrl("https://releases.example")).toBe("https://releases.example");
		expect(parseDownloadBaseUrl("https://releases.example/")).toBe("https://releases.example");
		expect(parseDownloadBaseUrl(" https://Releases.Example:8443/prime// ")).toBe(
			"https://releases.example:8443/prime",
		);
		expect(parseDownloadBaseUrl("https://releases.example:443/prime")).toBe("https://releases.example/prime");
	});

	it.each([
		["http scheme", "http://releases.example", /must use https/],
		["invalid URL", "releases.example", /not a valid URL/],
		["query string", "https://releases.example/?x=1", /query string/],
		["empty query string", "https://releases.example/?", /query string/],
		["fragment", "https://releases.example/#x", /fragment/],
		["empty fragment", "https://releases.example/#", /fragment/],
		["credentials", "https://user:pass@releases.example", /credentials/],
		["username", "https://user@releases.example", /credentials/],
	])("refuses a download origin with a %s", (_label, raw, message) => {
		expect(() => parseDownloadBaseUrl(raw, "The origin")).toThrow(message);
		expect(() => parseDownloadBaseUrl(raw, "The origin")).toThrow(/^The origin/);
		expect(() => releaseAssetUrl(raw, VERSION, "SHA256SUMS")).toThrow(ReleaseSignatureError);
	});

	it("joins release assets with the URL API, never by concatenation", () => {
		expect(releaseAssetUrl("https://releases.example", VERSION, "SHA256SUMS")).toBe(
			`https://releases.example/releases/v${VERSION}/SHA256SUMS`,
		);
		expect(releaseAssetUrl("https://releases.example/prime///", "1.2.4-beta.1", "SHA256SUMS.sigstore.json")).toBe(
			"https://releases.example/prime/releases/v1.2.4-beta.1/SHA256SUMS.sigstore.json",
		);
	});

	it.each([
		["a path segment in the version", "1.2.4/../../evil", "SHA256SUMS"],
		["a query in the version", "1.2.4?x=1", "SHA256SUMS"],
		["a tag prefix in the version", "v1.2.4", "SHA256SUMS"],
		["a path segment in the asset", VERSION, "../SHA256SUMS"],
		["a query in the asset", VERSION, "SHA256SUMS?x=1"],
		["an empty asset", VERSION, ""],
	])("refuses to build a release URL with %s", (_label, version, asset) => {
		expect(() => releaseAssetUrl("https://releases.example", version, asset)).toThrow(ReleaseSignatureError);
	});
});
