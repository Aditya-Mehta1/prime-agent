import type { Api, Model } from "@earendil-works/pi-ai";
import type { ProviderRetryPolicy } from "../provider-retry.js";
import { compileActionSpace } from "./action-space.js";
import { createModelDecisionFunction, routerThinkingLevel } from "./decide.js";
import { runSystemRouterLoop } from "./loop.js";
import { StdioRouterEnvironment } from "./stdio-environment.js";
import {
	type ParsedSystemRouterRunSpec,
	parseEnvironmentActions,
	type RouterEnvironment,
	type SystemRouterRunResult,
} from "./types.js";

/** The environment the segment runner needs: the loop contract plus the init handshake. */
export interface RouterSegmentEnvironment extends RouterEnvironment {
	/** Initialize the adapter and return its environment info (default action space), if any. */
	init(): Promise<Record<string, unknown> | undefined>;
}

export interface RouterSegmentOptions {
	model: Model<Api>;
	apiKey?: string;
	headers?: Record<string, string>;
	sessionId?: string;
	policy?: ProviderRetryPolicy;
	/** Defaults to a StdioRouterEnvironment built from the spec. */
	env?: RouterSegmentEnvironment;
}

/**
 * Run one bounded router segment: always init the adapter (it carries the
 * init payload, e.g. a ROM path), resolve the action space (the spec's
 * declaration wins over the adapter's defaults), then run the loop. The
 * adapter is closed on every path, including init failures and a missing
 * action space, so the subprocess never leaks.
 */
export async function runRouterSegment(
	spec: ParsedSystemRouterRunSpec,
	options: RouterSegmentOptions,
): Promise<SystemRouterRunResult> {
	const env =
		options.env ??
		new StdioRouterEnvironment({
			command: spec.environment.stdio.command,
			...(spec.environment.stdio.cwd ? { cwd: spec.environment.stdio.cwd } : {}),
			requestTimeoutMs: spec.environment.stdio.requestTimeoutMs,
			...(spec.environment.stdio.init !== undefined ? { init: spec.environment.stdio.init } : {}),
		});
	const segmentStartedAt = Date.now();
	try {
		let environment: Awaited<ReturnType<RouterSegmentEnvironment["init"]>>;
		try {
			// The segment timeout bounds the whole segment, adapter init included.
			environment = await raceInitAgainstSegmentTimeout(env.init(), spec.timeoutMs, segmentStartedAt);
		} catch (error) {
			throw new Error(
				`environment adapter init exceeded the segment timeout of ${spec.timeoutMs}ms: ${error instanceof Error ? error.message : String(error)}`,
			);
		}
		const actions = parseEnvironmentActions(spec.actions, environment?.actions);
		if (!actions) {
			throw new Error("system_router.run has no action space: declare one or use an adapter that supplies its own");
		}
		const { byName } = compileActionSpace(actions);
		return await runSystemRouterLoop({
			env,
			goal: spec.goal,
			actions,
			decide: createModelDecisionFunction({
				model: options.model,
				apiKey: options.apiKey,
				headers: options.headers,
				sessionId: options.sessionId,
				policy: options.policy,
				actions: byName,
			}),
			model: {
				id: options.model.id,
				provider: options.model.provider,
				input: options.model.input ?? [],
				thinkingLevel: routerThinkingLevel(options.model),
			},
			gate: spec.gate,
			maxSteps: spec.maxSteps,
			// The loop's deadline covers the remaining segment budget after init.
			timeoutMs: Math.max(1, spec.timeoutMs - (Date.now() - segmentStartedAt)),
			historySteps: spec.historySteps,
			observationChars: spec.observationChars,
		});
	} finally {
		// The loop closes the env on its own paths; this guards the window
		// between init and the loop so the adapter process never leaks.
		await env.close().catch(() => {});
	}
}

/** Race adapter init against the segment budget without leaking a late rejection. */
async function raceInitAgainstSegmentTimeout<T>(work: Promise<T>, timeoutMs: number, startedAt: number): Promise<T> {
	const remaining = timeoutMs - (Date.now() - startedAt);
	if (remaining <= 0) {
		void work.catch(() => {});
		throw new Error("segment budget already exhausted before init");
	}
	let timer: ReturnType<typeof setTimeout> | undefined;
	const timeout = new Promise<never>((_, reject) => {
		timer = setTimeout(() => reject(new Error("init timed out")), remaining);
		if (timer && typeof timer === "object" && "unref" in timer) timer.unref();
	});
	void timeout.catch(() => {});
	void work.catch(() => {});
	try {
		return await Promise.race([work, timeout]);
	} catch (error) {
		throw error instanceof Error ? error : new Error(String(error));
	} finally {
		if (timer) clearTimeout(timer);
	}
}
