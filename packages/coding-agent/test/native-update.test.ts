import { mkdirSync, mkdtempSync, readlinkSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { getNativeUpdatePlan } from "../src/cli/native-update.js";
import { NATIVE_RELEASE_ASSETS } from "../src/utils/native-installation.js";
import { ReleaseSignatureError } from "../src/utils/release-signature.js";
import { getLatestPiRelease } from "../src/utils/version-check.js";

const SIGNER_IDENTITY =
	"https://github.com/PrimeIntellect-ai/prime-agent/.github/workflows/build-binaries.yml@refs/heads/main";

// The real verifier is exercised end to end in release-signature.test.ts and, against the production
// pinning, in native-update-signature.test.ts. Here it is stubbed so these tests can concentrate on
// how the update plan reacts to its result.
const verifiedDigest = vi.hoisted(() => vi.fn());
vi.mock("../src/utils/release-signature.js", async (importOriginal) => ({
	...(await importOriginal<typeof import("../src/utils/release-signature.js")>()),
	fetchVerifiedReleaseArtifactDigest: verifiedDigest,
}));

const artifact = {
	platform: "linux-x64",
	file: "prime-agent-1.2.4-linux-x64.tar.gz",
	sha256: "b".repeat(64),
};
const invalidMetadata: Array<{ name: string; binaries: unknown }> = [
	{ name: "invalid checksum", binaries: [{ ...artifact, sha256: "invalid" }] },
	{ name: "duplicate platform", binaries: [artifact, artifact] },
	{ name: "valid entry followed by invalid entry", binaries: [artifact, null] },
	{ name: "wrong archive version", binaries: [{ ...artifact, file: "prime-agent-1.2.3-linux-x64.tar.gz" }] },
	{ name: "non-array metadata", binaries: { artifact } },
];

describe("native release metadata isolation", () => {
	let root: string;
	let executable: string;
	let target: string;
	const baseUrl = "https://releases.example";

	beforeEach(() => {
		vi.stubEnv("PI_SKIP_VERSION_CHECK", "");
		vi.stubEnv("PI_OFFLINE", "");
		vi.stubEnv("PRIME_AGENT_DOWNLOAD_BASE_URL", baseUrl);
		root = realpathSync(mkdtempSync(join(tmpdir(), "prime-native-metadata-")));
		const checksum = "a".repeat(64);
		const releaseName = `1.2.3-linux-x64-${checksum}`;
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
		verifiedDigest.mockReset();
		rmSync(root, { recursive: true, force: true });
	});

	it.each(invalidMetadata)(
		"preserves npm release details but refuses native updates for $name",
		async ({ binaries }) => {
			vi.stubGlobal(
				"fetch",
				vi.fn(async () =>
					Response.json({
						version: "v1.2.4",
						package: "prime-agent",
						tarball: "releases/v1.2.4/prime-agent-1.2.4.tgz",
						binaries,
					}),
				),
			);

			await expect(getLatestPiRelease("1.2.3")).resolves.toEqual({
				version: "1.2.4",
				packageName: "prime-agent",
				installSpec: `${baseUrl}/releases/v1.2.4/prime-agent-1.2.4.tgz`,
			});
			for (const force of [false, true]) {
				await expect(getNativeUpdatePlan({ force, rollback: false, executable })).rejects.toThrow(
					"No verified compiled archive is available for linux-x64.",
				);
			}
			expect(readlinkSync(join(root, "bin", "prime-agent"))).toBe(target);
		},
	);

	it("uses the signature-verified platform checksum when the entire native list is valid", async () => {
		vi.stubGlobal(
			"fetch",
			vi.fn(async () => Response.json({ version: "1.2.4", binaries: [artifact] })),
		);
		verifiedDigest.mockResolvedValue({
			digest: artifact.sha256,
			signerIdentity: SIGNER_IDENTITY,
			signerRef: "refs/heads/main",
		});

		const plan = await getNativeUpdatePlan({ force: false, rollback: false, executable });

		expect(verifiedDigest).toHaveBeenCalledWith(
			expect.objectContaining({ baseUrl, version: "1.2.4", file: artifact.file }),
		);
		expect(plan.targetVersion).toBe("1.2.4");
		expect(plan.verifiedSignerIdentity).toBe(SIGNER_IDENTITY);
		expect(plan.command?.args).toContain(`PRIME_AGENT_EXPECTED_SHA256=${artifact.sha256}`);
		expect(plan.command?.args).toContain("PRIME_AGENT_INSTALL_METHOD=binary");
	});

	it("refuses the update when the signed SHA256SUMS disagrees with the release manifest", async () => {
		vi.stubGlobal(
			"fetch",
			vi.fn(async () => Response.json({ version: "1.2.4", binaries: [artifact] })),
		);
		verifiedDigest.mockResolvedValue({
			digest: "c".repeat(64),
			signerIdentity: SIGNER_IDENTITY,
			signerRef: "refs/heads/main",
		});

		await expect(getNativeUpdatePlan({ force: false, rollback: false, executable })).rejects.toThrow(
			/does not match the release manifest/,
		);
		expect(readlinkSync(join(root, "bin", "prime-agent"))).toBe(target);
	});

	it("lets a verification failure abort the update instead of degrading to a warning", async () => {
		vi.stubGlobal(
			"fetch",
			vi.fn(async () => Response.json({ version: "1.2.4", binaries: [artifact] })),
		);
		verifiedDigest.mockRejectedValue(new ReleaseSignatureError("no signature was published"));

		await expect(getNativeUpdatePlan({ force: false, rollback: false, executable })).rejects.toThrow(
			ReleaseSignatureError,
		);
		expect(readlinkSync(join(root, "bin", "prime-agent"))).toBe(target);
	});

	it("records the origin override instead of applying it silently", async () => {
		vi.stubEnv("PRIME_AGENT_DOWNLOAD_BASE_URL", "https://mirror.example/");
		vi.stubGlobal(
			"fetch",
			vi.fn(async () => Response.json({ version: "1.2.4", binaries: [artifact] })),
		);
		verifiedDigest.mockResolvedValue({
			digest: artifact.sha256,
			signerIdentity: SIGNER_IDENTITY,
			signerRef: "refs/heads/main",
		});

		const plan = await getNativeUpdatePlan({ force: false, rollback: false, executable });

		// The override moves the origin, and verification still runs against that origin.
		expect(plan.overriddenBaseUrl).toBe("https://mirror.example");
		expect(verifiedDigest).toHaveBeenCalledWith(expect.objectContaining({ baseUrl: "https://mirror.example" }));
		expect(plan.command?.args).toContain("PRIME_AGENT_DOWNLOAD_BASE_URL=https://mirror.example");
	});

	it.each(["http://mirror.example", "not a url"])(
		"refuses an origin override that is not an https URL: %s",
		async (override) => {
			vi.stubEnv("PRIME_AGENT_DOWNLOAD_BASE_URL", override);

			await expect(getNativeUpdatePlan({ force: false, rollback: false, executable })).rejects.toThrow(
				/PRIME_AGENT_DOWNLOAD_BASE_URL/,
			);
			expect(verifiedDigest).not.toHaveBeenCalled();
		},
	);
});
