import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { parse } from "yaml";
import { NATIVE_PLATFORMS } from "../src/utils/native-installation.js";

interface Step {
	name?: string;
	id?: string;
	run?: string;
	uses?: string;
	if?: string;
	"continue-on-error"?: boolean;
	with?: Record<string, string>;
}
interface Matrix {
	platform?: string[];
	channel?: string[];
	include: { platform: string; runner: string }[];
}
interface Job {
	needs?: string | string[];
	if?: string;
	"continue-on-error"?: boolean;
	"runs-on"?: string;
	strategy?: { matrix: Matrix };
	steps: Step[];
	with?: Record<string, string>;
}
interface Workflow {
	jobs: Record<string, Job>;
	on: Record<string, unknown>;
}

const repository = resolve(__dirname, "../../..");
// The same command the release workflow uses to enumerate publishable platforms.
const releasePlatforms = spawnSync(process.execPath, [join(repository, "scripts/release-platforms.mjs")], {
	encoding: "utf8",
})
	.stdout.trim()
	.split("\n");
const release: Workflow = parse(readFileSync(join(repository, ".github/workflows/build-binaries.yml"), "utf8"));
const standalone: Workflow = parse(readFileSync(join(repository, ".github/workflows/standalone-binaries.yml"), "utf8"));

function step(job: Job, name: string): Step {
	const found = job.steps.find((entry) => entry.name === name);
	expect(found, `Missing workflow step: ${name}`).toBeDefined();
	return found!;
}

function requiresSuccess(job: Job): void {
	expect(job["continue-on-error"]).toBeUndefined();
	// GitHub adds success() unless a status-check function overrides it.
	expect(job.if ?? "").not.toMatch(/(?:always|failure|cancelled|success)\s*\(/);
	for (const entry of job.steps ?? []) {
		expect(entry["continue-on-error"]).toBeUndefined();
		expect(entry.if ?? "").not.toMatch(/(?:always|failure|cancelled)\s*\(/);
	}
}

describe("release workflow signature gates", () => {
	it("requires successful build and both native final validation jobs before publication", () => {
		const validation = release.jobs["validate-macos"]!;
		expect(validation.needs).toEqual(expect.arrayContaining(["release-context", "build"]));
		expect(validation["runs-on"]).toBe(`\${{ matrix.runner }}`);
		const matrix = validation.strategy?.matrix;
		expect(matrix?.platform).toEqual(["darwin-arm64", "darwin-x64"]);
		expect(matrix?.channel).toEqual(["production", "beta"]);
		expect(matrix?.include).toEqual([
			{ platform: "darwin-arm64", runner: "macos-15" },
			{ platform: "darwin-x64", runner: "macos-15-intel" },
		]);
		expect(
			matrix!.platform!.flatMap((platform) => matrix!.channel!.map((channel) => ({ platform, channel }))),
		).toEqual([
			{ platform: "darwin-arm64", channel: "production" },
			{ platform: "darwin-arm64", channel: "beta" },
			{ platform: "darwin-x64", channel: "production" },
			{ platform: "darwin-x64", channel: "beta" },
		]);
		const publish = release.jobs.publish!;
		expect(publish.needs).toEqual(expect.arrayContaining(["build", "validate-macos"]));
		expect(publish.if).toBe("github.event_name != 'pull_request'");
		requiresSuccess(validation);
		requiresSuccess(publish);
	});

	it.each([
		{ channel: "production", publishProduction: true, publishBeta: false, skip: false },
		{ channel: "beta", publishProduction: true, publishBeta: false, skip: true },
		{ channel: "production", publishProduction: false, publishBeta: true, skip: true },
		{ channel: "beta", publishProduction: false, publishBeta: true, skip: false },
		{ channel: "production", publishProduction: true, publishBeta: true, skip: false },
		{ channel: "beta", publishProduction: true, publishBeta: true, skip: false },
	])("gates the $channel matrix lane when skip=$skip", ({ channel, publishProduction, publishBeta, skip }) => {
		const validation = release.jobs["validate-macos"]!;
		expect(validation.if).toBe(
			"needs.release-context.outputs.publish_production == 'true' || needs.release-context.outputs.publish_beta == 'true'",
		);
		const gate = step(validation, "Check channel is enabled");
		expect(gate.id).toBe("gate");
		expect(validation.steps[0]).toBe(gate);
		for (const entry of validation.steps.slice(1)) {
			expect(entry.if, entry.name).toBe("steps.gate.outputs.skip != 'true'");
		}

		const directory = mkdtempSync(join(tmpdir(), "prime-release-gate-"));
		try {
			const output = join(directory, "output");
			const result = spawnSync("bash", ["-e", "-o", "pipefail", "-c", gate.run!], {
				env: {
					...process.env,
					CHANNEL: channel,
					PUBLISH_PRODUCTION: String(publishProduction),
					PUBLISH_BETA: String(publishBeta),
					GITHUB_OUTPUT: output,
				},
				encoding: "utf8",
			});
			expect(result.status, result.stderr).toBe(0);
			expect(readFileSync(output, "utf8").trim()).toBe(`skip=${skip}`);
		} finally {
			rmSync(directory, { recursive: true, force: true });
		}
	});

	it("tests final channel archives before uploading receipts, then checks receipts before external writes", () => {
		const validation = release.jobs["validate-macos"]!;
		const verify = step(validation, "Verify and exercise exact final Mac archive");
		expect(verify.run).not.toContain("for channel in production beta");
		expect(verify.run).toContain("validate-macos-release.mjs");
		expect(verify.run).toContain("standalone-reference/binaries.json");
		expect(verify.run).toContain("test/compiled-artifact.test.ts");
		expect(verify.run).not.toMatch(/\|\|\s*(?:true|:)|continue-on-error/);
		expect(validation.steps.indexOf(verify)).toBeLessThan(
			validation.steps.indexOf(step(validation, "Upload native validation receipt")),
		);
		const publish = release.jobs.publish!;
		const gate = step(publish, "Match native validation to publication artifacts");
		expect(gate.if).toBeUndefined();
		expect(gate.run).toContain(
			"verify-macos-validation-receipts.mjs release-artifacts/production macos-validation production",
		);
		expect(gate.run).toContain("verify-macos-validation-receipts.mjs release-artifacts/beta macos-validation beta");
		const writes = publish.steps.filter((entry) =>
			/aws s3 cp|gh release (?:upload|create|edit)|gh api --method/.test(entry.run ?? ""),
		);
		expect(writes.length).toBeGreaterThan(0);
		for (const write of writes) expect(publish.steps.indexOf(gate)).toBeLessThan(publish.steps.indexOf(write));
	});

	it.each([{ channel: "production" }, { channel: "beta" }])(
		"finds the downloaded manifest when validating $channel",
		({ channel }) => {
			const validation = release.jobs["validate-macos"]!;
			const directory = mkdtempSync(join(tmpdir(), "prime-release-downloads-"));
			try {
				mkdirSync(join(directory, "packages/coding-agent"), { recursive: true });
				const download = validation.steps.find(
					(entry) =>
						entry.uses?.startsWith("actions/download-artifact@") &&
						entry.name === "Download exact final channel artifacts",
				);
				expect(download, `Missing download step`).toBeDefined();
				const destination = download!
					.with!.path!.replace(`\${{ runner.temp }}`, directory)
					.replace(`\${{ matrix.channel }}`, channel);
				mkdirSync(destination, { recursive: true });
				writeFileSync(join(destination, channel === "production" ? "latest.json" : "beta.json"), "{}");
				writeFileSync(join(destination, "prime-agent-1.2.3-darwin-arm64.tar.gz"), "");
				const result = spawnSync(
					"bash",
					[
						"-e",
						"-o",
						"pipefail",
						"-c",
						`node() { test -f "$2/latest.json" || test -f "$2/beta.json"; }
npx() { test -f "$PRIME_AGENT_TEST_ARCHIVE"; printf '%s\\n' "$PRIME_AGENT_TEST_ARCHIVE"; }
${step(validation, "Verify and exercise exact final Mac archive").run}`,
					],
					{
						cwd: directory,
						env: {
							...process.env,
							RUNNER_TEMP: directory,
							TARGET_PLATFORM: "darwin-arm64",
							CHANNEL: channel,
						},
						encoding: "utf8",
					},
				);
				expect(result.status, result.stderr).toBe(0);
				expect(result.stdout.trim()).toBe(
					join(directory, `final-artifacts/prime-agent-${channel}/prime-agent-1.2.3-darwin-arm64.tar.gz`),
				);
			} finally {
				rmSync(directory, { recursive: true, force: true });
			}
		},
	);

	it("retains every standalone target and only uploads tested executable identities", () => {
		const build = standalone.jobs.build!;
		expect(build.strategy?.matrix.include.map((entry) => entry.platform)).toEqual(releasePlatforms);
		// Each target must be compiled explicitly; the host default cannot produce a cross-build.
		expect(step(build, "Compile standalone application").run).toContain(`--platform \${{ matrix.platform }}`);
		const test = step(build, "Test extracted application without JavaScript runtimes on PATH");
		expect(test.run).toContain("test/compiled-artifact.test.ts");
		expect(test.run).toContain("test/release-signatures.test.ts");
		const upload = build.steps.find((entry) => entry.uses?.startsWith("actions/upload-artifact@"))!;
		expect(upload.with?.path).toContain("binaries.json");
		expect(build.steps.indexOf(test)).toBeLessThan(build.steps.indexOf(upload));
		requiresSuccess(build);
		expect(release.jobs.standalone!.with?.build_ref).toBe(`\${{ needs.release-context.outputs.build_ref }}`);
	});

	it("executes every archive on its own libc, musl archives inside Alpine", () => {
		const build = standalone.jobs.build!;
		const glibc = step(build, "Test extracted application without JavaScript runtimes on PATH");
		const musl = step(build, "Test extracted application on Alpine without JavaScript runtimes");
		// A cross-compiled musl archive cannot run on the glibc runner that built it.
		expect(glibc.if).toBe(`\${{ !contains(matrix.platform, 'musl') }}`);
		expect(musl.if).toBe(`\${{ contains(matrix.platform, 'musl') }}`);
		expect(musl.run).toContain("docker run");
		expect(musl.run).toContain("alpine:");
		expect(musl.run).toContain("prime-agent --version");
		expect(musl.run).toContain("prime-agent --help");
		const upload = build.steps.find((entry) => entry.uses?.startsWith("actions/upload-artifact@"))!;
		expect(build.steps.indexOf(musl)).toBeLessThan(build.steps.indexOf(upload));
		// A container smoke test only proves execution when the runner matches the target architecture.
		for (const entry of build.strategy!.matrix.include) {
			if (!entry.platform.startsWith("linux-")) continue;
			expect(entry.runner.endsWith("-arm"), entry.platform).toBe(entry.platform.includes("arm64"));
		}
	});

	it("keeps one platform set across the installer, the release scripts, and the workflows", () => {
		expect([...NATIVE_PLATFORMS]).toEqual(releasePlatforms);
		const stage = step(release.jobs.build!, "Verify and stage standalone binaries");
		// A hardcoded list here silently drops newly published platforms from a release.
		expect(stage.run).toContain("node scripts/release-platforms.mjs");
		for (const platform of releasePlatforms) expect(stage.run).not.toContain(` ${platform} `);
	});

	it.skipIf(process.platform === "win32")(
		"selects both real packer paths for PR validation without allowing publication",
		() => {
			expect(release.on).toHaveProperty("pull_request");
			const directory = mkdtempSync(join(tmpdir(), "prime-release-context-"));
			try {
				const output = join(directory, "output");
				const context = step(release.jobs["release-context"]!, "Resolve release context");
				const result = spawnSync("bash", ["-e", "-o", "pipefail", "-c", context.run!], {
					cwd: repository,
					env: {
						...process.env,
						EVENT_NAME: "pull_request",
						GITHUB_SHA_VALUE: "abcdef0123456789",
						RUN_NUMBER: "5",
						RUN_ATTEMPT: "1",
						GITHUB_OUTPUT: output,
					},
					encoding: "utf8",
				});
				expect(result.status, result.stderr).toBe(0);
				const values = Object.fromEntries(
					readFileSync(output, "utf8")
						.trim()
						.split("\n")
						.map((line) => line.split("=")),
				);
				expect(values).toMatchObject({
					publish_beta: "true",
					publish_production: "true",
					build_ref: "abcdef0123456789",
				});
				expect(values.beta_version).toBe(`${values.production_version}-beta.5.1.abcdef0`);
				for (const [name, channel] of [
					["Pack production release", "stable"],
					["Pack beta release", "beta"],
				]) {
					const pack = step(release.jobs.build!, name!);
					expect(pack.run).toContain("npm run release:pack");
					expect(pack.run).toContain(`--channel ${channel}`);
					expect(pack.run).toContain("--binary-dir packages/coding-agent/binaries");
				}
				expect(release.jobs.publish!.if).toBe("github.event_name != 'pull_request'");
			} finally {
				rmSync(directory, { recursive: true, force: true });
			}
		},
	);
});

describe("release manifest schemas", () => {
	const manifestV1Platforms = ["darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64"];
	const binaries = releasePlatforms.map((platform, index) => ({
		platform,
		file: `prime-agent-1.2.4-${platform}.tar.gz`,
		sha256: (index + 1).toString(16).padStart(64, "0"),
		executableSha256: (index + 11).toString(16).padStart(64, "0"),
	}));
	const tarballs = [
		["prime-agent-ai", "1"],
		["prime-agent-core", "2"],
		["prime-agent-tui", "3"],
		["prime-agent", "4"],
	].map(([name, hash]) => ({
		name,
		file: `${name}-1.2.4.tgz`,
		sha256: hash.repeat(64),
	}));

	it.each([
		["stable", "latest.json"],
		["beta", "beta.json"],
	] as const)("writes the %s channel with v1 and v2 binary schemas", (channel, manifestName) => {
		const directory = mkdtempSync(join(tmpdir(), "prime-release-manifest-"));
		try {
			const result = spawnSync(
				process.execPath,
				[
					"--input-type=module",
					"-e",
					`import { writeReleaseMetadata } from ${JSON.stringify(join(repository, "scripts/pack-prime-agent-release.mjs"))}; writeReleaseMetadata(${JSON.stringify(
						{
							artifactsDir: directory,
							channel,
							releaseVersion: "1.2.4",
							codingAgentTarball: "prime-agent-1.2.4.tgz",
							tarballs,
							binaries,
						},
					)});`,
				],
				{ encoding: "utf8" },
			);
			expect(result.status, result.stderr).toBe(0);

			const manifest = JSON.parse(readFileSync(join(directory, manifestName), "utf8"));
			expect(manifest.version).toBe("v1.2.4");
			expect(manifest.tarballs).toEqual(
				tarballs.map(({ name: packageName, file, sha256 }) => ({ package: packageName, file, sha256 })),
			);
			expect(manifest.binaries.map((entry: { platform: string }) => entry.platform)).toEqual(manifestV1Platforms);
			expect(manifest.binariesV2.map((entry: { platform: string }) => entry.platform)).toEqual(releasePlatforms);
			expect(manifest.binariesV2.map((entry: { platform: string }) => entry.platform)).toEqual(
				expect.arrayContaining([
					"linux-arm64-musl",
					"linux-x64-baseline",
					"linux-x64-musl",
					"linux-x64-musl-baseline",
				]),
			);
			expect(readFileSync(join(directory, channel), "utf8")).toBe("v1.2.4\n");

			const expectedChecksums = [...tarballs, ...binaries]
				.map((artifact) => `${artifact.sha256}  ${artifact.file}`)
				.join("\n");
			expect(readFileSync(join(directory, "SHA256SUMS"), "utf8")).toBe(`${expectedChecksums}\n`);
		} finally {
			rmSync(directory, { recursive: true, force: true });
		}
	});
});
