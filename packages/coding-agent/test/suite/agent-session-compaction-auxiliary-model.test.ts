import type * as PiAi from "@earendil-works/pi-ai";
import { type AssistantMessage, fauxAssistantMessage } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { SUMMARIZATION_SYSTEM_PROMPT } from "../../src/core/compaction/utils.js";
import { createHarness, type Harness } from "./harness.js";

const { completeSimpleMock } = vi.hoisted(() => ({
	completeSimpleMock: vi.fn(),
}));

vi.mock("@earendil-works/pi-ai", async (importOriginal) => {
	const actual = await importOriginal<typeof PiAi>();
	return {
		...actual,
		completeSimple: completeSimpleMock,
	};
});

/** Valid summarizer response; the compaction flow only reads its text content. */
const summaryResponse: AssistantMessage = {
	role: "assistant",
	content: [{ type: "text", text: "## Goal\nTest summary" }],
	api: "faux",
	provider: "faux",
	model: "faux-1",
	usage: {
		input: 10,
		output: 10,
		cacheRead: 0,
		cacheWrite: 0,
		totalTokens: 20,
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
	},
	stopReason: "stop",
	timestamp: Date.now(),
};

/** Summary wire calls only: a split turn issues two of them. */
function summaryCalls() {
	return completeSimpleMock.mock.calls.filter(
		(call) => (call[1] as { systemPrompt?: string }).systemPrompt === SUMMARIZATION_SYSTEM_PROMPT,
	);
}

describe("AgentSession compaction auxiliary model", () => {
	const harnesses: Harness[] = [];

	beforeEach(() => {
		vi.useRealTimers();
		completeSimpleMock.mockReset();
		completeSimpleMock.mockResolvedValue(summaryResponse);
	});

	afterEach(() => {
		vi.useRealTimers();
		vi.restoreAllMocks();
		for (const harness of harnesses.splice(0)) harness.cleanup();
	});

	async function createCompactionHarness(
		options: { auxiliaryModel?: string; sessionReasoning?: boolean; auxContextWindow?: number } = {},
	): Promise<Harness> {
		const harness = await createHarness({
			models: [
				{ id: "session-model", name: "Session Model", reasoning: options.sessionReasoning },
				{ id: "aux-model", name: "Aux Model", contextWindow: options.auxContextWindow },
			],
			settings: {
				...(options.auxiliaryModel === undefined ? {} : { auxiliaryModel: options.auxiliaryModel }),
				compaction: { keepRecentTokens: 1 },
			},
			persistSession: true,
		});
		harnesses.push(harness);
		return harness;
	}

	async function compactAfterTwoTurns(harness: Harness) {
		harness.setResponses([fauxAssistantMessage("one response"), fauxAssistantMessage("two response")]);
		await harness.session.prompt("one");
		await harness.session.prompt("two");
		return await harness.session.compact();
	}

	it("routes compaction summaries to the configured auxiliary model", async () => {
		const harness = await createCompactionHarness({ auxiliaryModel: "faux/aux-model" });
		const result = await compactAfterTwoTurns(harness);

		const calls = summaryCalls();
		expect(calls.length).toBeGreaterThan(0);
		for (const call of calls) {
			expect(call[0]).toMatchObject({ provider: "faux", id: "aux-model" });
		}
		// The summary still lands as a compaction entry, so routing the call did not
		// change compaction behavior.
		expect(result.firstKeptEntryId).toBeTruthy();
		expect(result.summary).toContain("Test summary");
		const entry = harness.sessionManager.getEntries().find((candidate) => candidate.type === "compaction");
		expect(entry).toMatchObject({
			type: "compaction",
			summary: expect.stringContaining("Test summary"),
			firstKeptEntryId: result.firstKeptEntryId,
			fromHook: false,
		});
	});

	it("falls back to the session model when no auxiliary model is configured", async () => {
		const harness = await createCompactionHarness();
		await compactAfterTwoTurns(harness);

		const calls = summaryCalls();
		expect(calls.length).toBeGreaterThan(0);
		for (const call of calls) {
			expect(call[0]).toMatchObject({ provider: "faux", id: "session-model" });
		}
	});

	it("falls back to the session model when the auxiliary model is unusable", async () => {
		const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
		try {
			const harness = await createCompactionHarness({ auxiliaryModel: "faux/missing-model" });
			await compactAfterTwoTurns(harness);

			const calls = summaryCalls();
			expect(calls.length).toBeGreaterThan(0);
			for (const call of calls) {
				expect(call[0]).toMatchObject({ provider: "faux", id: "session-model" });
			}
			expect(warnSpy).toHaveBeenCalledTimes(1);
			const [message] = warnSpy.mock.calls[0];
			expect(message).toContain('auxiliaryModel "faux/missing-model" unusable for compaction summary');
			// Caught error details can embed credential material, so they must not be logged.
			expect(message).not.toContain("unavailable, unauthenticated, or expired");
		} finally {
			warnSpy.mockRestore();
		}
	});

	it("falls back to the session model when the auxiliary model cannot fit the summary request", async () => {
		const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
		try {
			const harness = await createCompactionHarness({
				auxiliaryModel: "faux/aux-model",
				auxContextWindow: 8192,
			});
			const result = await compactAfterTwoTurns(harness);

			// The serialized conversation plus the reserved completion budget cannot
			// fit 8192 tokens, so routing the summary there would fail over-limit and
			// leave compaction unable to reclaim context; the session model must run it.
			const calls = summaryCalls();
			expect(calls.length).toBeGreaterThan(0);
			for (const call of calls) {
				expect(call[0]).toMatchObject({ provider: "faux", id: "session-model" });
			}
			expect(warnSpy).toHaveBeenCalledTimes(1);
			const [message] = warnSpy.mock.calls[0];
			expect(message).toContain('auxiliaryModel "faux/aux-model" unusable for compaction summary');
			// The compaction still lands, so the fallback reclaimed context as usual.
			expect(result.firstKeptEntryId).toBeTruthy();
		} finally {
			warnSpy.mockRestore();
		}
	});

	it("keeps the auxiliary model when its context window fits the summary request", async () => {
		const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
		try {
			const harness = await createCompactionHarness({
				auxiliaryModel: "faux/aux-model",
				auxContextWindow: 131072,
			});
			const result = await compactAfterTwoTurns(harness);

			const calls = summaryCalls();
			expect(calls.length).toBeGreaterThan(0);
			for (const call of calls) {
				expect(call[0]).toMatchObject({ provider: "faux", id: "aux-model" });
			}
			expect(
				warnSpy.mock.calls.some(([message]) =>
					String(message).includes('auxiliaryModel "faux/aux-model" unusable for compaction summary'),
				),
			).toBe(false);
			expect(result.firstKeptEntryId).toBeTruthy();
		} finally {
			warnSpy.mockRestore();
		}
	});

	it("disables thinking on compaction summary calls", async () => {
		const harness = await createCompactionHarness({ sessionReasoning: true });
		harness.session.setThinkingLevel("medium");
		expect(harness.session.thinkingLevel).toBe("medium");
		await compactAfterTwoTurns(harness);

		const calls = summaryCalls();
		expect(calls.length).toBeGreaterThan(0);
		for (const call of calls) {
			expect(call[2]).not.toHaveProperty("reasoning");
		}
	});
});
