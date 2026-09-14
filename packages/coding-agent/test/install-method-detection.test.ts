import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, test } from "vitest";
import { getNativeUpdatePlan } from "../src/cli/native-update.js";
import {
	detectInstallMethod,
	getSelfUpdateCommand,
	getSelfUpdateUnavailableInstruction,
	getUpdateInstruction,
	isHomebrewManagedPath,
} from "../src/config.js";

/**
 * Install-source detection for COMPILED copies.
 *
 * Homebrew now ships the compiled binary through our own tap formula, and npm ships it through
 * per-platform packages. Both put a `bun --compile` executable somewhere that a package manager
 * owns, so "this is a compiled binary" can no longer imply "the self-updater owns it".
 */

const execPathDescriptor = Object.getOwnPropertyDescriptor(process, "execPath");
const originalPiPackageDir = process.env.PI_PACKAGE_DIR;
let tempDir: string | undefined;

function setExecPath(value: string): void {
	Object.defineProperty(process, "execPath", { value, configurable: true });
}

afterEach(() => {
	if (execPathDescriptor) Object.defineProperty(process, "execPath", execPathDescriptor);
	if (originalPiPackageDir === undefined) delete process.env.PI_PACKAGE_DIR;
	else process.env.PI_PACKAGE_DIR = originalPiPackageDir;
	if (tempDir) rmSync(tempDir, { recursive: true, force: true });
	tempDir = undefined;
});

/** A formula-installed keg: the compiled binary lives in `<keg>/bin`, linked from `<prefix>/bin`. */
function createHomebrewBinaryInstall(): { keg: string; executable: string; link: string } {
	const prefix = mkdtempSync(join(tmpdir(), "pi-brew-binary-"));
	tempDir = prefix;
	const keg = join(prefix, "Cellar", "prime-agent", "0.9.5");
	const binDir = join(keg, "bin");
	mkdirSync(binDir, { recursive: true });
	const executable = join(binDir, "prime-agent");
	writeFileSync(executable, "#!/bin/sh\n");
	mkdirSync(join(prefix, "bin"), { recursive: true });
	const link = join(prefix, "bin", "prime-agent");
	symlinkSync(executable, link);
	process.env.PI_PACKAGE_DIR = binDir;
	setExecPath(executable);
	return { keg, executable, link };
}

/** An npm per-platform package: the same compiled binary, under a global node_modules tree. */
function createNpmBinaryInstall(): { executable: string } {
	const prefix = mkdtempSync(join(tmpdir(), "pi-npm-binary-"));
	tempDir = prefix;
	const packageDir = join(prefix, "lib", "node_modules", "@primeintellect", "prime-agent-darwin-arm64");
	const binDir = join(packageDir, "bin");
	mkdirSync(binDir, { recursive: true });
	const executable = join(binDir, "prime-agent");
	writeFileSync(executable, "#!/bin/sh\n");
	process.env.PI_PACKAGE_DIR = binDir;
	setExecPath(executable);
	return { executable };
}

describe("isHomebrewManagedPath", () => {
	test("matches a keg, whatever the prefix or the layout inside it", () => {
		expect(isHomebrewManagedPath("/opt/homebrew/Cellar/prime-agent/0.9.5/bin/prime-agent")).toBe(true);
		expect(isHomebrewManagedPath("/usr/local/Cellar/prime-agent/0.9.5/bin/prime-agent")).toBe(true);
		expect(
			isHomebrewManagedPath(
				"/home/linuxbrew/.linuxbrew/Cellar/prime-agent/0.9.5/libexec/lib/node_modules/prime-agent",
			),
		).toBe(true);
		expect(isHomebrewManagedPath("C:\\brew\\Cellar\\prime-agent\\0.9.5\\bin")).toBe(true);
	});

	test("does not match paths that merely contain the word cellar", () => {
		expect(isHomebrewManagedPath("/Users/kevin/wine-cellar/prime-agent")).toBe(false);
		expect(isHomebrewManagedPath("/Users/kevin/.local/share/prime-agent/releases/0.9.5/prime-agent")).toBe(false);
	});
});

describe("detectInstallMethod for compiled copies", () => {
	test("a brew-installed compiled binary is Homebrew's, not the self-updater's", () => {
		createHomebrewBinaryInstall();

		expect(detectInstallMethod()).toBe("homebrew");
		expect(getSelfUpdateCommand("prime-agent")).toBeUndefined();
		expect(getSelfUpdateUnavailableInstruction("prime-agent")).toBe("Update with: brew upgrade prime-agent");
		expect(getUpdateInstruction("prime-agent")).toBe("Update with: brew upgrade prime-agent");
	});

	test("a brew copy invoked through the prefix symlink is still detected", () => {
		const { link } = createHomebrewBinaryInstall();
		// PI_PACKAGE_DIR unset and execPath is `<prefix>/bin/prime-agent`, which is not keg-shaped.
		// Only resolving the symlink reveals the keg, so detection must do that.
		delete process.env.PI_PACKAGE_DIR;
		setExecPath(link);

		expect(isHomebrewManagedPath(link)).toBe(false);
		expect(detectInstallMethod()).toBe("homebrew");
		expect(getUpdateInstruction("prime-agent")).toBe("Update with: brew upgrade prime-agent");
	});

	test("the self-updater refuses to rewrite a Homebrew keg", async () => {
		const { executable } = createHomebrewBinaryInstall();

		await expect(getNativeUpdatePlan({ force: true, rollback: false, executable })).rejects.toThrow(
			"managed by Homebrew. Update it with: brew upgrade prime-agent",
		);
	});

	test("an npm-installed compiled binary reports npm and gets an npm instruction", () => {
		createNpmBinaryInstall();

		expect(detectInstallMethod()).toBe("npm");
		expect(getUpdateInstruction("prime-agent")).toBe("Run: npm install -g prime-agent");
	});
});
