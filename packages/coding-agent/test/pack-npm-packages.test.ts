import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

const repository = resolve(__dirname, "../../..");
const packerPath = join(repository, "scripts/pack-npm-packages.mjs");
const packerUrl = pathToFileURL(packerPath).href;
const dependenciesUrl = pathToFileURL(join(repository, "scripts/lib/internal-dependencies.mjs")).href;
const platforms = ["darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64"];
const version = "1.2.3";

let packer: any;
let dependencies: any;
let root: string;
let binaryDir: string;
let packagesDir: string;

function sha256(path: string): string {
	return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function readJson(path: string): any {
	return JSON.parse(readFileSync(path, "utf8"));
}

/** Minimal stand-in for a compiled standalone build: executable plus every required sibling asset. */
function writeBinaryFixture(directory: string, platform: string): void {
	mkdirSync(directory, { recursive: true });
	writeFileSync(join(directory, "prime-agent"), `#!/bin/sh\necho "fixture ${platform} $*"\nexit 7\n`, { mode: 0o755 });
	for (const name of [
		"install.sh",
		"README.md",
		"CHANGELOG.md",
		"LICENSE",
		"photon_rs_bg.wasm",
		"prime-agent-runtime/pyproject.toml",
		"prime-agent-runtime/src/rlm/repl.py",
		"theme/prime.json",
		"theme/dark.json",
		"theme/light.json",
		"export-html/template.html",
		"export-html/template.css",
		"export-html/template.js",
		"export-html/vendor/marked.min.js",
		"export-html/vendor/highlight.min.js",
	]) {
		mkdirSync(dirname(join(directory, name)), { recursive: true });
		writeFileSync(join(directory, name), name);
	}
	for (const name of ["skills", "assets", "docs", "examples"]) mkdirSync(join(directory, name), { recursive: true });
	writeFileSync(join(directory, "package.json"), `${JSON.stringify({ name: "fixture", version: "0.0.0" })}\n`);
}

/** Workspace stand-in: the real manifests plus a built dist, so no repository build is required. */
function writePackagesFixture(directory: string): void {
	for (const name of ["ai", "agent", "tui", "coding-agent"]) {
		const target = join(directory, name);
		mkdirSync(join(target, "dist"), { recursive: true });
		cpSync(join(repository, "packages", name, "package.json"), join(target, "package.json"));
		writeFileSync(join(target, "dist", "index.js"), "export const fixture = true;\n");
		writeFileSync(join(target, "README.md"), `# ${name}\n`);
	}
}

function pack(args: string[]) {
	return spawnSync(process.execPath, [packerPath, ...args], { encoding: "utf8" });
}

function stage(extra: string[] = []) {
	const outDir = mkdtempSync(join(root, "out-"));
	rmSync(outDir, { recursive: true, force: true });
	const result = pack([
		"--binary-dir",
		binaryDir,
		"--packages-dir",
		packagesDir,
		"--version",
		version,
		"--out-dir",
		outDir,
		"--skip-pack",
		...extra,
	]);
	return { outDir, result };
}

function plan() {
	const sourcePackages = new Map(
		["ai", "agent", "tui", "coding-agent"].map((name) => [name, readJson(join(packagesDir, name, "package.json"))]),
	);
	const binaries = platforms.map((platform) => ({
		platform,
		executableSha256: sha256(join(binaryDir, platform, "prime-agent")),
		file: `prime-agent-${version}-${platform}.tar.gz`,
		sha256: "b".repeat(64),
	}));
	return packer.buildPackagePlan({
		version,
		scope: "@primeintellect",
		frontDoor: "prime-agent",
		sourcePackages,
		binaries,
	});
}

beforeAll(async () => {
	packer = await import(/* @vite-ignore */ packerUrl);
	dependencies = await import(/* @vite-ignore */ dependenciesUrl);
	root = mkdtempSync(join(tmpdir(), "prime-npm-packages-"));
	binaryDir = join(root, "binaries");
	packagesDir = join(root, "packages");
	for (const platform of platforms) writeBinaryFixture(join(binaryDir, platform), platform);
	writePackagesFixture(packagesDir);
});

afterAll(() => {
	if (root) rmSync(root, { recursive: true, force: true });
});

describe("npm package plan", () => {
	it("publishes the documented names in dependency order", () => {
		expect(plan().map((entry: any) => entry.name)).toEqual([
			"@primeintellect/prime-agent-darwin-arm64",
			"@primeintellect/prime-agent-darwin-x64",
			"@primeintellect/prime-agent-linux-arm64",
			"@primeintellect/prime-agent-linux-x64",
			"@primeintellect/prime-agent-ai",
			"@primeintellect/prime-agent-core",
			"@primeintellect/prime-agent-tui",
			"prime-agent",
			"@primeintellect/prime-agent",
		]);
	});

	it("gives every package the release version and public provenance publish config", () => {
		for (const entry of plan()) {
			expect(entry.packageJson.version).toBe(version);
			expect(entry.packageJson.publishConfig).toEqual({
				access: "public",
				registry: "https://registry.npmjs.org",
				provenance: true,
			});
			expect(entry.packageJson.scripts).toBeUndefined();
			expect(entry.packageJson.repository.url).toContain("PrimeIntellect-ai/prime-agent");
		}
	});

	it("gates each platform package on os and cpu and carries the executable receipt", () => {
		const expected: Record<string, { os: string; cpu: string }> = {
			"darwin-arm64": { os: "darwin", cpu: "arm64" },
			"darwin-x64": { os: "darwin", cpu: "x64" },
			"linux-arm64": { os: "linux", cpu: "arm64" },
			"linux-x64": { os: "linux", cpu: "x64" },
		};
		for (const entry of plan().filter((candidate: any) => candidate.kind === "platform")) {
			const { os, cpu } = expected[entry.platform];
			expect(entry.packageJson.os).toEqual([os]);
			expect(entry.packageJson.cpu).toEqual([cpu]);
			expect(entry.packageJson.preferUnplugged).toBe(true);
			expect(entry.packageJson.primeAgent.executableSha256).toBe(
				sha256(join(binaryDir, entry.platform, "prime-agent")),
			);
			expect(entry.packageJson.primeAgent.archive.file).toBe(`prime-agent-${version}-${entry.platform}.tar.gz`);
			expect(entry.packageJson.dependencies).toBeUndefined();
		}
	});

	it("pins the front door to the exact platform package versions and ships only the shim", () => {
		for (const entry of plan().filter((candidate: any) => candidate.kind === "front-door")) {
			expect(entry.packageJson.optionalDependencies).toEqual({
				"@primeintellect/prime-agent-darwin-arm64": version,
				"@primeintellect/prime-agent-darwin-x64": version,
				"@primeintellect/prime-agent-linux-arm64": version,
				"@primeintellect/prime-agent-linux-x64": version,
			});
			expect(entry.packageJson.bin).toEqual({ "prime-agent": "bin/prime-agent.cjs" });
			expect(entry.packageJson.dependencies).toBeUndefined();
			expect(Object.keys(entry.packageJson.primeAgent.platforms).sort()).toEqual(platforms);
		}
	});

	it("keeps the scoped mirror identical to the canonical front door apart from its name", () => {
		const entries = plan().filter((candidate: any) => candidate.kind === "front-door");
		expect({ ...entries[0].packageJson, name: undefined }).toEqual({ ...entries[1].packageJson, name: undefined });
		expect(entries.map((entry: any) => entry.packageJson.name)).toEqual([
			"prime-agent",
			"@primeintellect/prime-agent",
		]);
	});

	it("rewrites internal library dependencies to semver ranges, never tarball URLs", () => {
		const core = plan().find((entry: any) => entry.name === "@primeintellect/prime-agent-core");
		expect(core.packageJson.dependencies["@earendil-works/pi-ai"]).toBe(
			`npm:@primeintellect/prime-agent-ai@^${version}`,
		);
		expect(core.packageJson.dependencies.typebox).toBe("^1.3.9");
		expect(core.packageJson.bin).toBeUndefined();
		for (const entry of plan()) {
			for (const field of ["dependencies", "optionalDependencies"]) {
				for (const spec of Object.values(entry.packageJson[field] || {}) as string[]) {
					expect(spec).not.toMatch(/^https?:/);
					expect(spec).not.toMatch(/\.tgz$/);
				}
			}
		}
	});
});

describe("registry dependency guard", () => {
	it("rejects specifiers npm cannot verify and accepts ranges and aliases", () => {
		expect(() =>
			dependencies.assertRegistryDependencies({
				name: "prime-agent",
				dependencies: {
					"@earendil-works/pi-ai": "https://pub.example.dev/releases/v1.2.3/prime-agent-ai-1.2.3.tgz",
				},
			}),
		).toThrow(/must be a registry range/);
		expect(() =>
			dependencies.assertRegistryDependencies({
				name: "prime-agent",
				dependencies: { "@earendil-works/pi-ai": "npm:@primeintellect/prime-agent-ai@^1.2.3", chalk: "^5.5.0" },
			}),
		).not.toThrow();
	});

	it("still produces R2 tarball URLs for the installer channel", () => {
		const url = dependencies.tarballDependencySpec("https://pub.example.dev", "1.2.3", "prime-agent-ai-1.2.3.tgz");
		expect(url).toBe("https://pub.example.dev/releases/v1.2.3/prime-agent-ai-1.2.3.tgz");
		expect(
			dependencies.rewriteInternalDependencies(
				{ "@earendil-works/pi-ai": "^1.2.3", chalk: "^5.5.0" },
				new Map([["@earendil-works/pi-ai", url]]),
			),
		).toEqual({ "@earendil-works/pi-ai": url, chalk: "^5.5.0" });
	});
});

describe.skipIf(process.platform === "win32")("staged output", () => {
	it("writes one directory per package plus a manifest describing the publish order", () => {
		const { outDir, result } = stage();
		expect(result.status, result.stderr).toBe(0);
		const manifest = readJson(join(outDir, "manifest.json"));
		expect(manifest.version).toBe(version);
		expect(manifest.publishOrder).toEqual(plan().map((entry: any) => entry.name));
		for (const entry of manifest.packages)
			expect(existsSync(join(outDir, entry.directory, "package.json"))).toBe(true);
		expect(existsSync(join(outDir, "prime-agent/bin/prime-agent.cjs"))).toBe(true);
		expect(existsSync(join(outDir, "prime-agent/LICENSE"))).toBe(true);
		expect(existsSync(join(outDir, "@primeintellect/prime-agent-ai/dist/index.js"))).toBe(true);
	});

	it("carries the executable receipt into every platform package", () => {
		const { outDir, result } = stage();
		expect(result.status, result.stderr).toBe(0);
		for (const platform of platforms) {
			const packageDir = join(outDir, "@primeintellect", `prime-agent-${platform}`);
			const receipt = readJson(join(packageDir, "receipts.json"));
			const staged = join(packageDir, "bin/prime-agent");
			expect(receipt.executableSha256).toBe(sha256(join(binaryDir, platform, "prime-agent")));
			expect(receipt.executableSha256).toBe(sha256(staged));
			expect(receipt.platform).toBe(platform);
			expect(readJson(join(packageDir, "package.json")).primeAgent.executableSha256).toBe(receipt.executableSha256);
			expect(readJson(join(packageDir, "bin/package.json")).version).toBe(version);
			expect(existsSync(join(packageDir, "bin/prime-agent-runtime/pyproject.toml"))).toBe(true);
		}
	});

	it("refuses to pack when an R2 receipt disagrees with the compiled binary", () => {
		const receipts = join(root, "binaries.json");
		writeFileSync(
			receipts,
			JSON.stringify({
				version: `v${version}`,
				binaries: platforms.map((platform) => ({
					platform,
					file: `prime-agent-${version}-${platform}.tar.gz`,
					sha256: "c".repeat(64),
					executableSha256:
						platform === "linux-x64" ? "d".repeat(64) : sha256(join(binaryDir, platform, "prime-agent")),
				})),
			}),
		);
		const { result } = stage(["--receipts", receipts]);
		expect(result.status).not.toBe(0);
		expect(result.stderr).toContain("Receipt mismatch for linux-x64");
	});

	it("refuses to delete an output directory it did not create", () => {
		const outDir = mkdtempSync(join(root, "occupied-"));
		writeFileSync(join(outDir, "important.txt"), "keep me");
		const result = pack([
			"--binary-dir",
			binaryDir,
			"--packages-dir",
			packagesDir,
			"--out-dir",
			outDir,
			"--skip-pack",
		]);
		expect(result.status).not.toBe(0);
		expect(existsSync(join(outDir, "important.txt"))).toBe(true);
	});
});

describe.skipIf(process.platform === "win32")("bin shim resolution", () => {
	const key = `${process.platform}-${process.arch}`;

	function install(outDir: string, options: { platformPackage?: boolean } = {}) {
		const project = mkdtempSync(join(root, "project-"));
		const modules = join(project, "node_modules");
		cpSync(join(outDir, "prime-agent"), join(modules, "prime-agent"), { recursive: true });
		if (options.platformPackage !== false) {
			cpSync(
				join(outDir, "@primeintellect", `prime-agent-${key}`),
				join(modules, "@primeintellect", `prime-agent-${key}`),
				{ recursive: true },
			);
		}
		return { project, shim: join(modules, "prime-agent/bin/prime-agent.cjs") };
	}

	function launch(shim: string, project: string, args: string[] = [], env: Record<string, string> = {}) {
		return spawnSync(process.execPath, [shim, ...args], {
			cwd: project,
			encoding: "utf8",
			env: { ...process.env, ...env },
		});
	}

	it.skipIf(!platforms.includes(`${process.platform}-${process.arch}`))(
		"executes the platform binary, forwards arguments and propagates the exit code",
		() => {
			const { outDir, result } = stage();
			expect(result.status, result.stderr).toBe(0);
			const { project, shim } = install(outDir);
			const run = launch(shim, project, ["--print", "value"]);
			expect(run.stdout.trim()).toBe(`fixture ${key} --print value`);
			expect(run.status).toBe(7);
			expect(launch(shim, project, [], { PRIME_AGENT_VERIFY_BINARY: "1" }).status).toBe(7);
		},
	);

	it.skipIf(!platforms.includes(`${process.platform}-${process.arch}`))(
		"fails closed when the receipt does not match the installed binary",
		() => {
			const { outDir, result } = stage();
			expect(result.status, result.stderr).toBe(0);
			const { project, shim } = install(outDir);
			const binary = join(project, "node_modules/@primeintellect", `prime-agent-${key}`, "bin/prime-agent");
			writeFileSync(binary, "#!/bin/sh\nexit 0\n", { mode: 0o755 });
			const run = launch(shim, project, [], { PRIME_AGENT_VERIFY_BINARY: "1" });
			expect(run.status).toBe(1);
			expect(run.stderr).toContain("hash mismatch");
		},
	);

	it("explains how to recover when no platform package is installed", () => {
		const { outDir, result } = stage();
		expect(result.status, result.stderr).toBe(0);
		const { project, shim } = install(outDir, { platformPackage: false });
		const run = launch(shim, project);
		expect(run.status).toBe(1);
		expect(run.stderr).toContain("is not installed");
		expect(run.stderr).toContain("npm install @primeintellect/prime-agent-");
	});

	it("names the supported platforms when the host is not one of them", () => {
		const { outDir, result } = stage();
		expect(result.status, result.stderr).toBe(0);
		const { project, shim } = install(outDir, { platformPackage: false });
		const manifestPath = join(project, "node_modules/prime-agent/package.json");
		const manifest = readJson(manifestPath);
		delete manifest.primeAgent.platforms[key];
		writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));
		const run = launch(shim, project);
		expect(run.status).toBe(1);
		expect(run.stderr).toContain(`unsupported platform ${key}`);
		expect(run.stderr).toContain("install.prime-agent.dev");
	});
});
