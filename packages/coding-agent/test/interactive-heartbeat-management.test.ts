import { setKeybindings } from "@earendil-works/pi-tui";
import stripAnsi from "strip-ansi";
import { beforeAll, describe, expect, it, vi } from "vitest";
import type { AgentCronJob, AgentHeartbeatManagementAction } from "../src/core/cron-jobs.js";
import { KeybindingsManager } from "../src/core/keybindings.js";
import type {
	AgentConnectionHeartbeat,
	AgentConnectionRlmChildAgentSnapshot,
} from "../src/modes/agent-connection/types.js";
import type { HeartbeatManagerComponent } from "../src/modes/interactive/components/heartbeat-manager.js";
import { HEARTBEAT_REFRESH_FETCH_TIMEOUT_MS, InteractiveMode } from "../src/modes/interactive/interactive-mode.js";
import { initTheme } from "../src/modes/interactive/theme/theme.js";

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
	it("clears local user-heartbeat state after stopping from the manager", async () => {
		const current = heartbeat();
		const stopped = { ...current, status: "cancelled" as const, nextRunAt: undefined };
		const patches: Array<{ heartbeat: AgentCronJob | null }> = [];
		const harness = Object.create(InteractiveMode.prototype) as HeartbeatManagementHarness;
		harness.heartbeatCatalog = [{ job: current }];
		harness.connectionState = { activeSessionId: current.activeSessionId };
		harness.agentConnection = {
			manageHeartbeat: vi.fn(async () => stopped),
		};
		harness.patchConnectionState = (patch) => patches.push(patch);
		harness.applyHeartbeatCatalog = vi.fn();
		harness.refreshHeartbeatCatalog = vi.fn(async () => {});

		await harness.manageHeartbeat({ job: current }, "stop");

		expect(patches).toEqual([{ heartbeat: null }]);
		expect(harness.applyHeartbeatCatalog).toHaveBeenCalledWith([]);
		expect(harness.refreshHeartbeatCatalog).toHaveBeenCalledOnce();
	});

	it("keeps a successful action successful when the catalog refresh fails", async () => {
		const current = heartbeat();
		const paused = { ...current, status: "paused" as const, nextRunAt: undefined };
		const harness = Object.create(InteractiveMode.prototype) as HeartbeatManagementHarness;
		harness.heartbeatCatalog = [{ job: current, sessionName: "Primary session" }];
		harness.connectionState = { activeSessionId: current.activeSessionId };
		harness.agentConnection = { manageHeartbeat: vi.fn(async () => paused) };
		harness.patchConnectionState = vi.fn();
		harness.applyHeartbeatCatalog = vi.fn();
		harness.refreshHeartbeatCatalog = vi.fn(async () => {
			throw new Error("worker recovering");
		});

		await expect(harness.manageHeartbeat({ job: current, sessionName: "Primary session" }, "pause")).resolves.toBe(
			undefined,
		);

		expect(harness.applyHeartbeatCatalog).toHaveBeenCalledWith([{ job: paused, sessionName: "Primary session" }]);
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

	it("refreshes an open manager after the next scheduled run", async () => {
		vi.useFakeTimers();
		try {
			vi.setSystemTime(new Date("2026-01-01T00:00:00.000Z"));
			const harness = Object.create(InteractiveMode.prototype) as HeartbeatRefreshHarness;
			harness.heartbeatCatalog = [{ job: { ...heartbeat(), nextRunAt: "2026-01-01T00:00:01.000Z" } }];
			harness.connectionState = { activeSessionId: "active-1", sessionId: "session-1" };
			harness.subagentSnapshots = new Map();
			harness.heartbeatManager = {};
			harness.heartbeatManagerRefreshTimer = undefined;
			harness.refreshHeartbeatCatalog = vi.fn(async () => {});

			harness.scheduleHeartbeatManagerRefresh();
			await vi.advanceTimersByTimeAsync(1_250);

			expect(harness.refreshHeartbeatCatalog).toHaveBeenCalledOnce();
		} finally {
			vi.useRealTimers();
		}
	});

	it("keeps the overdue refresh deadline when subagent updates re-derive the schedule", async () => {
		vi.useFakeTimers();
		try {
			// nextRunAt (00:05) is already in the past, so the 5s overdue fallback applies.
			vi.setSystemTime(new Date("2026-01-01T00:10:00.000Z"));
			const harness = Object.create(InteractiveMode.prototype) as HeartbeatRefreshHarness;
			harness.heartbeatCatalog = [{ job: heartbeat() }];
			harness.connectionState = { activeSessionId: "active-1", sessionId: "session-1" };
			harness.subagentSnapshots = new Map();
			harness.heartbeatManager = {};
			harness.heartbeatManagerRefreshTimer = undefined;
			harness.refreshHeartbeatCatalog = vi.fn(async () => {});

			harness.scheduleHeartbeatManagerRefresh();
			// Subagent snapshot updates re-derive the schedule more often than
			// every 5s; they must not postpone the pending overdue refresh.
			for (let i = 0; i < 5; i++) {
				await vi.advanceTimersByTimeAsync(1_000);
				harness.scheduleHeartbeatManagerRefresh();
			}

			expect(harness.refreshHeartbeatCatalog).toHaveBeenCalledOnce();
		} finally {
			vi.useRealTimers();
		}
	});

	it("re-arms to an earlier deadline when a sooner heartbeat appears", async () => {
		vi.useFakeTimers();
		try {
			vi.setSystemTime(new Date("2026-01-01T00:00:00.000Z"));
			const harness = Object.create(InteractiveMode.prototype) as HeartbeatRefreshHarness;
			// Two minutes out, so the first schedule arms the capped 60s poll.
			harness.heartbeatCatalog = [{ job: { ...heartbeat(), nextRunAt: "2026-01-01T00:02:00.000Z" } }];
			harness.connectionState = { activeSessionId: "active-1", sessionId: "session-1" };
			harness.subagentSnapshots = new Map();
			harness.heartbeatManager = {};
			harness.heartbeatManagerRefreshTimer = undefined;
			harness.refreshHeartbeatCatalog = vi.fn(async () => {});

			harness.scheduleHeartbeatManagerRefresh();
			// A sooner heartbeat must pull the pending refresh forward, not sit
			// behind the already-armed 60s poll.
			harness.heartbeatCatalog = [{ job: { ...heartbeat(), nextRunAt: "2026-01-01T00:00:02.000Z" } }];
			harness.scheduleHeartbeatManagerRefresh();
			await vi.advanceTimersByTimeAsync(3_000);

			expect(harness.refreshHeartbeatCatalog).toHaveBeenCalledOnce();
		} finally {
			vi.useRealTimers();
		}
	});
});

interface HeartbeatManagerOpenHarness {
	heartbeatManager: HeartbeatManagerComponent | undefined;
	heartbeatManagerHandle: { focus(): void; hide(): void } | undefined;
	heartbeatCatalog: AgentConnectionHeartbeat[];
	heartbeatRefreshPromise: Promise<void> | undefined;
	heartbeatRefreshRequested: boolean;
	connectionState: { activeSessionId: string; sessionId: string };
	subagentSnapshots: Map<string, AgentConnectionRlmChildAgentSnapshot>;
	agentConnection: { listHeartbeats(): Promise<AgentConnectionHeartbeat[]> };
	isShuttingDown: boolean;
	isReturningToAgentsView: boolean;
	ui: {
		requestRender(): void;
		terminal: { rows: number };
		showOverlay(): { focus(): void; hide(): void };
	};
	scheduleHeartbeatManagerRefresh(): void;
	updateSubagentSummaryLine(): void;
	refreshHeartbeatCatalog(): Promise<void>;
	showHeartbeatManager(): void;
}

interface HeartbeatCommandHarness {
	defaultEditor: { onSubmit?: (text: string) => Promise<void> };
	editor: { getText(): string; setText(text: string): void };
	agentConnection: { prompt(text: string): Promise<unknown> };
	showError(message: string): void;
	heartbeatManagerHandle: { focus(): void; hide(): void } | undefined;
	heartbeatCatalog: AgentConnectionHeartbeat[];
	refreshHeartbeatCatalog(): Promise<void>;
	refreshHeartbeatCatalogCalled?: boolean;
	ui: {
		requestRender(): void;
		terminal: { rows: number };
		showOverlay(): { focus(): void; hide(): void };
	};
	[key: string]: unknown;
}

const setupEditorSubmitHandlerPrototype = (
	InteractiveMode.prototype as unknown as {
		setupEditorSubmitHandler(this: unknown): void;
	}
).setupEditorSubmitHandler;

describe("interactive heartbeat manager open (stale-while-revalidate)", () => {
	beforeAll(() => {
		initTheme("dark");
		setKeybindings(new KeybindingsManager());
	});

	const overlayHandle = { focus: vi.fn(), hide: vi.fn() };

	function makeOpenHarness(options?: { listHeartbeats?: () => Promise<AgentConnectionHeartbeat[]> }): {
		harness: HeartbeatManagerOpenHarness;
		requestRender: ReturnType<typeof vi.fn>;
	} {
		const requestRender = vi.fn();
		const harness = Object.create(InteractiveMode.prototype) as HeartbeatManagerOpenHarness;
		harness.heartbeatManager = undefined;
		harness.heartbeatManagerHandle = undefined;
		harness.heartbeatCatalog = [{ job: heartbeat() }];
		harness.heartbeatRefreshPromise = undefined;
		harness.heartbeatRefreshRequested = false;
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
		harness.agentConnection = {
			listHeartbeats: options?.listHeartbeats ?? vi.fn(() => new Promise<AgentConnectionHeartbeat[]>(() => {})),
		};
		harness.isShuttingDown = false;
		harness.isReturningToAgentsView = false;
		harness.ui = {
			requestRender,
			terminal: { rows: 24 },
			showOverlay: vi.fn(() => overlayHandle),
		};
		harness.scheduleHeartbeatManagerRefresh = vi.fn();
		harness.updateSubagentSummaryLine = vi.fn();
		return { harness, requestRender };
	}

	function renderedManager(manager: NonNullable<HeartbeatManagerOpenHarness["heartbeatManager"]>): string {
		return stripAnsi(manager.render(100).join("\n"));
	}

	it("opens immediately with the cached catalog while the fetch hangs, then updates when data lands", async () => {
		let resolveFetch: (heartbeats: AgentConnectionHeartbeat[]) => void = () => {};
		const listHeartbeats = vi.fn(
			() =>
				new Promise<AgentConnectionHeartbeat[]>((resolve) => {
					resolveFetch = resolve;
				}),
		);
		const { harness, requestRender } = makeOpenHarness({ listHeartbeats });

		harness.showHeartbeatManager();

		// The overlay must open synchronously with the cached catalog, before the fetch settles.
		expect(listHeartbeats).toHaveBeenCalledOnce();
		expect(harness.heartbeatManagerHandle).toBe(overlayHandle);
		expect(harness.heartbeatManager).toBeDefined();
		expect(renderedManager(harness.heartbeatManager!)).toContain("1 heartbeat.");

		// Input keeps working while the fetch is still in flight.
		harness.heartbeatManager!.handleInput("\x1b[B");
		expect(requestRender).toHaveBeenCalled();

		// Fresh data lands: the still-open view updates through the live catalog getter.
		resolveFetch([
			{ job: heartbeat() },
			{
				job: heartbeat({
					id: "heartbeat-2",
					activeSessionId: "active-2",
					sessionId: "session-2",
					sessionFile: "/tmp/session-2.jsonl",
					status: "paused",
					nextRunAt: undefined,
				}),
			},
		]);
		const refreshed = Promise.race([
			harness.heartbeatRefreshPromise ?? Promise.resolve(),
			new Promise<never>((_, reject) => setTimeout(() => reject(new Error("refresh deadline")), 1_000)),
		]);
		await expect(refreshed).resolves.toBeUndefined();
		expect(renderedManager(harness.heartbeatManager!)).toContain("2 heartbeats · 1 paused");
	});

	it("keeps the stale catalog usable when the fetch deadline expires", { timeout: 10_000 }, async () => {
		vi.useFakeTimers();
		try {
			const { harness } = makeOpenHarness({
				listHeartbeats: () => new Promise<AgentConnectionHeartbeat[]>(() => {}),
			});

			harness.showHeartbeatManager();
			expect(harness.heartbeatManager).toBeDefined();
			const refresh = harness.heartbeatRefreshPromise;
			expect(refresh).toBeDefined();

			await vi.advanceTimersByTimeAsync(HEARTBEAT_REFRESH_FETCH_TIMEOUT_MS + 10);

			// The deadline settles the refresh quietly: no rejection, no stale overwrite.
			await expect(refresh).resolves.toBeUndefined();
			expect(harness.heartbeatCatalog).toEqual([{ job: heartbeat() }]);
			expect(harness.updateSubagentSummaryLine).not.toHaveBeenCalled();
			expect(harness.heartbeatRefreshPromise).toBeUndefined();

			// The open view stays usable with the stale catalog.
			expect(renderedManager(harness.heartbeatManager!)).toContain("1 heartbeat.");
			harness.heartbeatManager!.handleInput("\x1b[B");
			expect(harness.ui.requestRender).toHaveBeenCalled();
		} finally {
			vi.useRealTimers();
		}
	});

	it("retries after the deadline when heartbeats_changed arrived mid-fetch", { timeout: 10_000 }, async () => {
		vi.useFakeTimers();
		try {
			const fresh: AgentConnectionHeartbeat[] = [
				{ job: heartbeat() },
				{
					job: heartbeat({
						id: "heartbeat-2",
						activeSessionId: "active-2",
						sessionId: "session-2",
						sessionFile: "/tmp/session-2.jsonl",
						status: "paused",
						nextRunAt: undefined,
					}),
				},
			];
			const listHeartbeats = vi.fn(() => new Promise<AgentConnectionHeartbeat[]>(() => {}));
			listHeartbeats.mockImplementationOnce(() => new Promise<AgentConnectionHeartbeat[]>(() => {}));
			listHeartbeats.mockImplementationOnce(() => Promise.resolve(fresh));
			const { harness } = makeOpenHarness({ listHeartbeats });

			const first = harness.refreshHeartbeatCatalog();
			// A heartbeats_changed event coalesces into the in-flight refresh.
			const joined = harness.refreshHeartbeatCatalog();
			await vi.advanceTimersByTimeAsync(HEARTBEAT_REFRESH_FETCH_TIMEOUT_MS + 10);
			await expect(first).resolves.toBeUndefined();
			await expect(joined).resolves.toBeUndefined();

			// The drained follow-up refresh fetches again and converges.
			await vi.advanceTimersByTimeAsync(10);
			expect(listHeartbeats).toHaveBeenCalledTimes(2);
			expect(harness.heartbeatCatalog).toEqual(fresh);
		} finally {
			vi.useRealTimers();
		}
	});
});

describe("interactive /heartbeats command", () => {
	beforeAll(() => {
		initTheme("dark");
		setKeybindings(new KeybindingsManager());
	});

	it("opens the manager without waiting for the catalog fetch", async () => {
		const overlayHandle = { focus: vi.fn(), hide: vi.fn() };
		let editorText = "";
		const refreshHeartbeatCatalog = vi.fn(() => new Promise<void>(() => {}));
		const context = Object.create(InteractiveMode.prototype) as HeartbeatCommandHarness;
		Object.assign(context, {
			defaultEditor: {} as { onSubmit?: (text: string) => Promise<void> },
			editor: {
				getText: () => editorText,
				setText: (text: string) => {
					editorText = text;
				},
			},
			agentConnection: { prompt: vi.fn(async () => undefined) },
			// Only settingsManager is read on the /heartbeats path (telemetry
			// capture is fire-and-forget), so a minimal uiServices suffices.
			uiServices: { settingsManager: {} },
			showError: vi.fn(),
			showStatus: vi.fn(),
			echoLocalCommand: vi.fn(),
			submittedInputBehavior: "steer",
			inputSubmissionGeneration: 0,
			inputSubmissionsPending: 0,
			pendingPromptStashReleases: [],
			promptStashState: {},
			pendingSubmittedPromptStash: undefined,
			snapshotPromptStash: vi.fn(() => ({ text: "" })),
			promptStash: undefined,
			promptStashSessionId: "session-1",
			sessionId: "session-1",
			clearShortcutGuide: vi.fn(),
			// Support for the real showHeartbeatManager: a fetch that never settles.
			heartbeatManager: undefined,
			heartbeatManagerHandle: undefined,
			heartbeatCatalog: [{ job: heartbeat() }],
			heartbeatRefreshPromise: undefined,
			heartbeatRefreshRequested: false,
			connectionState: { activeSessionId: "active-1", sessionId: "session-1" },
			subagentSnapshots: new Map(),
			refreshHeartbeatCatalog,
			ui: {
				requestRender: vi.fn(),
				terminal: { rows: 24 },
				showOverlay: vi.fn(() => overlayHandle),
			},
			scheduleHeartbeatManagerRefresh: vi.fn(),
			updateSubagentSummaryLine: vi.fn(),
		});

		setupEditorSubmitHandlerPrototype.call(context);
		const submitted = context.defaultEditor.onSubmit?.("/heartbeats");
		expect(submitted).toBeDefined();

		// The submit handler must settle without the fetch: bound the wait explicitly.
		const outcome = await Promise.race([
			submitted!.then(() => "settled" as const),
			new Promise<"deadline">((resolve) => setTimeout(() => resolve("deadline"), 1_000)),
		]);
		expect(outcome).toBe("settled");
		expect(context.heartbeatManagerHandle).toBe(overlayHandle);
		expect(refreshHeartbeatCatalog).toHaveBeenCalledOnce();
	});
});
