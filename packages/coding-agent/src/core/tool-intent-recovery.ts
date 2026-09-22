import type { AssistantMessage } from "@earendil-works/pi-ai";
import type { CustomMessage } from "./messages.js";

export const TOOL_INTENT_RECOVERY_CUSTOM_TYPE = "tool_intent_recovery";

/** Dynamo GLM-5.3 routes that report dropped tool calls as length. */
const DROPS_TOOL_CALLS_AS_LENGTH = /(?:^|\/)glm-5\.3(?:-fast(?:-\w+)?)?$/i;

/** Hidden model-facing retry message. */
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

/** Match protocol finishes eligible for a retry when no tool call was delivered. */
export function isDroppedToolCallStop(message: AssistantMessage): boolean {
	if (message.content.some((part) => part.type === "toolCall")) {
		return false;
	}
	if (message.stopReason === "toolUse") {
		return true;
	}
	return (
		message.stopReason === "length" &&
		message.provider === "prime-inference" &&
		DROPS_TOOL_CALLS_AS_LENGTH.test(message.model)
	);
}
