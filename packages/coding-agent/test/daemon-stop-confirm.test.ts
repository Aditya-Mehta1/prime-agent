import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { RunningDaemonProbe } from "../src/cli/daemon-launch.js";
import { confirmDaemonSessionLoss, type DaemonSessionLossCopy } from "../src/cli/daemon-stop-confirm.js";
import type { SessionSummary } from "../src/modes/daemon/daemon-session-list.js";

const COPY: DaemonSessionLossCopy = {
	busyDetail: (count) => `busy:${count}`,
	unlistableDetail: "unlistable",
	question: "Continue?",
	nonTtyHint: "hint",
};

function session(overrides: Partial<SessionSummary>): SessionSummary {
	return {
		isStreaming: false,
		isCompacting: false,
		sessionActions: { queuedCount: 0, steering: [], followUps: [] },
		...overrides,
	} as unknown as SessionSummary;
}

const ttyDescriptor = Object.getOwnPropertyDescriptor(process.stdin, "isTTY");
function setTTY(value: boolean): void {
	Object.defineProperty(process.stdin, "isTTY", { value, configurable: true });
}

describe("confirmDaemonSessionLoss", () => {
	beforeEach(() => {
		vi.spyOn(console, "error").mockImplementation(() => {});
	});
	afterEach(() => {
		vi.restoreAllMocks();
		if (ttyDescriptor) {
			Object.defineProperty(process.stdin, "isTTY", ttyDescriptor);
		}
	});

	// Stopping the daemon while a session is working loses that work, so the guard must
	// only proceed unattended when nothing is busy.
	it.each([
		["proceeds when the daemon is unreachable", true, { reachable: false } as RunningDaemonProbe, false, true],
		[
			"proceeds when force is set",
			true,
			{ reachable: true, activeSessions: [session({ isStreaming: true })] } as RunningDaemonProbe,
			true,
			true,
		],
		[
			"proceeds when no session is busy",
			true,
			{ reachable: true, activeSessions: [session({ isStreaming: false }), session({})] } as RunningDaemonProbe,
			false,
			true,
		],
		[
			"aborts when a queued session is busy off a TTY",
			false,
			{
				reachable: true,
				activeSessions: [
					session({ isSessionActive: true, sessionActions: { queuedCount: 2, steering: [], followUps: [] } }),
				],
			} as RunningDaemonProbe,
			false,
			false,
		],
		[
			"aborts when a compacting session is busy off a TTY",
			false,
			{
				reachable: true,
				activeSessions: [session({ isSessionActive: true, isCompacting: true })],
			} as RunningDaemonProbe,
			false,
			false,
		],
		[
			"aborts when a streaming session is busy off a TTY",
			false,
			{
				reachable: true,
				activeSessions: [session({ isSessionActive: true, isStreaming: true })],
			} as RunningDaemonProbe,
			false,
			false,
		],
		[
			"aborts when a running bash session is busy off a TTY",
			false,
			{
				reachable: true,
				activeSessions: [session({ isSessionActive: true, isBashRunning: true })],
			} as RunningDaemonProbe,
			false,
			false,
		],
		[
			"aborts when a session with running RLM children is busy off a TTY",
			false,
			{ reachable: true, activeSessions: [session({ hasRunningRlmChildren: true })] } as RunningDaemonProbe,
			false,
			false,
		],
		[
			"aborts when only client-owned sessions are busy off a TTY",
			false,
			{ reachable: true, activeSessions: [], busyClientOwnedSessionCount: 2 } as RunningDaemonProbe,
			false,
			false,
		],
		[
			"aborts when sessions cannot be listed off a TTY",
			false,
			{ reachable: true } as RunningDaemonProbe,
			false,
			false,
		],
	])("%s", async (_name, tty, probe, force, expected) => {
		setTTY(tty);

		expect(await confirmDaemonSessionLoss(probe, { force, copy: COPY })).toBe(expected);
	});
});
