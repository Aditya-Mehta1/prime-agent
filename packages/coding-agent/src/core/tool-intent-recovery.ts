import type { AssistantMessage, Model } from "@earendil-works/pi-ai";
import type { CustomMessage } from "./messages.js";

export const TOOL_INTENT_RECOVERY_CUSTOM_TYPE = "tool_intent_recovery";

export function createToolIntentRecoveryMessage(timestamp = Date.now()): CustomMessage {
	return {
		role: "custom",
		customType: TOOL_INTENT_RECOVERY_CUSTOM_TYPE,
		content:
			"Your previous reply ended before it was complete. If you were about to call a tool, call it now within the user's existing instructions and permissions; otherwise finish the reply. Do not repeat the preamble.",
		display: false,
		timestamp,
	};
}

export function isDroppedToolCallStop(message: AssistantMessage, model: Model<string> | undefined): boolean {
	if (message.content.some((part) => part.type === "toolCall")) {
		return false;
	}
	switch (message.stopReason) {
		case "toolUse":
			return true;
		case "length":
			return retriesOnTruncatedToolCall(model);
		default:
			return false;
	}
}

// Dynamo can report truncated GLM tool calls as length.
function retriesOnTruncatedToolCall(model: Model<string> | undefined): boolean {
	return (
		model?.api === "openai-completions" &&
		model.provider === "prime-inference" &&
		model.id.toLowerCase() === "z-ai/glm-5.3"
	);
}
