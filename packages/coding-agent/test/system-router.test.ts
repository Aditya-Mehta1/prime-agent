import type * as PiAi from "@earendil-works/pi-ai";
import type { AssistantMessage } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
	compileActionSpace,
	compileDecisionPrompt,
	createModelDecisionFunction,
	ESCALATE_ACTION,
	FINISH_ACTION,
	formatHistoryEntry,
	gateThreshold,
	observationDigest,
	parseDecision,
	parseEnvironmentActions,
	parseSystemRouterRunSpec,
	type RouterActionSpec,
	type RouterDecisionOutcome,
	type RouterEnvironment,
	type RouterObservation,
	runSystemRouterLoop,
	StdioRouterEnvironment,
	truncateObservation,
} from "../src/core/system-router/index.js";

const { completeSimpleMock } = vi.hoisted(() => ({
	completeSimpleMock: vi.fn(),
}));

vi.mock("@earendil-works/pi-ai", async (importOriginal) => {
	const actual = await importOriginal<typeof PiAi>();
	return {
		...actual,
		completeSimple: completeSimpleMock,
	};
});

const PRESS_ACTIONS: Record<string, RouterActionSpec> = {
	press_a: { description: "Press the A button." },
	read_menu: { description: "Read the menu.", risk: "read" },
	press_b: { description: "Press the B button.", risk: "destructive" },
	set_power: {
		description: "Set the power level.",
		risk: "write",
		params: { power: { choices: { low: "Slow but safe", high: "Fast but risky" } } },
	},
};

class FakeEnvironment implements RouterEnvironment {
	resetCalls = 0;
	closeCalls = 0;
	observeCalls = 0;
	executeLog: Array<[string, Record<string, string>]> = [];
	observations: RouterObservation[];
	executionResults: string[];
	terminalAfterExecute = Infinity;
	throwOnObserve: Error | undefined;
	throwOnExecute: Error | undefined;

	constructor(observations: string[], executionResults: string[] = []) {
		this.observations = observations.map((text) => ({ text }));
		this.executionResults = executionResults;
	}

	async reset(): Promise<void> {
		this.resetCalls += 1;
	}

	async observe(): Promise<RouterObservation> {
		this.observeCalls += 1;
		if (this.throwOnObserve) throw this.throwOnObserve;
		const observation = this.observations[Math.min(this.observeCalls - 1, this.observations.length - 1)];
		return { ...observation };
	}

	async execute(action: string, params: Record<string, string>): Promise<{ text: string }> {
		if (this.throwOnExecute) throw this.throwOnExecute;
		this.executeLog.push([action, params]);
		const executions = this.executeLog.length;
		const text = this.executionResults[Math.min(executions - 1, this.executionResults.length - 1)] ?? `${action} ok`;
		return { text, ...(executions >= this.terminalAfterExecute ? { terminal: true } : {}) };
	}

	async close(): Promise<void> {
		this.closeCalls += 1;
	}
}

function decision(outcome: Partial<RouterDecisionOutcome>): RouterDecisionOutcome {
	return { action: null, params: {}, confidence: null, rawText: "", ...outcome };
}

/** A decide function that replays outcomes; the last entry repeats. */
function scriptedDecide(outcomes: RouterDecisionOutcome[]) {
	let index = 0;
	return async () => {
		const outcome = outcomes[Math.min(index, outcomes.length - 1)];
		index += 1;
		return { ...outcome };
	};
}

interface LoopOverrides {
	decide?: (request: unknown) => Promise<RouterDecisionOutcome>;
	goal?: string;
	maxSteps?: number;
	timeoutMs?: number;
	gate?: { read?: number; write?: number; destructive?: number; finish?: number };
}

function runLoop(env: RouterEnvironment, overrides: LoopOverrides = {}) {
	const model = { id: "faux-fast", provider: "faux", input: [], thinkingLevel: "off" };
	return runSystemRouterLoop({
		env,
		goal: overrides.goal ?? "Finish the demo",
		actions: PRESS_ACTIONS,
		decide: overrides.decide ?? scriptedDecide([decision({ action: "press_a", confidence: 0.9 })]),
		model,
		...(overrides.gate ? { gate: overrides.gate } : {}),
		maxSteps: overrides.maxSteps ?? 10,
		timeoutMs: overrides.timeoutMs ?? 30_000,
	});
}

afterEach(() => {
	completeSimpleMock.mockReset();
});

describe("parseSystemRouterRunSpec", () => {
	const valid = {
		goal: "Play the game",
		environment: { stdio: { command: ["node", "adapter.mjs"] } },
	};

	it.each<[string, unknown, string | null]>([
		["minimal spec", valid, null],
		["with model and budgets", { ...valid, model: "internal/glm-5.3-fast", maxSteps: 5, timeoutMs: 1_000 }, null],
		["with actions", { ...valid, actions: { press_a: { description: "Press A." } } }, null],
		[
			"with env init payload",
			{ ...valid, environment: { stdio: { command: ["node", "a.mjs"], init: { romPath: "/x" } } } },
			null,
		],
		["non-object payload", null, "payload must be an object"],
		["empty goal", { ...valid, goal: " " }, "goal must be a non-empty string"],
		["missing environment", { goal: "g" }, "environment must be an object with a stdio adapter"],
		[
			"empty command",
			{ ...valid, environment: { stdio: { command: [] } } },
			"command must be a non-empty string array",
		],
		[
			"non-string command entry",
			{ ...valid, environment: { stdio: { command: ["node", 3] } } },
			"command must be a non-empty string array",
		],
		["empty actions object", { ...valid, actions: {} }, "actions must be a non-empty object"],
		[
			"non-snake-case action name",
			{ ...valid, actions: { PressA: { description: "x" } } },
			"must be lowercase snake_case",
		],
		[
			"reserved action name",
			{ ...valid, actions: { finish: { description: "x" } } },
			"is reserved for the loop itself",
		],
		[
			"reserved escalate name",
			{ ...valid, actions: { escalate: { description: "x" } } },
			"is reserved for the loop itself",
		],
		["missing action description", { ...valid, actions: { press_a: {} } }, "description must be a non-empty string"],
		[
			"bad risk",
			{ ...valid, actions: { press_a: { description: "x", risk: "extreme" } } },
			'must be "read", "write", or "destructive"',
		],
		[
			"non-finite param choices",
			{ ...valid, actions: { press_a: { description: "x", params: { p: {} } } } },
			"must declare a non-empty finite choices set",
		],
		[
			"empty param choice value",
			{ ...valid, actions: { press_a: { description: "x", params: { p: { choices: { "": "d" } } } } } },
			"has an empty choice value",
		],
		[
			"param choice missing description",
			{ ...valid, actions: { press_a: { description: "x", params: { p: { choices: { v: "" } } } } } },
			"needs a non-empty description",
		],
		["gate out of range", { ...valid, gate: { read: 1.5 } }, "gate.read must be a number in [0, 1]"],
		["maxSteps zero", { ...valid, maxSteps: 0 }, "maxSteps must be a whole number in [1, 200]"],
		["maxSteps over cap", { ...valid, maxSteps: 201 }, "maxSteps must be a whole number in [1, 200]"],
		["timeoutMs over cap", { ...valid, timeoutMs: 600_001 }, "timeoutMs must be a whole number in [1, 600000]"],
		["historySteps over cap", { ...valid, historySteps: 33 }, "historySteps must be a whole number in [1, 32]"],
		[
			"observationChars zero",
			{ ...valid, observationChars: 0 },
			"observationChars must be a whole number in [1, 32000]",
		],
		[
			"fractional maxSteps floors to zero",
			{ ...valid, maxSteps: 0.5 },
			"maxSteps must be a whole number in [1, 200]",
		],
		[
			"fractional timeoutMs floors to zero",
			{ ...valid, timeoutMs: 0.5 },
			"timeoutMs must be a whole number in [1, 600000]",
		],
		[
			"fractional historySteps floors to zero",
			{ ...valid, historySteps: 0.5 },
			"historySteps must be a whole number in [1, 32]",
		],
	])("%s %s", (_label, payload, errorFragment) => {
		if (errorFragment === null) {
			expect(() => parseSystemRouterRunSpec(payload)).not.toThrow();
		} else {
			expect(() => parseSystemRouterRunSpec(payload)).toThrow(errorFragment);
		}
	});

	it("applies default budgets", () => {
		const spec = parseSystemRouterRunSpec(valid);
		expect(spec.maxSteps).toBe(25);
		expect(spec.timeoutMs).toBe(120_000);
		expect(spec.historySteps).toBe(8);
		expect(spec.observationChars).toBe(6_000);
		expect(spec.environment.stdio.requestTimeoutMs).toBe(30_000);
	});
});

describe("parseDecision", () => {
	const { byName } = compileActionSpace(PRESS_ACTIONS);

	it.each<[string, string, Partial<RouterDecisionOutcome>]>([
		["valid action", '{"action":"press_a","params":{},"confidence":0.8}', { action: "press_a", confidence: 0.8 }],
		["finish", '{"action":"finish","confidence":0.9}', { action: FINISH_ACTION, confidence: 0.9 }],
		["escalate", '{"action":"escalate","confidence":0.4}', { action: ESCALATE_ACTION, confidence: 0.4 }],
		[
			"param in choices",
			'{"action":"set_power","params":{"power":"low"},"confidence":0.7}',
			{ action: "set_power", params: { power: "low" }, confidence: 0.7 },
		],
		["fenced json", '```json\n{"action":"press_b","confidence":0.6}\n```', { action: "press_b", confidence: 0.6 }],
		["prose-wrapped json", 'Sure! {"action":"press_a","confidence":1} done', { action: "press_a", confidence: 1 }],
		["free text", "press A please", { action: null, parseError: "reply was not a JSON object" }],
		["array not object", '["press_a"]', { action: null, parseError: "reply was not a JSON object" }],
		[
			"unknown action",
			'{"action":"press_x","confidence":0.9}',
			{ action: null, parseError: 'unknown action "press_x"' },
		],
		["missing action", '{"confidence":0.9}', { action: null, parseError: "unknown action null" }],
		[
			"unknown param",
			'{"action":"press_a","params":{"other":"x"},"confidence":0.9}',
			{ action: null, parseError: 'unknown param "other" for action "press_a"' },
		],
		[
			"param value not in choices",
			'{"action":"set_power","params":{"power":"turbo"},"confidence":0.9}',
			{ action: null, parseError: 'param "power" value must be one of its declared choices' },
		],
		[
			"params not object",
			'{"action":"press_a","params":"x","confidence":0.9}',
			{ action: null, parseError: "params must be an object" },
		],
		[
			"missing declared param",
			'{"action":"set_power","confidence":0.9}',
			{ action: null, parseError: 'missing param(s) power for action "set_power"' },
		],
		[
			"valid json followed by braced prose",
			'{"action":"press_a","confidence":0.9} (note: use {"action":"finish"} for done)',
			{ action: "press_a", confidence: 0.9 },
		],
		["stray trailing brace", '{"action":"press_b","confidence":0.7}\n}', { action: "press_b", confidence: 0.7 }],
		[
			"missing confidence",
			'{"action":"press_a"}',
			{ action: null, parseError: "confidence must be a number in [0, 1]" },
		],
		[
			"confidence too high",
			'{"action":"press_a","confidence":1.2}',
			{ action: null, parseError: "confidence must be a number in [0, 1]" },
		],
		[
			"confidence not number",
			'{"action":"press_a","confidence":"high"}',
			{ action: null, parseError: "confidence must be a number in [0, 1]" },
		],
	])("%s", (_label, raw, expected) => {
		const outcome = parseDecision(raw, byName);
		expect({
			action: outcome.action,
			params: outcome.params,
			confidence: outcome.confidence,
			parseError: outcome.parseError,
		}).toMatchObject(expected);
	});
});

describe("action space compilation", () => {
	const { byName } = compileActionSpace(PRESS_ACTIONS);

	it("appends finish and escalate to the declared space", () => {
		expect(byName.has(FINISH_ACTION)).toBe(true);
		expect(byName.has(ESCALATE_ACTION)).toBe(true);
		expect(byName.get("press_b")?.risk).toBe("destructive");
		expect(byName.get("press_a")?.risk).toBe("write");
	});

	it.each<[string, string, number, Record<string, number>]>([
		["write default", "press_a", 0.6, {}],
		["read default", "read_menu", 0.5, {}],
		["write gate override", "press_a", 0.9, { write: 0.9 }],
		["destructive default", "press_b", 0.8, {}],
		["finish default", FINISH_ACTION, 0.5, {}],
		["finish override", FINISH_ACTION, 0.7, { finish: 0.7 }],
	])("gate threshold: %s", (_label, action, expected, gate) => {
		const compiled = byName.get(action);
		if (!compiled) throw new Error("missing action");
		expect(gateThreshold(gate, compiled)).toBe(expected);
	});

	it("compileDecisionPrompt carries goal, observation, fields, actions, and format", () => {
		const prompt = compileDecisionPrompt({
			goal: "Leave the house",
			observation: { text: "Mom is talking.", fields: { ram: 12 } },
			history: ["press_a() -> screen advanced"],
			actions: byName,
			observationChars: 6_000,
		});
		expect(prompt).toContain("Leave the house");
		expect(prompt).toContain("Mom is talking.");
		expect(prompt).toContain("ram: 12");
		expect(prompt).toContain("- press_a [risk=write]: Press the A button.");
		expect(prompt).toContain('param "power"');
		expect(prompt).toContain('"low" (Slow but safe)');
		expect(prompt).toContain("- finish [risk=read]");
		expect(prompt).toContain("- escalate [risk=read]");
		expect(prompt).toContain("press_a() -> screen advanced");
		expect(prompt).toContain('"action"');
		expect(prompt).toContain('"confidence"');
	});

	it("truncates the observation to the budget", () => {
		const long = "x".repeat(100);
		expect(truncateObservation(long, 50).length).toBeLessThanOrEqual(50);
		expect(truncateObservation(long, 50)).toContain("<observation truncated>");
		expect(truncateObservation("short", 50)).toBe("short");
	});

	it("observation digest changes with content and history lines stay bounded", () => {
		expect(observationDigest({ text: "a" })).not.toBe(observationDigest({ text: "b" }));
		expect(observationDigest({ text: "a" })).toBe(observationDigest({ text: "a" }));
		expect(formatHistoryEntry("press_a", {}, "r".repeat(300))).toBe(`press_a() -> ${"r".repeat(157)}...`);
		expect(formatHistoryEntry("set_power", { power: "low" }, "ok")).toBe('set_power(power="low") -> ok');
	});
});

describe("runSystemRouterLoop", () => {
	it("executes gated decisions and stops on environment terminal state", async () => {
		const env = new FakeEnvironment(["intro screen", "overworld"]);
		env.terminalAfterExecute = 2;
		const result = await runLoop(env);
		expect(result.status).toBe("done");
		expect(result.reason).toBe("environment_terminal");
		expect(result.executed).toBe(2);
		expect(result.refused).toBe(0);
		expect(env.resetCalls).toBe(1);
		expect(env.closeCalls).toBe(1);
		expect(env.executeLog).toEqual([
			["press_a", {}],
			["press_a", {}],
		]);
	});

	it("records a complete trace entry per step and sums usage", async () => {
		const env = new FakeEnvironment(["screen 1", "screen 2"]);
		const decide = scriptedDecide([
			decision({ action: "press_a", confidence: 0.9, usage: { inputTokens: 10, outputTokens: 2 } }),
			decision({ action: FINISH_ACTION, confidence: 0.95, usage: { inputTokens: 11, outputTokens: 3 } }),
		]);
		const result = await runLoop(env, { decide });
		expect(result.status).toBe("done");
		expect(result.reason).toBe("goal_reached");
		expect(result.usage).toEqual({ inputTokens: 21, outputTokens: 5 });
		expect(result.trace).toHaveLength(2);
		const step = result.trace[0];
		expect(step).toMatchObject({
			action: "press_a",
			params: {},
			confidence: 0.9,
			gate: { threshold: 0.6, verdict: "pass" },
			observationDigest: observationDigest({ text: "screen 1" }),
		});
		expect(typeof step.latencyMs).toBe("number");
		expect(result.trace[1].terminal).toBe(true);
		expect(result.summary).toContain("step 1");
	});

	it("passes params through to the environment", async () => {
		const env = new FakeEnvironment(["screen"]);
		const decide = scriptedDecide([decision({ action: "set_power", params: { power: "low" }, confidence: 0.9 })]);
		await runLoop(env, { decide });
		expect(env.executeLog).toEqual([["set_power", { power: "low" }]]);
	});

	it("refuses below the gate, resets the streak on a pass, and never executes refused actions", async () => {
		const env = new FakeEnvironment(["screen 1", "screen 2", "screen 3"]);
		const decide = scriptedDecide([
			decision({ action: "press_b", confidence: 0.5 }), // below destructive 0.8
			decision({ action: "press_a", confidence: 0.7 }), // above write 0.6
			decision({ action: FINISH_ACTION, confidence: 1 }),
		]);
		const result = await runLoop(env, { decide });
		expect(result.status).toBe("done");
		expect(result.refused).toBe(1);
		expect(result.executed).toBe(1);
		expect(env.executeLog).toEqual([["press_a", {}]]);
		const refused = result.trace[0];
		expect(refused.gate.verdict).toBe("refused");
		expect(refused.action).toBe("press_b");
		expect(refused.result).toContain("below destructive gate 0.80");
	});

	it("stops as stuck after the refusal streak and escalates when asked", async () => {
		const stuckEnv = new FakeEnvironment(["screen"]);
		const stuck = await runLoop(stuckEnv, {
			decide: scriptedDecide([decision({ action: "press_b", confidence: 0.5 })]),
		});
		expect(stuck.status).toBe("stuck");
		expect(stuck.reason).toBe("no_confident_decision");
		expect(stuck.executed).toBe(0);

		const escalEnv = new FakeEnvironment(["screen"]);
		const escalated = await runLoop(escalEnv, {
			decide: scriptedDecide([decision({ action: ESCALATE_ACTION, confidence: 0.3 })]),
		});
		expect(escalated.status).toBe("escalated");
		expect(escalated.reason).toBe("escalation_requested");
	});

	it("counts parse failures in the refusal streak without executing", async () => {
		const env = new FakeEnvironment(["screen 1", "screen 2", "screen 3"]);
		const decide = scriptedDecide([decision({ parseError: "reply was not a JSON object" })]);
		const result = await runLoop(env, { decide });
		expect(result.status).toBe("stuck");
		expect(result.reason).toBe("no_confident_decision");
		expect(env.executeLog).toEqual([]);
		expect(result.trace[0].gate.verdict).toBe("parse_failure");
		expect(result.trace[0].result).toContain("refused:");
	});

	it("stops as stuck when the same action repeats on the same observation", async () => {
		const env = new FakeEnvironment(["same screen", "same screen", "same screen"]);
		const decide = scriptedDecide([decision({ action: "press_a", confidence: 0.9 })]);
		const result = await runLoop(env, { decide });
		expect(result.status).toBe("stuck");
		expect(result.reason).toBe("repeated_state");
		expect(env.executeLog).toHaveLength(1);
		// The repetition stop is a refusal by the loop, so steps stays consistent.
		expect(result.refused).toBe(1);
		expect(result.steps).toBe(result.executed + result.refused);
	});

	it("fails the run when the environment cannot reset", async () => {
		const env = {
			reset: () => Promise.reject(new Error("no savestate")),
			observe: () => Promise.resolve({ text: "x" }),
			execute: () => Promise.resolve({ text: "y" }),
			close: () => Promise.resolve(),
		};
		const result = await runLoop(env as unknown as RouterEnvironment, {});
		expect(result.status).toBe("failed");
		expect(result.reason).toBe("environment_error");
		expect(result.summary).toContain("no savestate");
	});

	it("parseEnvironmentActions prefers the declared space and validates supplied ones", () => {
		const declared = { press_a: { description: "Declared press." } };
		expect(parseEnvironmentActions(declared, { press_b: { description: "Supplied." } })).toEqual({
			press_a: { description: "Declared press.", risk: "write" },
		});
		const supplied = { wait: { description: "Wait a bit." } };
		expect(parseEnvironmentActions(undefined, supplied)).toEqual({
			wait: { description: "Wait a bit.", risk: "write" },
		});
		expect(parseEnvironmentActions(undefined, undefined)).toBeUndefined();
		expect(() => parseEnvironmentActions(undefined, { finish: { description: "Nope." } })).toThrow(
			"is reserved for the loop itself",
		);
	});

	it("allows the same action twice when the observation moves", async () => {
		const env = new FakeEnvironment(["screen a", "screen b", "screen c"]);
		const decide = scriptedDecide([decision({ action: "press_a", confidence: 0.9 })]);
		env.terminalAfterExecute = 2;
		const result = await runLoop(env, { decide });
		expect(result.status).toBe("done");
		expect(env.executeLog).toHaveLength(2);
	});

	it("ends incomplete at the step budget", async () => {
		const env = new FakeEnvironment(["screen a", "screen b", "screen c"]);
		const result = await runLoop(env, {
			maxSteps: 2,
			decide: scriptedDecide([decision({ action: "press_a", confidence: 0.9 })]),
		});
		expect(result.status).toBe("incomplete");
		expect(result.reason).toBe("max_steps");
		expect(result.executed).toBe(2);
	});

	it("fails on environment observe errors, execute errors, and decision model errors", async () => {
		const observeEnv = new FakeEnvironment(["x"]);
		observeEnv.throwOnObserve = new Error("bus detached");
		const observeFailure = await runLoop(observeEnv);
		expect(observeFailure.status).toBe("failed");
		expect(observeFailure.reason).toBe("environment_error");
		expect(observeFailure.summary).toContain("bus detached");

		const executeEnv = new FakeEnvironment(["x"]);
		executeEnv.throwOnExecute = new Error("button jammed");
		const executeFailure = await runLoop(executeEnv);
		expect(executeFailure.status).toBe("failed");
		expect(executeFailure.reason).toBe("environment_error");
		expect(executeFailure.summary).toContain("button jammed");

		const crashEnv = new FakeEnvironment(["x"]);
		const crash = await runLoop(crashEnv, {
			decide: async () => {
				throw new Error("provider exploded");
			},
		});
		expect(crash.status).toBe("failed");
		expect(crash.reason).toBe("decision_model_error");

		const modelEnv = new FakeEnvironment(["x"]);
		const modelFailure = await runLoop(modelEnv, {
			decide: scriptedDecide([decision({ modelError: "decision model failed: 500" })]),
		});
		expect(modelFailure.status).toBe("failed");
		expect(modelFailure.reason).toBe("decision_model_error");
		expect(modelFailure.summary).toContain("500");
	});

	it("reports aborted when the signal fires before the first step", async () => {
		const controller = new AbortController();
		controller.abort();
		const aborted = await runSystemRouterLoop({
			env: new FakeEnvironment(["x"]),
			goal: "g",
			actions: PRESS_ACTIONS,
			decide: scriptedDecide([decision({ action: "press_a", confidence: 0.9 })]),
			model: { id: "m", provider: "p", input: [], thinkingLevel: "off" },
			maxSteps: 3,
			timeoutMs: 30_000,
			signal: controller.signal,
		});
		expect(aborted.status).toBe("failed");
		expect(aborted.reason).toBe("aborted");
	});

	it("closes the environment even when the loop fails", async () => {
		const env = new FakeEnvironment(["x"]);
		env.throwOnObserve = new Error("nope");
		await runLoop(env);
		expect(env.closeCalls).toBe(1);
	});

	it("rejects an empty action space", async () => {
		const env = new FakeEnvironment(["x"]);
		await expect(
			runSystemRouterLoop({
				env,
				goal: "g",
				actions: {},
				decide: scriptedDecide([decision({ action: "press_a", confidence: 0.9 })]),
				model: { id: "m", provider: "p", input: [], thinkingLevel: "off" },
				maxSteps: 3,
				timeoutMs: 30_000,
			}),
		).rejects.toThrow("action space is empty");
	});
});

describe("runSystemRouterLoop budgets (fake timers)", () => {
	beforeEach(() => {
		vi.useFakeTimers();
	});
	afterEach(() => {
		vi.useRealTimers();
	});

	it("ends incomplete when the deadline elapses mid-decision", async () => {
		const env = new FakeEnvironment(["screen"]);
		const never = new Promise<RouterDecisionOutcome>(() => {});
		// The never-settling promise never rejects, but keep it handled anyway.
		void never.catch(() => {});
		const run = runLoop(env, { decide: () => never, timeoutMs: 5_000 });
		// Attach the rejection handler before advancing any timer.
		run.catch(() => {});
		await vi.advanceTimersByTimeAsync(5_000);
		const result = await run;
		expect(result.status).toBe("incomplete");
		expect(result.reason).toBe("timeout");
		expect(result.summary).toContain("timeout");
		expect(env.closeCalls).toBe(1);
	});

	it("ends incomplete when the losing side rejects after the deadline fires", async () => {
		const env = new FakeEnvironment(["screen"]);
		// The decide call rejects 25ms after the 5ms deadline; its rejection must
		// stay handled (the raceDeadline invariant), not crash the host.
		let lateReject: ((error: Error) => void) | undefined;
		const decide: (request: unknown) => Promise<RouterDecisionOutcome> = () =>
			new Promise<RouterDecisionOutcome>((_, reject) => {
				lateReject = reject;
			});
		const run = runLoop(env, { decide, timeoutMs: 5 });
		run.catch(() => {});
		await vi.advanceTimersByTimeAsync(5);
		const result = await run;
		expect(result.status).toBe("incomplete");
		expect(result.reason).toBe("timeout");
		// The losing promise rejects only now; it was already handled by raceDeadline.
		lateReject?.(new Error("late rejection"));
		await vi.advanceTimersByTimeAsync(0);
	});

	it("ends incomplete when the deadline elapses mid-observe", async () => {
		const env = {
			reset: () => Promise.resolve(),
			observe: () => new Promise<RouterObservation>(() => {}),
			execute: () => Promise.resolve({ text: "" }),
			close: () => Promise.resolve(),
		};
		const run = runLoop(env as unknown as RouterEnvironment, { timeoutMs: 2_000 });
		run.catch(() => {});
		await vi.advanceTimersByTimeAsync(2_000);
		const result = await run;
		expect(result.status).toBe("incomplete");
		expect(result.reason).toBe("timeout");
	});
});

describe("createModelDecisionFunction", () => {
	const { byName } = compileActionSpace(PRESS_ACTIONS);
	const textModel = {
		id: "glm-fast",
		provider: "internal",
		api: "openai-completions",
		maxTokens: 4_096,
		input: ["text"],
		reasoning: false,
	} as unknown as PiAi.Model<PiAi.Api>;

	function assistant(text: string): AssistantMessage {
		return {
			role: "assistant",
			content: [{ type: "text", text }],
			stopReason: "stop",
			timestamp: Date.now(),
		} as AssistantMessage;
	}

	it("makes one call with thinking off, parses the choice, and reports usage", async () => {
		completeSimpleMock.mockResolvedValueOnce(assistant('{"action":"press_a","params":{},"confidence":0.75}'));
		const decide = createModelDecisionFunction({ model: textModel, actions: byName });
		const outcome = await decide({ prompt: "go" });
		expect(outcome.action).toBe("press_a");
		expect(outcome.confidence).toBe(0.75);
		expect(completeSimpleMock).toHaveBeenCalledTimes(1);
		const [_model, context, options] = completeSimpleMock.mock.calls[0];
		expect(options.reasoning).toBe("off");
		expect(options.maxTokens).toBeLessThanOrEqual(4_096);
		expect(context.systemPrompt).toContain("action model");
		expect(context.messages[0].content[0].text).toBe("go");
		expect(context.messages[0].content).toHaveLength(1);
	});

	it.each([
		[4_096, 4_096],
		[512, 512],
		[131_072, 4_096],
	])(
		"caps decision output at the model ceiling or 4096, whichever is smaller (maxTokens=%d)",
		async (modelMaxTokens, expectedCap) => {
			const model = { ...textModel, maxTokens: modelMaxTokens } as unknown as PiAi.Model<PiAi.Api>;
			completeSimpleMock.mockReset();
			completeSimpleMock.mockResolvedValueOnce(assistant('{"action":"press_a","confidence":0.6}'));
			const decide = createModelDecisionFunction({ model, actions: byName });
			const outcome = await decide({ prompt: "p" });
			expect(outcome.action).toBe("press_a");
			expect(completeSimpleMock.mock.calls[0][2].maxTokens).toBe(expectedCap);
		},
	);

	it("surfaces length and abort stop reasons as model errors, not parse refusals", async () => {
		const decide = createModelDecisionFunction({ model: textModel, actions: byName });
		completeSimpleMock.mockResolvedValueOnce({ ...assistant('{"action":"pre'), stopReason: "length" });
		const truncated = await decide({ prompt: "p" });
		expect(truncated.modelError).toContain("stopped early (length)");
		expect(truncated.action).toBeNull();
		completeSimpleMock.mockReset();
		completeSimpleMock.mockResolvedValueOnce({ ...assistant(""), stopReason: "aborted" });
		const aborted = await decide({ prompt: "p" });
		expect(aborted.modelError).toContain("stopped early (aborted)");
	});

	it("attaches the screenshot only for image-capable models", async () => {
		const imageModel = { ...textModel, input: ["text", "image"] } as unknown as PiAi.Model<PiAi.Api>;
		completeSimpleMock.mockResolvedValueOnce(assistant('{"action":"press_a","confidence":0.6}'));
		const textDecide = createModelDecisionFunction({ model: textModel, actions: byName });
		await textDecide({ prompt: "p", image: "aGk=" });
		expect(completeSimpleMock.mock.calls[0][1].messages[0].content).toHaveLength(1);
		completeSimpleMock.mockClear();
		completeSimpleMock.mockResolvedValueOnce(assistant('{"action":"press_a","confidence":0.6}'));
		const imageDecide = createModelDecisionFunction({ model: imageModel, actions: byName });
		await imageDecide({ prompt: "p", image: "aGk=" });
		expect(completeSimpleMock.mock.calls[0][1].messages[0].content).toHaveLength(2);
		expect(completeSimpleMock.mock.calls[0][1].messages[0].content[1]).toMatchObject({ type: "image" });
	});

	it("surfaces model errors as modelError and malformed replies as parseError", async () => {
		completeSimpleMock.mockResolvedValueOnce({
			...assistant(""),
			stopReason: "error",
			errorMessage: "boom 500",
		});
		const decide = createModelDecisionFunction({
			model: textModel,
			actions: byName,
			policy: { enabled: false, maxRetries: 0, baseDelayMs: 0, maxRetryDelayMs: 0 },
		});
		const modelError = await decide({ prompt: "p" });
		expect(modelError.modelError).toContain("boom 500");
		expect(modelError.action).toBeNull();
		completeSimpleMock.mockReset();
		completeSimpleMock.mockResolvedValueOnce(assistant("let me press the A button now"));
		const parseFailure = await decide({ prompt: "p" });
		expect(parseFailure.parseError).toBe("reply was not a JSON object");
	});
});

describe("StdioRouterEnvironment (real subprocess)", () => {
	const echoAdapter = (script: string) => [process.execPath, "-e", script];

	it("speaks init/reset/observe/execute and surfaces adapter errors", async () => {
		const script = `
			const lines = require("readline").createInterface({ input: process.stdin });
			const state = { ticks: 0 };
			lines.on("line", (line) => {
				const msg = JSON.parse(line);
				const reply = (payload) => process.stdout.write(JSON.stringify({ id: msg.id, ...payload }) + "\\n");
				if (msg.type === "init") reply({ ok: true, environment: { actions: { wait: { description: "Wait." } } } });
				else if (msg.type === "reset") { state.ticks = 0; reply({ ok: true }); }
				else if (msg.type === "observe") reply({ ok: true, observation: { text: "frame " + state.ticks, fields: { ticks: state.ticks } } });
				else if (msg.type === "execute") {
					if (msg.action === "wait") { state.ticks += 1; reply({ ok: true, text: "waited" }); }
					else reply({ ok: false, error: "unknown action: " + msg.action });
				} else if (msg.type === "close") { lines.close(); process.exit(0); }
			});
		`;
		const env = new StdioRouterEnvironment({ command: echoAdapter(script), requestTimeoutMs: 5_000 });
		try {
			const environment = await env.init();
			expect(environment?.actions).toMatchObject({ wait: { description: "Wait." } });
			await env.reset("goal");
			const first = await env.observe();
			expect(first.text).toBe("frame 0");
			expect(first.fields).toMatchObject({ ticks: 0 });
			await env.execute("wait", {});
			const second = await env.observe();
			expect(second.text).toBe("frame 1");
			await expect(env.execute("explode", {})).rejects.toThrow("unknown action: explode");
		} finally {
			await env.close();
		}
	});

	it("rejects init when the adapter exits early, with stderr context", async () => {
		const env = new StdioRouterEnvironment({
			command: [process.execPath, "-e", "console.error('adapter blew up'); process.exit(3);"],
			requestTimeoutMs: 5_000,
		});
		await expect(env.init()).rejects.toThrow(/exited early .*adapter blew up/);
		await env.close();
	});

	it("closes fast when the adapter already exited, and twice in a row", async () => {
		const env = new StdioRouterEnvironment({
			command: [process.execPath, "-e", "process.exit(0);"],
			requestTimeoutMs: 5_000,
		});
		await expect(env.init()).rejects.toThrow(/exited early/);
		const started = Date.now();
		await env.close();
		expect(Date.now() - started).toBeLessThan(500);
		await env.close();
	});

	it("tolerates a reply split across two stdout chunks and noise lines", async () => {
		const script = `
			const lines = require("readline").createInterface({ input: process.stdin });
			lines.on("line", (line) => {
				const msg = JSON.parse(line);
				if (msg.type === "init") {
					process.stdout.write('{"id":' + msg.id + ',');
					setTimeout(() => {
						process.stdout.write('"ok":true,"environment":{"actions":{"wait":{"description":"Wait."}}}}\\n');
						process.stdout.write('not json noise\\n');
						process.stdout.write('{"id":999,"ok":true}\\n');
					}, 10);
				} else if (msg.type === "observe") {
					process.stdout.write(JSON.stringify({ id: msg.id, ok: true, observation: { text: "ok screen" } }) + "\\n");
				} else if (msg.type === "close") { lines.close(); process.exit(0); }
			});
		`;
		const env = new StdioRouterEnvironment({ command: [process.execPath, "-e", script], requestTimeoutMs: 5_000 });
		try {
			const environment = await env.init();
			expect(environment?.actions).toMatchObject({ wait: { description: "Wait." } });
			const observation = await env.observe();
			expect(observation.text).toBe("ok screen");
		} finally {
			await env.close();
		}
	});

	it("times out requests against a silent adapter", async () => {
		const env = new StdioRouterEnvironment({
			command: [
				process.execPath,
				"-e",
				"process.stdin.on('end', () => process.exit(0)); setInterval(() => {}, 1000);",
			],
			requestTimeoutMs: 50,
		});
		try {
			await expect(env.init()).rejects.toThrow(/init timed out after 50ms/);
		} finally {
			await env.close();
		}
	});
});
