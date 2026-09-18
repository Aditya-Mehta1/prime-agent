// Visual-parity driver extension: registers the faux provider with scripted
// responses read from PRIME_AGENT_FAUX_SCRIPT (same harness contract the Rust
// rewrite uses). Verification harness only; never installed for real users.
import {
	registerFauxProvider,
	fauxAssistantMessage,
	fauxText,
	fauxThinking,
	fauxToolCall,
	getApiProvider,
} from "@earendil-works/pi-ai";
import { readFileSync } from "node:fs";

export default function registerVisualFaux(pi) {
	const scriptPath = process.env.PRIME_AGENT_FAUX_SCRIPT;
	const script = scriptPath ? JSON.parse(readFileSync(scriptPath, "utf8")) : {};
	const provider = script.provider || "faux";
	const modelId = script.modelId || "faux-1";
	const faux = registerFauxProvider({
		provider,
		api: "faux",
		models: [
			{
				id: modelId,
				name: script.modelName || "Faux Model",
				reasoning: script.reasoning ?? false,
				contextWindow: script.contextWindow ?? 128000,
			},
		],
		tokensPerSecond: script.tokensPerSecond,
	});
	const responses = (script.responses || []).map((entry) => {
		let content;
		if (typeof entry === "string") {
			content = [fauxText(entry)];
		} else if (Array.isArray(entry.content)) {
			content = entry.content.map((block) => {
				if (block.type === "thinking") return fauxThinking(block.thinking);
				if (block.type === "toolCall") return fauxToolCall(block.name, block.arguments, { id: block.id });
				return fauxText(block.text);
			});
		} else {
			content = [fauxText(entry.text || "")];
		}
		const stopReason =
			entry.stopReason ||
			(content.some((block) => block.type === "toolCall") ? "toolUse" : "stop");
		return fauxAssistantMessage(content, { stopReason });
	});
	faux.setResponses(responses);
	const apiProvider = getApiProvider(faux.api);
	if (!apiProvider) {
		throw new Error("Faux API provider was not registered");
	}
	pi.registerProvider(provider, {
		api: faux.api,
		apiKey: "faux-key",
		baseUrl: faux.getModel().baseUrl,
		streamSimple: apiProvider.streamSimple,
		models: faux.models.map((model) => ({
			api: model.api,
			baseUrl: model.baseUrl,
			contextWindow: model.contextWindow,
			cost: model.cost,
			id: model.id,
			input: model.input,
			maxTokens: model.maxTokens,
			name: model.name,
			reasoning: model.reasoning,
		})),
	});
}
