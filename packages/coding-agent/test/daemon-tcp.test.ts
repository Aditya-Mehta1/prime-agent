import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";

const tokenRace = vi.hoisted(() => ({ armed: false, winnerToken: "" }));

vi.mock("node:fs", async (importOriginal) => {
	const actual = await importOriginal<typeof import("node:fs")>();
	return {
		...actual,
		writeFileSync: (path: unknown, data: unknown, options: unknown) => {
			if (tokenRace.armed && typeof path === "string" && path.endsWith("daemon-tcp-token")) {
				tokenRace.armed = false;
				const winnerLine = `${JSON.stringify({ token: tokenRace.winnerToken })}\n`;
				actual.writeFileSync(path, winnerLine, { mode: 0o600 });
				throw Object.assign(new Error("concurrent creator already wrote the token"), { code: "EEXIST" });
			}
			return actual.writeFileSync(path as never, data as never, options as never);
		},
	};
});

const { checkDaemonTcpLineAuth, loadOrCreateDaemonTcpToken, resolveDaemonTcpPort } = await import(
	"../src/modes/daemon/daemon-tcp.js"
);

const tempDirs: string[] = [];

afterEach(() => {
	for (const directory of tempDirs.splice(0)) rmSync(directory, { recursive: true, force: true });
	tokenRace.armed = false;
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
		expect(readFileSync(join(agentDir, "daemon-tcp-token"), "utf8")).toContain(first.token);
	});

	it("creates the token file with owner-only permissions", () => {
		const agentDir = tempAgentDir();
		const { tokenPath } = loadOrCreateDaemonTcpToken(agentDir);
		expect(statSync(tokenPath).mode & 0o777).toBe(0o600);
	});

	it("refuses corrupt token files instead of overwriting them", () => {
		const agentDir = tempAgentDir();
		writeFileSync(join(agentDir, "daemon-tcp-token"), "{ not json", { mode: 0o600 });
		expect(() => loadOrCreateDaemonTcpToken(agentDir)).toThrow(/not valid JSON/);
		writeFileSync(join(agentDir, "daemon-tcp-token"), JSON.stringify({ version: 1 }), { mode: 0o600 });
		expect(() => loadOrCreateDaemonTcpToken(agentDir)).toThrow(/missing its token/);
		expect(readFileSync(join(agentDir, "daemon-tcp-token"), "utf8")).toContain(JSON.stringify({ version: 1 }));
	});

	it("reuses the concurrent winner's token when the exclusive create loses", () => {
		const agentDir = tempAgentDir();
		tokenRace.winnerToken = "winner-token-value-0123456789abcdef";
		tokenRace.armed = true;

		const record = loadOrCreateDaemonTcpToken(agentDir);

		expect(record.token).toBe("winner-token-value-0123456789abcdef");
		expect(record.created).toBe(false);
		expect(readFileSync(join(agentDir, "daemon-tcp-token"), "utf8")).toContain("winner-token-value-0123456789abcdef");
	});
});

describe("daemon tcp port resolution", () => {
	it("prefers the CLI flag > env > settings, rejecting invalid sources", () => {
		expect(resolveDaemonTcpPort(4100, 4200, { PRIME_AGENT_DAEMON_PORT: "4300" })).toBe(4100);
		expect(resolveDaemonTcpPort(undefined, 4200, { PRIME_AGENT_DAEMON_PORT: "4300" })).toBe(4300);
		expect(resolveDaemonTcpPort(undefined, 4200, {})).toBe(4200);
		expect(resolveDaemonTcpPort(undefined, undefined, {})).toBeUndefined();
		expect(resolveDaemonTcpPort(70000, undefined, {})).toBeUndefined();
		expect(resolveDaemonTcpPort(0, 8123, {})).toBe(8123);
		expect(() => resolveDaemonTcpPort(undefined, undefined, { PRIME_AGENT_DAEMON_PORT: "not-a-port" })).toThrow(
			/PRIME_AGENT_DAEMON_PORT/,
		);
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
		const envelopeLine = `{"type":"command","id":"t6","command":{"type":"list"},"auth":{"token":"x"}}`;
		expect(checkDaemonTcpLineAuth(envelopeLine, token)).toMatchObject({ ok: false, id: "t6", command: "list" });
		expect(checkDaemonTcpLineAuth("not json at all", token)).toMatchObject({ ok: false, reason: "invalid_json" });
	});

	it("refuses JSON primitive lines instead of dereferencing them", () => {
		// `null` in particular used to throw from the socket data handler.
		for (const line of ["null", "5", '"str"', "true"]) {
			expect(checkDaemonTcpLineAuth(line, token)).toMatchObject({ ok: false, reason: "invalid_json" });
		}
	});
});
