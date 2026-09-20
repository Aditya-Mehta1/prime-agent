import {
	compileActionSpace,
	compileDecisionPrompt,
	formatHistoryEntry,
	gateThreshold,
	observationDigest,
} from "./action-space.js";
import type { RouterDecisionFunction } from "./decide.js";
import {
	DEFAULT_ROUTER_HISTORY_STEPS,
	ESCALATE_ACTION,
	FINISH_ACTION,
	type RouterActionSpec,
	type RouterEnvironment,
	type RouterExecution,
	type RouterGateSpec,
	type RouterModelInfo,
	type RouterRunStatus,
	type RouterStepTrace,
	type SystemRouterRunResult,
} from "./types.js";

/** Consecutive gate refusals before the loop stops as stuck. */
export const ROUTER_REFUSAL_STREAK_LIMIT = 3;
/** The same action with the same params on the same observation, this many times, is stuck. */
export const ROUTER_REPETITION_LIMIT = 2;

/** Race work against a deadline without leaking a late rejection from the losing side. */
async function raceDeadline<T>(work: Promise<T>, deadline: Promise<"deadline">): Promise<T | "deadline"> {
	// Both losers are marked handled: attach handlers to the promises themselves
	// before the race, never after it resolves.
	const handledWork = work.finally(() => {});
	void handledWork.catch(() => {});
	const result = (await Promise.race([handledWork, deadline])) as T | "deadline";
	if (result === "deadline") {
		// The work may still reject later; it already has a handler, so it cannot crash.
		return "deadline";
	}
	return result;
}

export interface SystemRouterLoopOptions {
	env: RouterEnvironment;
	goal: string;
	actions: Record<string, RouterActionSpec>;
	decide: RouterDecisionFunction;
	model: RouterModelInfo;
	gate?: RouterGateSpec;
	maxSteps: number;
	timeoutMs: number;
	historySteps?: number;
	observationChars?: number;
	/** External abort (host shutdown). The in-flight step completes and the run ends failed("aborted"). */
	signal?: AbortSignal;
}

/**
 * The System 1 step loop: observe -> decide (ONE call) -> gate -> execute -> record.
 * Mirrors the SystemOneHarness controller: a probability on every transition,
 * confidence gates per risk, refusal streaks and a repetition guard toward
 * stuck, explicit budgets, and a complete trace.
 */
export async function runSystemRouterLoop(options: SystemRouterLoopOptions): Promise<SystemRouterRunResult> {
	const declaredActions = Object.keys(options.actions);
	if (declaredActions.length === 0) {
		throw new Error("system router action space is empty (finish and escalate are always appended)");
	}
	const { byName } = compileActionSpace(options.actions);
	const historySteps = options.historySteps ?? DEFAULT_ROUTER_HISTORY_STEPS;
	const observationChars = options.observationChars ?? 6_000;
	const startedAt = Date.now();
	const deadlineAt = startedAt + options.timeoutMs;
	let deadlineTimer: ReturnType<typeof setTimeout> | undefined;
	const deadline = new Promise<"deadline">((resolve) => {
		const remaining = deadlineAt - Date.now();
		deadlineTimer = setTimeout(() => resolve("deadline"), Math.max(0, remaining));
	});
	// A late deadline resolution must never surface as an unhandled rejection; it never rejects,
	// but attach a handler anyway so the invariant is structural, not incidental.
	void deadline.catch(() => {});

	const trace: RouterStepTrace[] = [];
	const history: string[] = [];
	let inputTokens = 0;
	let outputTokens = 0;
	let executed = 0;
	let refused = 0;
	let refusalStreak = 0;
	let repeatCount = 0;
	let lastSignature: string | null = null;

	const finish = (status: RouterRunStatus, reason: string, summary: string): SystemRouterRunResult => ({
		status,
		reason,
		steps: trace.length,
		executed,
		refused,
		trace,
		summary,
		model: {
			provider: options.model.provider,
			id: options.model.id,
			thinkingLevel: options.model.thinkingLevel,
		},
		usage: { inputTokens, outputTokens },
	});

	try {
		try {
			await options.env.reset(options.goal);
		} catch (error) {
			return finish(
				"failed",
				"environment_error",
				`Environment failed resetting at segment start: ${error instanceof Error ? error.message : String(error)}`,
			);
		}
		for (let step = 0; step < options.maxSteps; step += 1) {
			if (options.signal?.aborted) {
				return finish("failed", "aborted", "Router aborted before the current step.");
			}
			let observation: Awaited<ReturnType<RouterEnvironment["observe"]>> | "deadline";
			try {
				observation = await raceDeadline(options.env.observe(), deadline);
			} catch (error) {
				return finish(
					"failed",
					"environment_error",
					`Environment failed observing at step ${step}: ${error instanceof Error ? error.message : String(error)}`,
				);
			}
			if (observation === "deadline") {
				return finish(
					"incomplete",
					"timeout",
					`Stopped at step ${step}: the segment timeout of ${options.timeoutMs}ms elapsed.`,
				);
			}
			if (observation.terminal) {
				return finish(
					"done",
					"environment_terminal",
					`Environment reported terminal state at step ${step} after ${executed} executed action(s).`,
				);
			}
			const digest = observationDigest(observation);
			const prompt = compileDecisionPrompt({
				goal: options.goal,
				observation,
				history: historySteps > 0 ? history.slice(-historySteps) : [],
				actions: byName,
				observationChars,
			});
			const decisionStarted = Date.now();
			let decision: Awaited<ReturnType<RouterDecisionFunction>> | "deadline";
			try {
				decision = await raceDeadline(
					options.decide({
						prompt,
						...(observation.image ? { image: observation.image } : {}),
					}),
					deadline,
				);
			} catch (error) {
				return finish(
					"failed",
					"decision_model_error",
					`Decision function threw at step ${step}: ${error instanceof Error ? error.message : String(error)}`,
				);
			}
			if (decision === "deadline") {
				return finish(
					"incomplete",
					"timeout",
					`Stopped at step ${step}: the segment timeout of ${options.timeoutMs}ms elapsed mid-decision.`,
				);
			}
			const latencyMs = Date.now() - decisionStarted;
			inputTokens += decision.usage?.inputTokens ?? 0;
			outputTokens += decision.usage?.outputTokens ?? 0;

			if (decision.modelError) {
				const stepTrace: RouterStepTrace = {
					step,
					timestampMs: decisionStarted,
					latencyMs,
					action: null,
					params: {},
					confidence: null,
					gate: { threshold: 0, verdict: "parse_failure" },
					observationDigest: digest,
					observationChars: observation.text.length,
					result: decision.modelError,
					terminal: false,
					thinkingLevel: options.model.thinkingLevel,
					...(decision.usage ? { usage: decision.usage } : {}),
				};
				trace.push(stepTrace);
				return finish(
					"failed",
					"decision_model_error",
					`Decision model failed at step ${step}: ${decision.modelError}`,
				);
			}

			const action = decision.action ? byName.get(decision.action) : undefined;
			if (!action || decision.action === null || decision.confidence === null) {
				refusalStreak += 1;
				refused += 1;
				const reason = decision.parseError ?? "decision was not a valid choice";
				trace.push({
					step,
					timestampMs: decisionStarted,
					latencyMs,
					action: null,
					params: {},
					confidence: null,
					gate: { threshold: 0, verdict: "parse_failure" },
					observationDigest: digest,
					observationChars: observation.text.length,
					result: `refused: ${reason}`,
					terminal: false,
					thinkingLevel: options.model.thinkingLevel,
					...(decision.usage ? { usage: decision.usage } : {}),
				});
				if (refusalStreak >= ROUTER_REFUSAL_STREAK_LIMIT) {
					return finish(
						"stuck",
						"no_confident_decision",
						`Stopped at step ${step}: ${refusalStreak} consecutive decisions were not a valid choice from the action space.`,
					);
				}
				history.push("invalid decision -> refused");
				continue;
			}

			const threshold = gateThreshold(options.gate ?? {}, action);
			if (decision.confidence < threshold) {
				refusalStreak += 1;
				refused += 1;
				const result = `refused: confidence ${decision.confidence.toFixed(2)} below ${action.risk} gate ${threshold.toFixed(2)}`;
				trace.push({
					step,
					timestampMs: decisionStarted,
					latencyMs,
					action: action.name,
					params: decision.params,
					confidence: decision.confidence,
					gate: { threshold, verdict: "refused" },
					observationDigest: digest,
					observationChars: observation.text.length,
					result,
					terminal: false,
					thinkingLevel: options.model.thinkingLevel,
					...(decision.usage ? { usage: decision.usage } : {}),
				});
				history.push(formatHistoryEntry(action.name, decision.params, "refused below gate"));
				if (refusalStreak >= ROUTER_REFUSAL_STREAK_LIMIT) {
					return finish(
						"stuck",
						"no_confident_decision",
						`Stopped at step ${step}: no action cleared its confidence gate for ${refusalStreak} consecutive decisions.`,
					);
				}
				continue;
			}

			// The gate passed.
			refusalStreak = 0;
			if (action.name === FINISH_ACTION) {
				trace.push({
					step,
					timestampMs: decisionStarted,
					latencyMs,
					action: action.name,
					params: {},
					confidence: decision.confidence,
					gate: { threshold, verdict: "pass" },
					observationDigest: digest,
					observationChars: observation.text.length,
					result: "goal declared reached",
					terminal: true,
					thinkingLevel: options.model.thinkingLevel,
					...(decision.usage ? { usage: decision.usage } : {}),
				});
				return finish(
					"done",
					"goal_reached",
					`Goal declared reached at step ${step} after ${executed} executed action(s).`,
				);
			}
			if (action.name === ESCALATE_ACTION) {
				trace.push({
					step,
					timestampMs: decisionStarted,
					latencyMs,
					action: action.name,
					params: {},
					confidence: decision.confidence,
					gate: { threshold, verdict: "pass" },
					observationDigest: digest,
					observationChars: observation.text.length,
					result: "escalation requested",
					terminal: true,
					thinkingLevel: options.model.thinkingLevel,
					...(decision.usage ? { usage: decision.usage } : {}),
				});
				return finish(
					"escalated",
					"escalation_requested",
					`Escalated at step ${step} after ${executed} executed action(s); review the trace and steer.`,
				);
			}

			// Repeated-state detection: the same action with the same params on the same observation.
			const canonicalParams = Object.keys(decision.params)
				.sort()
				.map((key) => `${key}=${decision.params[key]}`)
				.join("&");
			const signature = `${digest}:${action.name}:${canonicalParams}`;
			repeatCount = signature === lastSignature ? repeatCount + 1 : 0;
			lastSignature = signature;
			if (repeatCount + 1 >= ROUTER_REPETITION_LIMIT) {
				refused += 1;
				trace.push({
					step,
					timestampMs: decisionStarted,
					latencyMs,
					action: action.name,
					params: decision.params,
					confidence: decision.confidence,
					gate: { threshold, verdict: "pass" },
					observationDigest: digest,
					observationChars: observation.text.length,
					result: `repeated ${action.name} on the same observation ${repeatCount + 1} times`,
					terminal: false,
					thinkingLevel: options.model.thinkingLevel,
					...(decision.usage ? { usage: decision.usage } : {}),
				});
				return finish(
					"stuck",
					"repeated_state",
					`Stopped at step ${step}: ${action.name} repeated on the same observation ${repeatCount + 1} times.`,
				);
			}

			let executionResult: RouterExecution | "deadline";
			try {
				executionResult = await raceDeadline(options.env.execute(action.name, decision.params), deadline);
			} catch (error) {
				return finish(
					"failed",
					"environment_error",
					`Environment failed executing ${action.name} at step ${step}: ${error instanceof Error ? error.message : String(error)}`,
				);
			}
			if (executionResult === "deadline") {
				return finish(
					"incomplete",
					"timeout",
					`Stopped at step ${step}: the segment timeout of ${options.timeoutMs}ms elapsed mid-execution.`,
				);
			}
			executed += 1;
			const resultText =
				executionResult.text.length > 240 ? `${executionResult.text.slice(0, 237)}...` : executionResult.text;
			trace.push({
				step,
				timestampMs: decisionStarted,
				latencyMs,
				action: action.name,
				params: decision.params,
				confidence: decision.confidence,
				gate: { threshold, verdict: "pass" },
				observationDigest: digest,
				observationChars: observation.text.length,
				result: resultText,
				terminal: executionResult.terminal === true,
				thinkingLevel: options.model.thinkingLevel,
				...(decision.usage ? { usage: decision.usage } : {}),
			});
			history.push(formatHistoryEntry(action.name, decision.params, resultText));
			if (executionResult.terminal) {
				return finish(
					"done",
					"environment_terminal",
					`Environment reported terminal state at step ${step} after ${executed} executed action(s).`,
				);
			}
		}
		return finish(
			"incomplete",
			"max_steps",
			`Stopped after ${options.maxSteps} steps: the segment step budget is exhausted; review the trace and steer.`,
		);
	} finally {
		if (deadlineTimer) clearTimeout(deadlineTimer);
		await options.env.close().catch(() => {});
	}
}
