import { afterEach, describe, expect, it, vi } from "vitest";
import { AgentSession } from "../src/core/agent-session.js";
import { type AutonomousRuntimeState, createAutonomousRuntimeState } from "../src/core/autonomous.js";

type FakeAssistantMessage = { role: "assistant"; stopReason: string };

type FakeSession = {
	_autonomousState: AutonomousRuntimeState;
	_autonomousContinuationAwaitsRlmWork: boolean;
	_autonomousSubagentKeepAliveTimer: ReturnType<typeof setTimeout> | undefined;
	_disposed: boolean;
	_disposing: boolean;
	_sessionInputAdmissionPauses: Set<symbol>;
	_sessionInputPumpSuspended: boolean;
	_sessionInputArrivalEpoch: number;
	_cwd: string | undefined;
	queuedActionCount: number;
	_goalState: { status: string; objective?: string };
	_goalAccountingStartedAt: number | undefined;
	_getGoalContinuationMessages: () => Promise<unknown[]>;
	_autonomousContinuationSuppressionDepth: number;
	_autonomousContinuationSuppressedMessages: WeakSet<object>;
	_hasUnsettledRlmQuiescenceWork: () => boolean;
	_admitSessionInput: ReturnType<typeof vi.fn>;
	_createPreparedTurnAction: ReturnType<typeof vi.fn>;
	_snapshotAutonomousRuntimeState: () => unknown;
	_restoreAutonomousRuntimeSnapshot: (snapshot: unknown) => void;
};

const getContinuationMessages = Reflect.get(AgentSession.prototype, "_getContinuationMessages") as (
	this: FakeSession,
	context: { message: FakeAssistantMessage; newMessages: unknown[] },
	signal?: AbortSignal,
) => Promise<unknown[]>;
const holdForRlmWork = Reflect.get(AgentSession.prototype, "_holdAutonomousContinuationForRlmWork") as (
	this: FakeSession,
	message: FakeAssistantMessage,
) => boolean;
const maybeResume = Reflect.get(AgentSession.prototype, "_maybeResumeAutonomousContinuationAfterRlmWork") as (
	this: FakeSession,
) => void;
const fireKeepAlive = Reflect.get(AgentSession.prototype, "_fireAutonomousSubagentKeepAlive") as (
	this: FakeSession,
) => void;
const queueThresholdContinuation = Reflect.get(
	AgentSession.prototype,
	"_queueAutonomousContinuationForThresholdCompaction",
) as (this: FakeSession, message: FakeAssistantMessage) => Promise<unknown>;
const snapshotRuntimeState = Reflect.get(AgentSession.prototype, "_snapshotAutonomousRuntimeState") as (
	this: FakeSession,
) => unknown;
const restoreRuntimeSnapshot = Reflect.get(AgentSession.prototype, "_restoreAutonomousRuntimeSnapshot") as (
	this: FakeSession,
	snapshot: unknown,
) => void;

function fakeSession(overrides: Partial<FakeSession> = {}): FakeSession {
	const session: FakeSession = {
		_autonomousState: createAutonomousRuntimeState({ enabled: true, maxContinuations: 5 }),
		_autonomousContinuationAwaitsRlmWork: false,
		_autonomousSubagentKeepAliveTimer: undefined,
		_disposed: false,
		_disposing: false,
		_sessionInputAdmissionPauses: new Set(),
		_sessionInputPumpSuspended: false,
		_sessionInputArrivalEpoch: 0,
		_cwd: undefined,
		queuedActionCount: 0,
		_goalState: { status: "idle" },
		_goalAccountingStartedAt: undefined,
		_getGoalContinuationMessages: async () => [],
		_autonomousContinuationSuppressionDepth: 0,
		_autonomousContinuationSuppressedMessages: new WeakSet(),
		_hasUnsettledRlmQuiescenceWork: () => false,
		_admitSessionInput: vi.fn(),
		_createPreparedTurnAction: vi.fn(
			(schedule: string, _text: string, _images: unknown, options: Record<string, unknown>) => ({
				schedule,
				options,
			}),
		),
		_snapshotAutonomousRuntimeState: () => undefined,
		_restoreAutonomousRuntimeSnapshot: () => undefined,
		...overrides,
	};
	// The gate, resume, and keep-alive methods run for real: assign the
	// prototype functions as own properties so internal `this.` calls land on
	// the fake session.
	const realMethods = [
		"_holdAutonomousContinuationForRlmWork",
		"_maybeResumeAutonomousContinuationAfterRlmWork",
		"_deliverOwedAutonomousContinuation",
		"_armAutonomousSubagentKeepAlive",
		"_disarmAutonomousSubagentKeepAlive",
		"_clearAutonomousContinuationAwait",
		"_fireAutonomousSubagentKeepAlive",
	] as const;
	for (const method of realMethods) {
		(session as unknown as Record<string, unknown>)[method] = Reflect.get(AgentSession.prototype, method);
	}
	// Real prototype helpers read the live state object, so keep the snapshot
	// pair bound to the fake session.
	session._snapshotAutonomousRuntimeState = () => snapshotRuntimeState.call(session);
	session._restoreAutonomousRuntimeSnapshot = (snapshot: unknown) => restoreRuntimeSnapshot.call(session, snapshot);
	return session;
}

const stoppedTurn = { role: "assistant", stopReason: "stop" } as FakeAssistantMessage;
const context = { message: stoppedTurn, newMessages: [] };

describe("autonomous continuation vs active subagents", () => {
	afterEach(() => {
		vi.useRealTimers();
	});

	it("schedules a continuation and counts it when no descendant work is pending", async () => {
		const session = fakeSession();
		const messages = await getContinuationMessages.call(session, context);
		expect(messages).toHaveLength(1);
		expect(session._autonomousState.continuationsUsed).toBe(1);
		expect(session._autonomousContinuationAwaitsRlmWork).toBe(false);
	});

	it("holds the timer-driven continuation while a child runs without spending budget", async () => {
		vi.useFakeTimers();
		const session = fakeSession({
			_hasUnsettledRlmQuiescenceWork: () => true,
		});
		const messages = await getContinuationMessages.call(session, context);
		expect(messages).toEqual([]);
		expect(session._autonomousContinuationAwaitsRlmWork).toBe(true);
		expect(session._autonomousState.continuationsUsed).toBe(0);
		expect(vi.getTimerCount()).toBe(1);
	});

	it("holds without queueing behind an active goal continuation", () => {
		vi.useFakeTimers();
		const session = fakeSession({
			_hasUnsettledRlmQuiescenceWork: () => true,
		});
		session._goalState = { status: "active", objective: "ship it" };
		expect(holdForRlmWork.call(session, stoppedTurn)).toBe(true);
		expect(session._autonomousContinuationAwaitsRlmWork).toBe(false);
		expect(vi.getTimerCount()).toBe(0);
		expect(session._autonomousState.continuationsUsed).toBe(0);
	});

	it("does not hold the continuation for aborted or errored turns", async () => {
		const session = fakeSession({ _hasUnsettledRlmQuiescenceWork: () => true });
		for (const stopReason of ["aborted", "error"]) {
			const held = holdForRlmWork.call(session, { role: "assistant", stopReason });
			expect(held).toBe(false);
		}
		expect(session._autonomousContinuationAwaitsRlmWork).toBe(false);
		await expect(
			getContinuationMessages.call(session, {
				message: { role: "assistant", stopReason: "aborted" },
				newMessages: [],
			}),
		).resolves.toEqual([]);
		expect(session._autonomousState.continuationsUsed).toBe(0);
		expect(session._autonomousContinuationAwaitsRlmWork).toBe(false);
	});

	it("resumes the held continuation exactly once, unqueued, idle-waking, and counted", () => {
		const session = fakeSession({ _autonomousContinuationAwaitsRlmWork: true });
		maybeResume.call(session);
		maybeResume.call(session);
		expect(session._admitSessionInput).toHaveBeenCalledTimes(1);
		const [action] = session._admitSessionInput.mock.calls[0]!;
		expect((action as { options: { resumeIfIdle: boolean } }).options.resumeIfIdle).toBe(true);
		expect(session._autonomousState.continuationsUsed).toBe(1);
		expect(session._autonomousContinuationAwaitsRlmWork).toBe(false);
	});

	it("keeps the deferral while descendant work remains", () => {
		const session = fakeSession({
			_autonomousContinuationAwaitsRlmWork: true,
			_hasUnsettledRlmQuiescenceWork: () => true,
		});
		maybeResume.call(session);
		expect(session._admitSessionInput).not.toHaveBeenCalled();
		expect(session._autonomousContinuationAwaitsRlmWork).toBe(true);
		expect(session._autonomousState.continuationsUsed).toBe(0);
	});

	it("keeps the deferral while admission is paused and retries after release", () => {
		const session = fakeSession({ _autonomousContinuationAwaitsRlmWork: true });
		session._sessionInputAdmissionPauses.add(Symbol("pause"));
		maybeResume.call(session);
		expect(session._admitSessionInput).not.toHaveBeenCalled();
		expect(session._autonomousContinuationAwaitsRlmWork).toBe(true);

		session._sessionInputAdmissionPauses.clear();
		maybeResume.call(session);
		expect(session._admitSessionInput).toHaveBeenCalledTimes(1);
		expect(session._autonomousContinuationAwaitsRlmWork).toBe(false);
	});

	it("keeps the deferral while the pump is suspended after an abort", () => {
		const session = fakeSession({
			_autonomousContinuationAwaitsRlmWork: true,
			_sessionInputPumpSuspended: true,
		});
		maybeResume.call(session);
		expect(session._admitSessionInput).not.toHaveBeenCalled();
		expect(session._autonomousContinuationAwaitsRlmWork).toBe(true);
	});

	it("keeps the deferral and rolls back the count when admission throws", () => {
		const session = fakeSession({
			_autonomousContinuationAwaitsRlmWork: true,
			_admitSessionInput: vi.fn(() => {
				throw new Error("admission race");
			}),
		});
		maybeResume.call(session);
		expect(session._autonomousContinuationAwaitsRlmWork).toBe(true);
		expect(session._autonomousState.continuationsUsed).toBe(0);
	});

	it("drops the held continuation when autonomous mode is disabled", () => {
		const session = fakeSession({ _autonomousContinuationAwaitsRlmWork: true });
		session._autonomousState.enabled = false;
		maybeResume.call(session);
		expect(session._admitSessionInput).not.toHaveBeenCalled();
		expect(session._autonomousContinuationAwaitsRlmWork).toBe(false);
	});

	it("drops the held continuation when limits are already reached", () => {
		const session = fakeSession({
			_autonomousContinuationAwaitsRlmWork: true,
			_autonomousState: createAutonomousRuntimeState({ enabled: true, maxContinuations: 1 }),
		});
		session._autonomousState.continuationsUsed = 1;
		maybeResume.call(session);
		expect(session._admitSessionInput).not.toHaveBeenCalled();
		expect(session._autonomousContinuationAwaitsRlmWork).toBe(false);
		expect(session._autonomousState.continuationsUsed).toBe(1);
	});

	it("fires one keep-alive continuation per window while children stay active", () => {
		vi.useFakeTimers();
		const session = fakeSession({
			_hasUnsettledRlmQuiescenceWork: () => true,
			_autonomousState: createAutonomousRuntimeState({ enabled: true, subagentKeepAliveMs: 1_000 }),
		});
		expect(holdForRlmWork.call(session, stoppedTurn)).toBe(true);
		vi.advanceTimersByTime(1_000);
		expect(session._admitSessionInput).toHaveBeenCalledTimes(1);
		const [action] = session._admitSessionInput.mock.calls[0]!;
		expect((action as { options: { resumeIfIdle: boolean } }).options.resumeIfIdle).toBe(true);
		expect(session._autonomousState.continuationsUsed).toBe(1);
		// Children are still active: the next turn end re-holds and re-arms.
		expect(session._autonomousContinuationAwaitsRlmWork).toBe(false);
		expect(holdForRlmWork.call(session, stoppedTurn)).toBe(true);
		vi.advanceTimersByTime(999);
		expect(session._admitSessionInput).toHaveBeenCalledTimes(1);
		vi.advanceTimersByTime(1);
		expect(session._admitSessionInput).toHaveBeenCalledTimes(2);
		expect(session._autonomousState.continuationsUsed).toBe(2);
	});

	it("delivers the plain owed continuation, not a keep-alive, when children settle first", () => {
		vi.useFakeTimers();
		const session = fakeSession({
			_hasUnsettledRlmQuiescenceWork: () => true,
			_autonomousState: createAutonomousRuntimeState({ enabled: true, subagentKeepAliveMs: 1_000 }),
		});
		expect(holdForRlmWork.call(session, stoppedTurn)).toBe(true);
		session._hasUnsettledRlmQuiescenceWork = () => false;
		fireKeepAlive.call(session);
		const [action] = session._admitSessionInput.mock.calls[0]!;
		const message = (action as { options: { message: { content: Array<{ text: string }> } } }).options.message;
		expect(message.content[0]!.text).toContain("[autonomous-continuation]");
		expect(message.content[0]!.text).not.toContain("subagent-keep-alive");
		expect(session._autonomousState.continuationsUsed).toBe(1);
	});

	it("never arms a keep-alive when the valve is disabled with 0", () => {
		vi.useFakeTimers();
		const session = fakeSession({
			_hasUnsettledRlmQuiescenceWork: () => true,
			_autonomousState: createAutonomousRuntimeState({ enabled: true, subagentKeepAliveMs: 0 }),
		});
		expect(holdForRlmWork.call(session, stoppedTurn)).toBe(true);
		expect(vi.getTimerCount()).toBe(0);
		expect(session._autonomousState.subagentKeepAliveMs).toBe(0);
	});

	it("retries the keep-alive after the window when admission is paused", () => {
		vi.useFakeTimers();
		const session = fakeSession({
			_hasUnsettledRlmQuiescenceWork: () => true,
			_autonomousState: createAutonomousRuntimeState({ enabled: true, subagentKeepAliveMs: 1_000 }),
		});
		expect(holdForRlmWork.call(session, stoppedTurn)).toBe(true);
		session._sessionInputAdmissionPauses.add(Symbol("pause"));
		vi.advanceTimersByTime(1_000);
		expect(session._admitSessionInput).not.toHaveBeenCalled();
		expect(vi.getTimerCount()).toBe(1);
		session._sessionInputAdmissionPauses.clear();
		vi.advanceTimersByTime(1_000);
		expect(session._admitSessionInput).toHaveBeenCalledTimes(1);
	});

	it("holds the threshold-compaction continuation while a child runs", async () => {
		const session = fakeSession({
			_hasUnsettledRlmQuiescenceWork: () => true,
		}) as FakeSession & {
			_queuedAutonomousThresholdContinuations: Map<unknown, unknown>;
			_postCompactionContinuationMessages: unknown[];
		};
		session._queuedAutonomousThresholdContinuations = new Map();
		session._postCompactionContinuationMessages = [];
		const queued = await queueThresholdContinuation.call(session, stoppedTurn);
		expect(queued).toBeUndefined();
		expect(session._autonomousContinuationAwaitsRlmWork).toBe(true);
		expect(session._autonomousState.continuationsUsed).toBe(0);
	});
});
