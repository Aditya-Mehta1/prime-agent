import { Type } from "typebox";
import { beforeAll, describe, expect, it } from "vitest";
import { getModel } from "../src/models.js";
import { completeSimple, getEnvApiKey } from "../src/stream.js";
import type { Api, Message, Model, Tool, ToolResultMessage } from "../src/types.js";
import { resolveApiKey } from "./oauth.js";

const testToolSchema = Type.Object({ value: Type.Number({ description: "A number to double" }) });
const testTool: Tool<typeof testToolSchema> = {
	name: "double_number",
	description: "Doubles a number and returns the result",
	parameters: testToolSchema,
};

interface ProviderModelPair {
	provider: string;
	model: string;
	label: string;
	apiOverride?: Api;
}

// One pair per wire format that has to survive a handoff: anthropic, google, openai-completions,
// openai-responses and the codex variant. Adding every catalog entry only multiplied live calls.
const PROVIDER_MODEL_PAIRS: ProviderModelPair[] = [
	{ provider: "anthropic", model: "claude-sonnet-4-5", label: "anthropic-claude-sonnet-4-5" },
	{ provider: "google", model: "gemini-3-flash-preview", label: "google-gemini-3-flash-preview" },
	{
		provider: "openai",
		model: "gpt-4o-mini",
		label: "openai-completions-gpt-4o-mini",
		apiOverride: "openai-completions",
	},
	{ provider: "openai", model: "gpt-5-mini", label: "openai-responses-gpt-5-mini" },
	{ provider: "openai-codex", model: "gpt-5.2-codex", label: "openai-codex-gpt-5.2-codex" },
];

function resolveProviderModel(pair: ProviderModelPair): Model<Api> | undefined {
	const base = (getModel as (provider: string, model: string) => Model<Api> | undefined)(pair.provider, pair.model);
	if (!base) return undefined;
	return pair.apiOverride ? { ...base, api: pair.apiOverride } : base;
}

function hasApiKey(pair: ProviderModelPair): boolean {
	return !!getEnvApiKey(pair.provider);
}

async function getApiKey(provider: string): Promise<string | undefined> {
	return (await resolveApiKey(provider)) ?? getEnvApiKey(provider);
}

/** Runs one tool-call round trip and returns the resulting provider-shaped message history. */
async function generateContext(pair: ProviderModelPair, apiKey: string): Promise<Message[] | null> {
	const model = resolveProviderModel(pair);
	if (!model) return null;
	const reasoning = model.reasoning === true ? ("high" as const) : undefined;
	const userMessage: Message = {
		role: "user",
		content: "Please double the number 21 using the double_number tool.",
		timestamp: Date.now(),
	};

	const assistantResponse = await completeSimple(
		model,
		{ systemPrompt: "Use the provided tool to complete the task.", messages: [userMessage], tools: [testTool] },
		{ apiKey, reasoning },
	);
	if (assistantResponse.stopReason === "error") return null;

	const toolCall = assistantResponse.content.find((c) => c.type === "toolCall");
	if (!toolCall || toolCall.type !== "toolCall") return null;

	const toolResult: ToolResultMessage = {
		role: "toolResult",
		toolCallId: toolCall.id,
		toolName: toolCall.name,
		content: [{ type: "text", text: "42" }],
		isError: false,
		timestamp: Date.now(),
	};
	const messages = [userMessage, assistantResponse, toolResult];
	const finalResponse = await completeSimple(
		model,
		{ systemPrompt: "You are a helpful assistant.", messages, tools: [testTool] },
		{ apiKey, reasoning },
	);
	if (finalResponse.stopReason === "error") return null;
	return [...messages, finalResponse];
}

describe("Cross-Provider Handoff configuration", () => {
	it("references models in the generated catalog", () => {
		const missing = PROVIDER_MODEL_PAIRS.filter((pair) => !resolveProviderModel(pair)).map(
			(pair) => `${pair.provider}/${pair.model}`,
		);
		expect(missing).toEqual([]);
	});
});

describe.skipIf(!PROVIDER_MODEL_PAIRS.some(hasApiKey))("Cross-Provider Handoff", () => {
	const contexts = new Map<string, Message[]>();

	beforeAll(async () => {
		for (const pair of PROVIDER_MODEL_PAIRS) {
			if (!hasApiKey(pair)) continue;
			const apiKey = await getApiKey(pair.provider);
			if (!apiKey) continue;
			const messages = await generateContext(pair, apiKey);
			if (messages && messages.length >= 4) contexts.set(pair.label, messages);
		}
	}, 300000);

	it("accepts a history assembled from every other provider without a wire-format error", async () => {
		if (contexts.size < 2) return;
		const failures: string[] = [];

		for (const targetPair of PROVIDER_MODEL_PAIRS) {
			const model = resolveProviderModel(targetPair);
			const apiKey = hasApiKey(targetPair) ? await getApiKey(targetPair.provider) : undefined;
			if (!model || !apiKey) continue;

			const foreign = [...contexts.entries()]
				.filter(([label]) => label !== targetPair.label)
				.flatMap(([, messages]) => messages);
			if (foreign.length === 0) continue;

			const response = await completeSimple(
				model,
				{
					systemPrompt: "You are a helpful assistant.",
					messages: [
						...foreign,
						{ role: "user", content: "Say 'Hello, handoff successful!'", timestamp: Date.now() },
					],
					tools: [testTool],
				},
				{ apiKey, reasoning: model.reasoning === true ? "high" : undefined },
			);
			if (response.stopReason === "error") failures.push(`${targetPair.label}: ${response.errorMessage}`);
		}

		expect(failures).toEqual([]);
	}, 600000);
});
