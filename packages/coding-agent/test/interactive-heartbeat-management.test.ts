import { describe, expect, it, vi } from "vitest";
import type { AgentCronJob, AgentHeartbeatManagementAction } from "../src/core/cron-jobs.js";
import type {
	AgentConnectionHeartbeat,
	AgentConnectionRlmChildAgentSnapshot,
} from "../src/modes/agent-connection/types.js";
import { InteractiveMode } from "../src/modes/interactive/interactive-mode.js";

interface HeartbeatManagementHarness {
	heartbeatCatalog: AgentConnectionHeartbeat[];
	agentConnection: {
		manageHeartbeat(
			activeSessionId: string,
			jobId: string,
			action: AgentHeartbeatManagementAction,
		): Promise<AgentCronJob>;
	};
	connectionState: { activeSessionId: string };
	patchConnectionState(patch: { heartbeat: AgentCronJob | null }): void;
	applyHeartbeatCatalog(heartbeats: AgentConnectionHeartbeat[]): void;
	refreshHeartbeatCatalog(): Promise<void>;
	manageHeartbeat(heartbeat: AgentConnectionHeartbeat, action: AgentHeartbeatManagementAction): Promise<void>;
}

interface HeartbeatScopeHarness {
	heartbeatCatalog: AgentConnectionHeartbeat[];
	connectionState: { activeSessionId: string; sessionId: string };
	subagentSnapshots: Map<string, AgentConnectionRlmChildAgentSnapshot>;
	ui: { requestRender(): void };
	scheduleHeartbeatManagerRefresh(): void;
	updateSubagentSummaryLine(): void;
	applyHeartbeatCatalog(heartbeats: AgentConnectionHeartbeat[]): void;
	getScopedHeartbeats(): AgentConnectionHeartbeat[];
}

interface ChildIdentityUpdateHarness {
	subagentSnapshots: Map<string, AgentConnectionRlmChildAgentSnapshot>;
	ui: { requestRender(): void };
	updateSubagentSummary(child: AgentConnectionRlmChildAgentSnapshot): void;
	scheduleHeartbeatManagerRefresh(): void;
	updateSubagentSummaryLine(): void;
	updateWorkingPulse(): void;
	syncWorkingLoader(): void;
	updateWorkingLoaderMessage(): void;
}

interface HeartbeatRefreshHarness {
	heartbeatCatalog: AgentConnectionHeartbeat[];
	connectionState: { activeSessionId: string; sessionId: string };
	subagentSnapshots: Map<string, AgentConnectionRlmChildAgentSnapshot>;
	heartbeatManager: object | undefined;
	heartbeatManagerRefreshTimer: ReturnType<typeof setTimeout> | undefined;
	refreshHeartbeatCatalog(): Promise<void>;
	scheduleHeartbeatManagerRefresh(): void;
}

function heartbeat(overrides: Partial<AgentCronJob> = {}): AgentCronJob {
	return {
		id: "heartbeat-1",
		status: "active",
		source: "heartbeat",
		activeSessionId: "active-1",
		sessionId: "session-1",
		sessionFile: "/tmp/session.jsonl",
		cwd: "/tmp",
		prompt: "check the session",
		schedule: { kind: "interval", expression: "every 5m", intervalMs: 300_000 },
		createdAt: "2026-01-01T00:00:00.000Z",
		updatedAt: "2026-01-01T00:00:00.000Z",
		nextRunAt: "2026-01-01T00:05:00.000Z",
		runCount: 0,
		...overrides,
	};
}

describe("interactive heartbeat management", () => {
	// [action, resulting status, refresh outcome] - stop clears the local heartbeat,
	// pause keeps the updated job on the connection state.
	it.each([
		["stop", "cancelled", "ok"],
		// A failed catalog refresh must not turn a successful action into a failure.
		["pause", "paused", "throws"],
	] as const)("applies %s locally and still refreshes the catalog", async (action, status, refresh) => {
		const current = heartbeat();
		const updated = { ...current, status: status as AgentCronJob["status"], nextRunAt: undefined };
		const entry = { job: current, sessionName: "Primary session" };
		const seen: Array<{ heartbeat: AgentCronJob | null }> = [];
		const harness = Object.create(InteractiveMode.prototype) as HeartbeatManagementHarness;
		harness.heartbeatCatalog = [entry];
		harness.connectionState = { activeSessionId: current.activeSessionId };
		harness.agentConnection = { manageHeartbeat: vi.fn(async () => updated) };
		harness.patchConnectionState = (patch) => seen.push(patch);
		harness.applyHeartbeatCatalog = vi.fn();
		harness.refreshHeartbeatCatalog = vi.fn(async () => {
			if (refresh === "throws") throw new Error("worker recovering");
		});

		await expect(harness.manageHeartbeat(entry, action)).resolves.toBe(undefined);

		expect(seen).toEqual([{ heartbeat: action === "stop" ? null : updated }]);
		expect(harness.applyHeartbeatCatalog).toHaveBeenCalledWith(
			action === "stop" ? [] : [{ job: updated, sessionName: "Primary session" }],
		);
		expect(harness.refreshHeartbeatCatalog).toHaveBeenCalledOnce();
	});

	it("scopes the catalog to the current session and its subagents", () => {
		const own = { job: heartbeat() };
		const child = {
			job: heartbeat({ id: "heartbeat-2", activeSessionId: "active-2", sessionId: "session-2" }),
		};
		const unrelated = {
			job: heartbeat({ id: "heartbeat-3", activeSessionId: "active-3", sessionId: "session-3" }),
		};
		const harness = Object.create(InteractiveMode.prototype) as HeartbeatScopeHarness;
		harness.heartbeatCatalog = [];
		harness.connectionState = { activeSessionId: "active-1", sessionId: "session-1" };
		harness.subagentSnapshots = new Map([
			[
				"child-1",
				{
					id: "child-1",
					activeSessionId: "active-2",
					label: "child",
					status: "running",
					sessionDir: "/tmp/child-1",
				},
			],
		]);
		harness.ui = { requestRender: vi.fn() };
		harness.scheduleHeartbeatManagerRefresh = vi.fn();
		harness.updateSubagentSummaryLine = vi.fn();

		harness.applyHeartbeatCatalog([own, child, unrelated]);

		expect(harness.heartbeatCatalog).toEqual([own, child, unrelated]);
		expect(harness.getScopedHeartbeats()).toEqual([own, child]);
		expect(harness.updateSubagentSummaryLine).toHaveBeenCalledOnce();
	});

	it("refreshes heartbeat scope when a known subagent gains its active session id", () => {
		const existing: AgentConnectionRlmChildAgentSnapshot = {
			id: "child-1",
			label: "child",
			status: "running",
			sessionDir: "/tmp/child-1",
		};
		const harness = Object.create(InteractiveMode.prototype) as ChildIdentityUpdateHarness;
		harness.subagentSnapshots = new Map([[existing.id, existing]]);
		harness.ui = { requestRender: vi.fn() };
		harness.scheduleHeartbeatManagerRefresh = vi.fn();
		harness.updateSubagentSummaryLine = vi.fn();
		harness.updateWorkingPulse = vi.fn();
		harness.syncWorkingLoader = vi.fn();
		harness.updateWorkingLoaderMessage = vi.fn();

		harness.updateSubagentSummary({ ...existing, activeSessionId: "active-2" });

		expect(harness.subagentSnapshots.get(existing.id)?.activeSessionId).toBe("active-2");
		expect(harness.scheduleHeartbeatManagerRefresh).toHaveBeenCalledOnce();
	});

	// [name, now, nextRunAt, follow-up scheduling during the wait]
	it.each([
		[
			"arms a refresh for the next scheduled run",
			"2026-01-01T00:00:00.000Z",
			"2026-01-01T00:00:01.000Z",
			async () => {
				await vi.advanceTimersByTimeAsync(1_250);
			},
		],
		[
			// nextRunAt (00:05) is already in the past, so the 5s overdue fallback applies.
			// Subagent snapshot updates re-derive the schedule more often than every 5s;
			// they must not postpone the pending overdue refresh.
			"keeps the overdue deadline when updates re-derive the schedule",
			"2026-01-01T00:10:00.000Z",
			"2026-01-01T00:05:00.000Z",
			async (harness: HeartbeatRefreshHarness) => {
				for (let i = 0; i < 5; i++) {
					await vi.advanceTimersByTimeAsync(1_000);
					harness.scheduleHeartbeatManagerRefresh();
				}
			},
		],
		[
			// The first schedule arms the capped 60s poll; a sooner heartbeat must pull the
			// pending refresh forward instead of sitting behind that armed poll.
			"re-arms to an earlier deadline when a sooner heartbeat appears",
			"2026-01-01T00:00:00.000Z",
			"2026-01-01T00:02:00.000Z",
			async (harness: HeartbeatRefreshHarness) => {
				harness.heartbeatCatalog = [{ job: { ...heartbeat(), nextRunAt: "2026-01-01T00:00:02.000Z" } }];
				harness.scheduleHeartbeatManagerRefresh();
				await vi.advanceTimersByTimeAsync(3_000);
			},
		],
	])("%s", async (_name, now, nextRunAt, drive) => {
		vi.useFakeTimers();
		try {
			vi.setSystemTime(new Date(now));
			const harness = Object.create(InteractiveMode.prototype) as HeartbeatRefreshHarness;
			harness.heartbeatCatalog = [{ job: { ...heartbeat(), nextRunAt } }];
			harness.connectionState = { activeSessionId: "active-1", sessionId: "session-1" };
			harness.subagentSnapshots = new Map();
			harness.heartbeatManager = {};
			harness.heartbeatManagerRefreshTimer = undefined;
			harness.refreshHeartbeatCatalog = vi.fn(async () => {});

			harness.scheduleHeartbeatManagerRefresh();
			await drive(harness);

			expect(harness.refreshHeartbeatCatalog).toHaveBeenCalledOnce();
		} finally {
			vi.useRealTimers();
		}
	});
});
