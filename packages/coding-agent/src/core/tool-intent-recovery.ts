import type { AssistantMessage, TextContent } from "@earendil-works/pi-ai";
import type { CustomMessage } from "./messages.js";

export const TOOL_INTENT_RECOVERY_CUSTOM_TYPE = "tool_intent_recovery";

const AFFECTED_MODEL_ID = /^(?:internal\/)?glm-5\.3(?:-fast)?$/i;

/** Longest visible text still treated as a bare action preface. */
const MAX_PREFACE_LENGTH = 200;

// Prefer missing a recovery to forcing a tool for an answer or a request for permission.
const PROMISED_ACTION =
	/^(?:let me|i['’]ll|i will)\s+(?:(?:actually|first|just|quickly|now)\s+){0,2}(?:check|examine|fetch|grep|inspect|list|look\s+(?:at|into|through|for)|open|query|read|recheck|review|run|scan|search|test|trace|validate|verify|view)\b/i;
const DEFERRED_OR_CONDITIONAL =
	/\?|\b(?:if|unless|once|after|before|until|later|tomorrow|permission|approval|approve|confirm|wait|waiting|cannot|can't|won't|don't|not|instead)\b/i;

/** Hidden model-facing continuation appended before the required-tool retry. */
export function createToolIntentRecoveryMessage(timestamp = Date.now()): CustomMessage {
	return {
		role: "custom",
		customType: TOOL_INTENT_RECOVERY_CUSTOM_TYPE,
		content:
			"Use a tool to perform the action you just announced, within the user's existing instructions and permissions. Do not repeat the preamble.",
		display: false,
		timestamp,
	};
}

export function isToolIntentStop(message: AssistantMessage): boolean {
	if (message.provider !== "prime-inference" || !AFFECTED_MODEL_ID.test(message.model)) {
		return false;
	}
	const visibleText = message.content
		.filter((part): part is TextContent => part.type === "text")
		.map((part) => part.text)
		.join("\n")
		.trim();
	if (
		visibleText.length === 0 ||
		visibleText.length > MAX_PREFACE_LENGTH ||
		DEFERRED_OR_CONDITIONAL.test(visibleText)
	) {
		return false;
	}
	return PROMISED_ACTION.test(visibleText);
}
