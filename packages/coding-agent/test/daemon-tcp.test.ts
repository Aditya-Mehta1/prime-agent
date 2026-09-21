import { existsSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import {
	checkDaemonTcpLineAuth,
	DAEMON_TCP_PORT_ENV,
	daemonTcpPortFromEnv,
	daemonTcpTokenPath,
	daemonTcpTokensMatch,
	loadOrCreateDaemonTcpToken,
	readDaemonTcpToken,
	resolveDaemonTcpPort,
} from "../src/modes/daemon/daemon-tcp.js";

const tempDirs: string[] = [];

afterEach(() => {
	for (const directory of tempDirs.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function tempAgentDir(): string {
	const directory = mkdtempSync(join(tmpdir(), "prime-daemon-tcp-test-"));
	tempDirs.push(directory);
	return directory;
}

describe("daemon tcp token store", () => {
	it("creates a token on first load and keeps it stable across restarts", () => {
		const agentDir = tempAgentDir();
		const first = loadOrCreateDaemonTcpToken(agentDir);
		expect(first.created).toBe(true);
		expect(first.token).toMatch(/^[A-Za-z0-9_-]{43}$/);
		const second = loadOrCreateDaemonTcpToken(agentDir);
		expect(second.created).toBe(false);
		expect(second.token).toBe(first.token);
		expect(readFileSync(daemonTcpTokenPath(agentDir), "utf8")).toContain(first.token);
	});

	it("creates the token file with owner-only permissions", () => {
		const agentDir = tempAgentDir();
		const { tokenPath } = loadOrCreateDaemonTcpToken(agentDir);
		expect(statSync(tokenPath).mode & 0o777).toBe(0o600);
	});

	it("reads without creating when unset and refuses corrupt files", () => {
		const agentDir = tempAgentDir();
		expect(readDaemonTcpToken(agentDir)).toBeUndefined();
		expect(existsSync(daemonTcpTokenPath(agentDir))).toBe(false);
		loadOrCreateDaemonTcpToken(agentDir);
		expect(readDaemonTcpToken(agentDir)).toBe(loadOrCreateDaemonTcpToken(agentDir).token);
		writeFileSync(daemonTcpTokenPath(agentDir), "{ not json", { mode: 0o600 });
		expect(() => readDaemonTcpToken(agentDir)).toThrow(/not valid JSON/);
		writeFileSync(daemonTcpTokenPath(agentDir), JSON.stringify({ version: 1 }), { mode: 0o600 });
		expect(() => loadOrCreateDaemonTcpToken(agentDir)).toThrow(/missing its token/);
	});
});

describe("daemon tcp port resolution", () => {
	it("prefers the CLI flag > env > settings, rejecting invalid sources", () => {
		expect(resolveDaemonTcpPort(4100, 4200, { [DAEMON_TCP_PORT_ENV]: "4300" })).toBe(4100);
		expect(resolveDaemonTcpPort(undefined, 4200, { [DAEMON_TCP_PORT_ENV]: "4300" })).toBe(4300);
		expect(resolveDaemonTcpPort(undefined, 4200, {})).toBe(4200);
		expect(resolveDaemonTcpPort(undefined, undefined, {})).toBeUndefined();
		expect(resolveDaemonTcpPort(70000, undefined, {})).toBeUndefined();
		expect(resolveDaemonTcpPort(0, 8123, {})).toBe(8123);
		expect(() => daemonTcpPortFromEnv({ [DAEMON_TCP_PORT_ENV]: "not-a-port" })).toThrow(new RegExp(DAEMON_TCP_PORT_ENV));
		expect(daemonTcpPortFromEnv({})).toBeUndefined();
	});
});

describe("daemon tcp line auth", () => {
	const token = "test-token-value-0123456789";

	it("accepts raw and envelope command lines with the right token", () => {
		expect(checkDaemonTcpLineAuth(`{"id":"t1","type":"list","auth":{"token":"${token}"}}`, token).ok).toBe(true);
		expect(
			checkDaemonTcpLineAuth(
				`{"type":"command","id":"t2","protocol":{"name":"prime-agent.daemon","version":7},"command":{"type":"list"},"auth":{"token":"${token}"}}`,
				token,
			).ok,
		).toBe(true);
	});

	it("refuses bad tokens and compares without leaking length differences", () => {
		const missing = checkDaemonTcpLineAuth(`{"id":"t3","type":"list"}`, token);
		expect(missing).toMatchObject({ ok: false, reason: "missing_token", id: "t3", command: "list" });
		const wrong = checkDaemonTcpLineAuth(`{"id":"t4","type":"list","auth":{"token":"nope"}}`, token);
		expect(wrong).toMatchObject({ ok: false, reason: "wrong_token", id: "t4" });
		const empty = checkDaemonTcpLineAuth(`{"id":"t5","type":"list","auth":{"token":""}}`, token);
		expect(empty).toMatchObject({ ok: false, reason: "missing_token" });
		expect(checkDaemonTcpLineAuth("not json at all", token)).toMatchObject({ ok: false, reason: "invalid_json" });
		expect(daemonTcpTokensMatch(token, token)).toBe(true);
		expect(daemonTcpTokensMatch(token, `${token}x`)).toBe(false);
		expect(daemonTcpTokensMatch(token, "test-token-value-0123456788")).toBe(false);
	});
});
