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
	try {
		const environment = await env.init();
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
			timeoutMs: spec.timeoutMs,
			historySteps: spec.historySteps,
			observationChars: spec.observationChars,
		});
	} finally {
		// The loop closes the env on its own paths; this guards the window
		// between init and the loop so the adapter process never leaks.
		await env.close().catch(() => {});
	}
}
