import {
	type Api,
	type AssistantMessage,
	clampThinkingLevel,
	completeSimple,
	type Model,
	type ModelThinkingLevel,
} from "@earendil-works/pi-ai";
import { completeWithProviderRetry, type ProviderRetryPolicy } from "../provider-retry.js";
import type { CompiledAction } from "./action-space.js";
import { ESCALATE_ACTION, FINISH_ACTION } from "./types.js";

/** Bounded output: the decision object is a few dozen tokens. */
/**
 * Output cap for one decision call. The decision object itself is a few dozen
 * tokens, but models that keep reasoning even when "off" is requested
 * (mandatory-reasoning models map "off" to their minimum effort) spend output
 * tokens on reasoning content first; a small cap would truncate the decision.
 */
export const ROUTER_DECISION_MAX_TOKENS = 4_096;

export const ROUTER_DECISION_SYSTEM_PROMPT = [
	"You are the action model inside a System 1 control loop.",
	"You do not plan, explain, or write prose.",
	"Each step you receive the goal, the latest observation, recent history, and the finite list of available actions.",
	"Reply with exactly one JSON object: the chosen action, its parameter values, and your confidence that it is the right next step.",
	"Reply with JSON only.",
].join(" ");

export interface RouterDecisionRequest {
	prompt: string;
	/** Base64 PNG screenshot; included only when the action model accepts images. */
	image?: string;
}

export interface RouterDecisionOutcome {
	/** null when the reply was not a valid single choice from the action space. */
	action: string | null;
	params: Record<string, string>;
	/** Parsed confidence in [0, 1]; null on parse failure. */
	confidence: number | null;
	rawText: string;
	/** Why a malformed reply was refused; counts toward the refusal streak. */
	parseError?: string;
	/** Transport or model failure; fails the run. */
	modelError?: string;
	usage?: { inputTokens?: number; outputTokens?: number };
}

export interface RouterDecisionContext {
	model: Model<Api>;
	apiKey?: string;
	headers?: Record<string, string>;
	sessionId?: string;
	policy?: ProviderRetryPolicy;
	actions: Map<string, CompiledAction>;
}

export type RouterDecisionFunction = (request: RouterDecisionRequest) => Promise<RouterDecisionOutcome>;

/** The thinking level the System 1 decision calls run at: off, clamped per model. */
export function routerThinkingLevel(model: Model<Api>): ModelThinkingLevel {
	return clampThinkingLevel(model, "off");
}

function textOf(message: AssistantMessage): string {
	return message.content
		.filter((content): content is { type: "text"; text: string } => content.type === "text")
		.map((content) => content.text)
		.join("\n")
		.trim();
}

function extractFirstJsonObject(raw: string): Record<string, unknown> | null {
	const fenced = raw.match(/```(?:json)?\s*([\s\S]*?)```/);
	const candidates = [fenced?.[1], raw].flatMap((candidate) => (candidate ? [candidate] : []));
	for (const candidate of candidates) {
		const trimmed = candidate.trim();
		const start = trimmed.indexOf("{");
		const end = trimmed.lastIndexOf("}");
		if (start === -1 || end <= start) continue;
		try {
			const parsed: unknown = JSON.parse(trimmed.slice(start, end + 1));
			if (typeof parsed === "object" && parsed !== null && !Array.isArray(parsed)) {
				return parsed as Record<string, unknown>;
			}
		} catch {
			// Try the next candidate.
		}
	}
	return null;
}

/** Validate one decision against the compiled action space. Free text never passes. */
export function parseDecision(raw: string, actions: Map<string, CompiledAction>): RouterDecisionOutcome {
	const object = extractFirstJsonObject(raw);
	if (!object) {
		return { action: null, params: {}, confidence: null, rawText: raw, parseError: "reply was not a JSON object" };
	}
	const actionName = object.action;
	if (typeof actionName !== "string" || !actions.has(actionName)) {
		return {
			action: null,
			params: {},
			confidence: null,
			rawText: raw,
			parseError: `unknown action ${JSON.stringify(actionName ?? null)}`,
		};
	}
	const action = actions.get(actionName);
	if (!action) {
		return { action: null, params: {}, confidence: null, rawText: raw, parseError: "unknown action" };
	}
	const params: Record<string, string> = {};
	const rawParams = object.params;
	if (rawParams !== undefined) {
		if (typeof rawParams !== "object" || rawParams === null || Array.isArray(rawParams)) {
			return {
				action: null,
				params: {},
				confidence: null,
				rawText: raw,
				parseError: "params must be an object",
			};
		}
		for (const [key, value] of Object.entries(rawParams as Record<string, unknown>)) {
			const allowed = action.params[key];
			if (!allowed) {
				return {
					action: null,
					params: {},
					confidence: null,
					rawText: raw,
					parseError: `unknown param "${key}" for action "${actionName}"`,
				};
			}
			if (typeof value !== "string" || !(value in allowed.choices)) {
				return {
					action: null,
					params: {},
					confidence: null,
					rawText: raw,
					parseError: `param "${key}" value must be one of its declared choices`,
				};
			}
			params[key] = value;
		}
	}
	const confidence = object.confidence;
	if (typeof confidence !== "number" || !Number.isFinite(confidence) || confidence < 0 || confidence > 1) {
		return {
			action: null,
			params: {},
			confidence: null,
			rawText: raw,
			parseError: "confidence must be a number in [0, 1]",
		};
	}
	return { action: actionName, params, confidence, rawText: raw };
}

export function supportsImages(model: Model<Api>): boolean {
	return (model.input ?? []).includes("image");
}

/**
 * Build the System 1 decision function: ONE model call per step with thinking
 * disabled (clamped per model), provider-retry, and strict single-choice
 * parsing against the declared action space.
 */
export function createModelDecisionFunction(context: RouterDecisionContext): RouterDecisionFunction {
	const thinkingLevel = routerThinkingLevel(context.model);
	const includeImages = supportsImages(context.model);
	return async (request) => {
		const content: Array<{ type: "text"; text: string } | { type: "image"; data: string; mimeType: string }> = [
			{ type: "text", text: request.prompt },
		];
		if (includeImages && request.image) {
			content.push({ type: "image", data: request.image, mimeType: "image/png" });
		}
		const message = await completeWithProviderRetry(
			() =>
				completeSimple(
					context.model,
					{
						systemPrompt: ROUTER_DECISION_SYSTEM_PROMPT,
						messages: [
							{
								role: "user",
								content,
								timestamp: Date.now(),
							},
						],
					},
					{
						reasoning: thinkingLevel,
						maxTokens: Math.min(context.model.maxTokens, ROUTER_DECISION_MAX_TOKENS),
						apiKey: context.apiKey,
						headers: context.headers,
						sessionId: context.sessionId,
					},
				),
			{ policy: context.policy },
		);
		const usage = {
			inputTokens: message.usage?.input,
			outputTokens: message.usage?.output,
		};
		if (message.stopReason === "error") {
			return {
				action: null,
				params: {},
				confidence: null,
				rawText: "",
				modelError: `decision model failed: ${message.errorMessage || "unknown error"}`,
				usage,
			};
		}
		const outcome = parseDecision(textOf(message), context.actions);
		return { ...outcome, usage };
	};
}

export const RESERVED_ACTION_NAMES = new Set([FINISH_ACTION, ESCALATE_ACTION]);
