import { execFileSync } from "node:child_process";
import { accessSync, constants, readFileSync } from "node:fs";
import { join, relative } from "node:path";
import { APP_NAME, isHomebrewManagedPath, type SelfUpdateCommand } from "../config.js";
import {
	getNativeInstallationTarget,
	readNativeInstallation,
	readNativeRollbackInstallation,
} from "../utils/native-installation.js";
import { getPiUserAgent } from "../utils/pi-user-agent.js";
import { fetchVerifiedReleaseArtifactDigest, ReleaseSignatureError } from "../utils/release-signature.js";
import {
	getLatestPiRelease,
	isBaseVersionDowngrade,
	isReleaseUpdateCandidate,
	type UpdateChannel,
} from "../utils/version-check.js";

/** The release manifest for the requested channel could not be resolved; the installed version was kept. */
export class NativeReleaseUnavailableError extends Error {
	constructor(cause?: unknown) {
		super(
			cause instanceof Error
				? `Could not resolve a compiled release: ${cause.message}. The installed version was kept.`
				: "Could not resolve a compiled release. The installed version was kept.",
			cause instanceof Error ? { cause } : undefined,
		);
		this.name = "NativeReleaseUnavailableError";
	}
}

export interface NativeUpdatePlan {
	command?: SelfUpdateCommand;
	targetVersion: string;
	/** Set when the channel's current release has a lower base version than the installed one; nothing is planned. */
	refusedDowngradeTo?: string;
	/** Set when PRIME_AGENT_DOWNLOAD_BASE_URL moved the origin away from the recorded install source. */
	overriddenBaseUrl?: string;
	/** Certificate identity that signed the SHA256SUMS this plan trusts. Absent for rollbacks. */
	verifiedSignerIdentity?: string;
}

/**
 * Read the development origin override.
 *
 * The override may move WHERE bytes come from; it can never change WHAT is accepted. The cosign
 * signature over SHA256SUMS is still required, and the pinned signer identity is compiled in, so an
 * attacker-controlled origin cannot serve an installable artifact. The override must be an absolute
 * https URL; the previous code accepted any string and quietly replaced the recorded
 * `.install-source`, which made the redirection invisible to the user.
 */
function readDownloadBaseUrlOverride(): string | undefined {
	const raw = process.env.PRIME_AGENT_DOWNLOAD_BASE_URL?.trim();
	if (!raw) return undefined;
	let parsed: URL;
	try {
		parsed = new URL(raw);
	} catch {
		throw new Error(`PRIME_AGENT_DOWNLOAD_BASE_URL is not a valid URL: ${raw}`);
	}
	if (parsed.protocol !== "https:")
		throw new Error(`PRIME_AGENT_DOWNLOAD_BASE_URL must use https, got ${parsed.protocol}//.`);
	return raw.replace(/\/+$/, "");
}

export async function getNativeUpdatePlan(options: {
	force: boolean;
	rollback: boolean;
	channel?: UpdateChannel;
	executable?: string;
}): Promise<NativeUpdatePlan> {
	// A Homebrew keg is Homebrew's to replace. Check before anything else so that a brew copy can
	// never be talked into rewriting itself, even if it otherwise looks installer-shaped.
	const executablePath = options.executable ?? process.execPath;
	if (isHomebrewManagedPath(executablePath))
		throw new Error(`This ${APP_NAME} copy is managed by Homebrew. Update it with: brew upgrade ${APP_NAME}`);
	const current = getNativeInstallationTarget(options.executable);
	if (!current)
		throw new Error(
			"This compiled application is not owned by the Prime Agent installer. Update it using its original installer.",
		);
	const active = readNativeInstallation(current.root);
	const installation = active ?? readNativeInstallation(current.root, "previous");
	if (!installation || installation.platform !== current.platform)
		throw new Error("The compiled installation is damaged. Run the published installer again to repair it.");
	if (
		(options.rollback || !active) &&
		!/^# prime-agent-native-recovery-v1$/m.test(readFileSync(join(installation.releaseDir, "install.sh"), "utf8"))
	)
		throw new Error(
			"The retained installer does not support this recovery. Run the published installer at https://app.primeintellect.ai/prime-agent/install.sh again to repair it.",
		);
	accessSync(installation.root, constants.W_OK);
	accessSync(join(installation.root, "bin"), constants.W_OK);
	let version: string;
	let checksum: string | undefined;
	let signerIdentity: string | undefined;
	let previousTarget: string | undefined;
	const overriddenBaseUrl = readDownloadBaseUrlOverride();
	const baseUrl = overriddenBaseUrl ?? installation.baseUrl;
	if (options.rollback) {
		const previous = readNativeRollbackInstallation(installation.root);
		if (!previous || previous.executable === current.executable)
			throw new Error("No valid previous compiled release is available.");
		let reportedVersion: string;
		try {
			reportedVersion = execFileSync(previous.executable, ["--version"], {
				encoding: "utf8",
				timeout: 10000,
			});
		} catch {
			throw new Error("The previous compiled release executable could not be validated.");
		}
		if (reportedVersion !== previous.version && reportedVersion !== `${previous.version}\n`)
			throw new Error("The previous compiled release executable reports a different version.");
		try {
			execFileSync(previous.executable, ["--help"], { stdio: "ignore", timeout: 10000 });
		} catch {
			throw new Error("The previous compiled release executable failed its help probe.");
		}
		version = previous.version;
		previousTarget = relative(join(installation.root, "bin"), previous.executable);
	} else {
		let release: Awaited<ReturnType<typeof getLatestPiRelease>>;
		try {
			release = await getLatestPiRelease(current.version, { baseUrl, channel: options.channel });
		} catch (error) {
			// Network, timeout, and malformed-manifest failures all mean the same thing here: nothing to install.
			throw new NativeReleaseUnavailableError(error);
		}
		if (!release || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(release.version))
			throw new NativeReleaseUnavailableError();
		if (active && isBaseVersionDowngrade(release.version, current.version))
			return { targetVersion: current.version, refusedDowngradeTo: release.version };
		if (active && !options.force && !isReleaseUpdateCandidate(release.version, current.version, options.channel))
			return { targetVersion: current.version };
		const artifact = release.binaries?.find((entry) => entry.platform === current.platform);
		if (!artifact) throw new Error(`No verified compiled archive is available for ${current.platform}.`);
		version = release.version;
		// The manifest and the archive come from the same origin, so the manifest digest proves
		// nothing on its own. Take the digest from a cosign-signed SHA256SUMS instead, and treat any
		// disagreement between the two as tampering. Verification is mandatory on every origin,
		// including one supplied through PRIME_AGENT_DOWNLOAD_BASE_URL.
		const verification = await fetchVerifiedReleaseArtifactDigest({
			baseUrl,
			version,
			file: artifact.file,
			userAgent: getPiUserAgent(current.version),
		});
		if (verification.digest !== artifact.sha256)
			throw new ReleaseSignatureError(
				`The signed SHA256SUMS for ${version} does not match the release manifest for ${artifact.file}. The installed version was kept.`,
			);
		checksum = verification.digest;
		signerIdentity = verification.signerIdentity;
	}
	const environment = {
		PRIME_AGENT_INSTALL_METHOD: "binary",
		PRIME_AGENT_INSTALL_DIR: installation.root,
		PRIME_AGENT_DOWNLOAD_BASE_URL: baseUrl,
		PRIME_AGENT_INSTALL_LINK: "0",
		PRIME_AGENT_INSTALLER_NONINTERACTIVE: "1",
		PRIME_AGENT_INSTALLER_PLAIN: "1",
		PRIME_AGENT_BOOTSTRAP_KERNEL_ON_INSTALL: "0",
		PRIME_AGENT_EXPECTED_CURRENT: relative(join(current.root, "bin"), current.executable),
		...(previousTarget ? { PRIME_AGENT_EXPECTED_PREVIOUS: previousTarget } : {}),
		...(checksum ? { PRIME_AGENT_EXPECTED_SHA256: checksum } : {}),
	};
	return {
		targetVersion: version,
		...(overriddenBaseUrl ? { overriddenBaseUrl } : {}),
		...(signerIdentity ? { verifiedSignerIdentity: signerIdentity } : {}),
		command: {
			command: "/usr/bin/env",
			args: [
				...Object.entries(environment).map(([name, value]) => `${name}=${value}`),
				"sh",
				join(installation.releaseDir, "install.sh"),
				options.rollback ? "--rollback" : version,
			],
			display: `${APP_NAME} update${options.rollback ? " --rollback" : options.force ? " --force" : ""}`,
		},
	};
}
