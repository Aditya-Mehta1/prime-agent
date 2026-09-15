import { describe, expect, it } from "vitest";
import { getModel } from "../src/models.js";
import { complete } from "../src/stream.js";
import type { Context } from "../src/types.js";
import { isContextOverflow } from "../src/utils/overflow.js";

// Classification logic is unit-covered by overflow.test.ts. This file keeps exactly one
// live end-to-end overflow case so the real provider error shape stays wired to isContextOverflow.

const LOREM_IPSUM = `Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. `;

function generateOverflowContent(contextWindow: number): string {
	const targetChars = (contextWindow + 10000) * 4 * 1.5;
	return LOREM_IPSUM.repeat(Math.ceil(targetChars / LOREM_IPSUM.length));
}

describe.skipIf(!process.env.ANTHROPIC_API_KEY)("Context overflow (live)", () => {
	it("surfaces an error response that isContextOverflow classifies as overflow", async () => {
		const model = getModel("anthropic", "claude-haiku-4-5");
		const context: Context = {
			systemPrompt: "You are a helpful assistant.",
			messages: [{ role: "user", content: generateOverflowContent(model.contextWindow), timestamp: Date.now() }],
		};

		const response = await complete(model, context, { apiKey: process.env.ANTHROPIC_API_KEY! });

		expect(response.stopReason).toBe("error");
		expect(isContextOverflow(response, model.contextWindow)).toBe(true);
	}, 120000);
});
