import { randomBytes } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

/** Environment variable checked for the daemon TCP port (after the CLI flag, before settings). */
export const DAEMON_TCP_PORT_ENV = "PRIME_AGENT_DAEMON_PORT";

/** Auth verdict for one TCP command line. */
export interface DaemonTcpAuthVerdict {
	ok: boolean;
	/** Correlatable response id when the line was JSON. */
	id: string;
	/** Best-effort command name for the failure response. */
	command: string | undefined;
	reason: string;
}

export interface DaemonTcpTokenRecord {
	token: string;
	tokenPath: string;
	/** True when this load call created the token (first time). */
	created?: boolean;
}

/** Token file path inside the agent dir. */
export function daemonTcpTokenPath(agentDir: string): string {
	return join(agentDir, "daemon-tcp-token");
}

/**
 * Parse the port from an environment map. Throws a named error when the
 * variable is present but not a valid port (never silently ignored).
 */
export function daemonTcpPortFromEnv(env: Record<string, string | undefined>): number | undefined {
	const raw = env[DAEMON_TCP_PORT_ENV];
	if (raw === undefined || raw === "") {
		return undefined;
	}
	const port = Number(raw);
	if (!Number.isInteger(port) || port < 1 || port > 65535) {
		throw new Error(`Invalid ${DAEMON_TCP_PORT_ENV}: "${raw}" (expected an integer between 1 and 65535)`);
	}
	return port;
}

/**
 * Resolve the daemon TCP port. Precedence: explicit CLI flag > env var >
 * settings `daemonPort`. Returns undefined when no source provides a valid port.
 */
export function resolveDaemonTcpPort(
	explicit: number | undefined,
	settingsPort: number | undefined,
	env: Record<string, string | undefined> = process.env,
): number | undefined {
	if (typeof explicit === "number" && Number.isInteger(explicit) && explicit >= 1 && explicit <= 65535) {
		return explicit;
	}
	return daemonTcpPortFromEnv(env) ?? settingsPort;
}

/** Timing-safe token comparison that does not leak length differences. */
export function daemonTcpTokensMatch(actual: string, expected: string): boolean {
	if (actual.length !== expected.length) {
		return false;
	}
	let mismatch = 0;
	for (let i = 0; i < actual.length; i++) {
		mismatch |= actual.charCodeAt(i) ^ expected.charCodeAt(i);
	}
	return mismatch === 0;
}

/** Read the existing token without creating one. Returns undefined when unset. */
export function readDaemonTcpToken(agentDir: string): string | undefined {
	const tokenPath = daemonTcpTokenPath(agentDir);
	if (!existsSync(tokenPath)) {
		return undefined;
	}
	const raw = readFileSync(tokenPath, "utf8").trim();
	if (!raw) {
		throw new Error(`daemon TCP token file ${tokenPath} is empty`);
	}
	try {
		const parsed = JSON.parse(raw) as { token?: unknown };
		if (typeof parsed.token !== "string" || parsed.token.length === 0) {
			throw new Error(`daemon TCP token file ${tokenPath} is missing its token`);
		}
		return parsed.token;
	} catch (error) {
		if (error instanceof SyntaxError) {
			throw new Error(`daemon TCP token file ${tokenPath} is not valid JSON`);
		}
		throw error;
	}
}

/**
 * Load or create the per-machine token used to authenticate TCP lines.
 * The token is stored as JSON in `<agentDir>/daemon-tcp-token`.
 */
export function loadOrCreateDaemonTcpToken(agentDir: string): DaemonTcpTokenRecord {
	const tokenPath = daemonTcpTokenPath(agentDir);
	try {
		const token = readDaemonTcpToken(agentDir);
		if (token !== undefined) {
			return { token, tokenPath, created: false };
		}
	} catch (error) {
		// Refuse to overwrite a corrupt token file
		throw error;
	}
	mkdirSync(agentDir, { recursive: true });
	const token = randomBytes(32).toString("base64url");
	writeFileSync(tokenPath, JSON.stringify({ token }) + "\n", { mode: 0o600 });
	return { token, tokenPath, created: true };
}

/**
 * Check that a TCP command line carries the expected per-machine token.
 * Supports both the raw format ({"id","type","auth":{"token"}}) and the
 * daemon envelope format ({"type":"command","command":{...},"auth":{"token"}}).
 */
export function checkDaemonTcpLineAuth(line: string, expectedToken: string): DaemonTcpAuthVerdict {
	let parsed: {
		id?: string;
		type?: string;
		auth?: { token?: unknown };
		command?: { type?: string };
	};
	try {
		parsed = JSON.parse(line) as typeof parsed;
	} catch {
		return { ok: false, id: "unknown", command: undefined, reason: "invalid_json" };
	}
	const id = typeof parsed.id === "string" ? parsed.id : "unknown";
	const command = parsed.type ?? parsed.command?.type;
	const token = parsed.auth?.token;
	if (typeof token === "string" && token.length > 0 && daemonTcpTokensMatch(token, expectedToken)) {
		return { ok: true, id, command, reason: "" };
	}
	const reason = token === undefined || token === "" ? "missing_token" : "wrong_token";
	return { ok: false, id, command, reason };
}
