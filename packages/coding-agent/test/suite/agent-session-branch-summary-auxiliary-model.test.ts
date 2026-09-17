import type * as PiAi from "@earendil-works/pi-ai";
import { type AssistantMessage, fauxAssistantMessage } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { estimateBranchSummaryRequestTokens } from "../../src/core/compaction/index.js";
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

/** Valid summarizer response; the branch flow only reads its text content. */
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

/** Summary wire calls only: the session's own turns stream through the faux provider. */
function summaryCalls() {
	return completeSimpleMock.mock.calls.filter(
		(call) => (call[1] as { systemPrompt?: string }).systemPrompt === SUMMARIZATION_SYSTEM_PROMPT,
	);
}

/** Session-model context window and branch-summary reserve, as the call site resolves them. */
const SESSION_CONTEXT_WINDOW = 128000;
const BRANCH_SUMMARY_RESERVE_TOKENS = 16384;
/** Completion budget generateBranchSummary requests, and the system prompt it sends. */
const BRANCH_SUMMARY_COMPLETION_BUDGET = 2048;
const SUMMARIZATION_SYSTEM_TOKENS = Math.ceil(SUMMARIZATION_SYSTEM_PROMPT.length / 4);

describe("AgentSession branch summary auxiliary model", () => {
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

	async function createBranchSummaryHarness(
		options: { auxiliaryModel?: string; sessionReasoning?: boolean; auxContextWindow?: number } = {},
	): Promise<Harness> {
		const harness = await createHarness({
			models: [
				{ id: "session-model", name: "Session Model", reasoning: options.sessionReasoning },
				{ id: "aux-model", name: "Aux Model", contextWindow: options.auxContextWindow },
			],
			settings: {
				...(options.auxiliaryModel === undefined ? {} : { auxiliaryModel: options.auxiliaryModel }),
			},
			persistSession: true,
		});
		harnesses.push(harness);
		return harness;
	}

	/** Navigate to the first user entry, which summarizes every turn left behind. */
	async function navigateToRootWithSummary(harness: Harness) {
		const [rootNode] = harness.sessionManager.getTree();
		const result = await harness.session.navigateTree(rootNode.entry.id, { summarize: true });
		expect(result.cancelled).toBe(false);
		return result;
	}

	/** Two short turns, then a summarized navigation away from both. */
	async function summarizeAfterTwoTurns(harness: Harness) {
		harness.setResponses([fauxAssistantMessage("one response"), fauxAssistantMessage("two response")]);
		await harness.session.prompt("one");
		await harness.session.prompt("two");
		return await navigateToRootWithSummary(harness);
	}

	/** ~13k characters per turn, so the summary request outgrows a 8192-token window. */
	function longTurnText(): string {
		return "long branch turn text ".repeat(600);
	}

	/**
	 * ~112k characters -> ~28k prompt tokens: one reply this long lands a 33000
	 * token auxiliary window inside the fit band (below).
	 */
	function longReplyText(): string {
		return "long branch reply text ".repeat(5090);
	}

	/**
	 * Three long turns, then a summarized navigation away from all of them. The
	 * branch is long enough that the summary request outgrows a 8192-token
	 * auxiliary window, which the two routing cases below both rely on.
	 */
	async function summarizeLongBranch(harness: Harness) {
		const longText = longTurnText();
		harness.setResponses([
			fauxAssistantMessage(longText),
			fauxAssistantMessage(longText),
			fauxAssistantMessage(longText),
		]);
		await harness.session.prompt(longText);
		await harness.session.prompt(longText);
		await harness.session.prompt(longText);
		expect(sessionSummaryRequestTokens(harness)).toBeGreaterThan(8192);
		return await navigateToRootWithSummary(harness);
	}

	/** The request the session model would issue for the branch left behind. */
	function sessionSummaryRequestTokens(harness: Harness): number {
		return estimateBranchSummaryRequestTokens(harness.sessionManager.getEntries(), {
			contextWindow: SESSION_CONTEXT_WINDOW,
			reserveTokens: BRANCH_SUMMARY_RESERVE_TOKENS,
		});
	}

	it("routes branch summaries to the configured auxiliary model", async () => {
		const harness = await createBranchSummaryHarness({ auxiliaryModel: "faux/aux-model" });
		const result = await summarizeAfterTwoTurns(harness);

		const calls = summaryCalls();
		expect(calls.length).toBeGreaterThan(0);
		for (const call of calls) {
			expect(call[0]).toMatchObject({ provider: "faux", id: "aux-model" });
		}
		// The summary still lands as a branch_summary entry, so routing the call did
		// not change tree-navigation behavior.
		expect(result.summaryEntry?.type).toBe("branch_summary");
		const entry = harness.sessionManager.getEntries().find((candidate) => candidate.type === "branch_summary");
		expect(entry).toMatchObject({
			type: "branch_summary",
			summary: expect.stringContaining("Test summary"),
			fromHook: false,
		});
	}, 60000);

	it("falls back to the session model when no auxiliary model is configured", async () => {
		const harness = await createBranchSummaryHarness();
		await summarizeAfterTwoTurns(harness);

		const calls = summaryCalls();
		expect(calls.length).toBeGreaterThan(0);
		for (const call of calls) {
			expect(call[0]).toMatchObject({ provider: "faux", id: "session-model" });
		}
	}, 60000);

	it("falls back to the session model when the auxiliary model is unusable", async () => {
		const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
		try {
			const harness = await createBranchSummaryHarness({ auxiliaryModel: "faux/missing-model" });
			await summarizeAfterTwoTurns(harness);

			const calls = summaryCalls();
			expect(calls.length).toBeGreaterThan(0);
			for (const call of calls) {
				expect(call[0]).toMatchObject({ provider: "faux", id: "session-model" });
			}
			expect(warnSpy).toHaveBeenCalledTimes(1);
			const [message] = warnSpy.mock.calls[0];
			expect(message).toContain('auxiliaryModel "faux/missing-model" unusable for branch summary');
			// Caught error details can embed credential material, so they must not be logged.
			expect(message).not.toContain("unavailable, unauthenticated, or expired");
		} finally {
			warnSpy.mockRestore();
		}
	}, 60000);

	it("falls back to the session model when the auxiliary model cannot fit the branch summary request", async () => {
		const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
		try {
			const harness = await createBranchSummaryHarness({
				auxiliaryModel: "faux/aux-model",
				auxContextWindow: 8192,
			});
			// The long branch serializes into a request larger than the 8192-token
			// auxiliary window, so routing the summary there would fail over-limit and
			// leave the navigation stranded; the session model must run it instead.
			const result = await summarizeLongBranch(harness);

			const calls = summaryCalls();
			expect(calls.length).toBeGreaterThan(0);
			for (const call of calls) {
				expect(call[0]).toMatchObject({ provider: "faux", id: "session-model" });
			}
			expect(warnSpy).toHaveBeenCalledTimes(1);
			const [message] = warnSpy.mock.calls[0];
			expect(message).toContain('auxiliaryModel "faux/aux-model" unusable for branch summary');
			// The summary still lands, so the fallback kept tree navigation working.
			expect(result.summaryEntry?.type).toBe("branch_summary");
		} finally {
			warnSpy.mockRestore();
		}
	}, 60000);

	it("keeps the auxiliary model when its context window fits the branch summary request", async () => {
		const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
		try {
			const harness = await createBranchSummaryHarness({
				auxiliaryModel: "faux/aux-model",
				auxContextWindow: 131072,
			});
			// Same branch as the "too small" case, so the two outcomes are comparable:
			// here the auxiliary window holds the request and routing stays put.
			const result = await summarizeLongBranch(harness);

			const calls = summaryCalls();
			expect(calls.length).toBeGreaterThan(0);
			for (const call of calls) {
				expect(call[0]).toMatchObject({ provider: "faux", id: "aux-model" });
			}
			expect(
				warnSpy.mock.calls.some(([message]) =>
					String(message).includes('auxiliaryModel "faux/aux-model" unusable for branch summary'),
				),
			).toBe(false);
			expect(result.summaryEntry?.type).toBe("branch_summary");
		} finally {
			warnSpy.mockRestore();
		}
	}, 60000);

	it("falls back to the session model when the auxiliary window holds the request but not the input slice and its reserve", async () => {
		const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
		try {
			const harness = await createBranchSummaryHarness({
				auxiliaryModel: "faux/aux-model",
				auxContextWindow: 33000,
			});
			harness.setResponses([fauxAssistantMessage(longReplyText())]);
			await harness.session.prompt("short prompt");

			// The dangerous band: a 33000-token window accepts the request body the
			// session model would send, but not the input slice plus the reserve the
			// branch call subtracts. Routing there shrinks the slice until nothing
			// survives and writes the "No content to summarize" stub instead.
			const entries = harness.sessionManager.getEntries();
			const requestSize = estimateBranchSummaryRequestTokens(entries, {
				contextWindow: SESSION_CONTEXT_WINDOW,
				reserveTokens: 0,
			});
			const promptTokens = requestSize - SUMMARIZATION_SYSTEM_TOKENS - BRANCH_SUMMARY_COMPLETION_BUDGET;
			expect(requestSize).toBeLessThan(33000);
			expect(promptTokens + BRANCH_SUMMARY_RESERVE_TOKENS).toBeGreaterThan(33000);

			const result = await navigateToRootWithSummary(harness);

			const calls = summaryCalls();
			expect(calls.length).toBeGreaterThan(0);
			for (const call of calls) {
				expect(call[0]).toMatchObject({ provider: "faux", id: "session-model" });
			}
			expect(warnSpy).toHaveBeenCalledTimes(1);
			const [message] = warnSpy.mock.calls[0];
			expect(message).toContain('auxiliaryModel "faux/aux-model" unusable for branch summary');
			// The real summary lands: the aux budget would have kept no entry at all.
			expect(result.summaryEntry?.summary).not.toContain("No content to summarize");
			expect(result.summaryEntry?.summary).toContain("Test summary");
		} finally {
			warnSpy.mockRestore();
		}
	}, 60000);

	it("never requests thinking on branch summary calls", async () => {
		// Branch summaries are transcription, not reasoning: no reasoning option is
		// ever passed, even when the session model reasons and a level is selected.
		const harness = await createBranchSummaryHarness({ sessionReasoning: true });
		harness.session.setThinkingLevel("medium");
		expect(harness.session.thinkingLevel).toBe("medium");
		await summarizeAfterTwoTurns(harness);

		const calls = summaryCalls();
		expect(calls.length).toBeGreaterThan(0);
		for (const call of calls) {
			expect(call[2]).not.toHaveProperty("reasoning");
		}
	}, 60000);
});

describe("estimateBranchSummaryRequestTokens", () => {
	const harnesses: Harness[] = [];

	afterEach(() => {
		for (const harness of harnesses.splice(0)) harness.cleanup();
	});

	/** Two short turns; the branch the estimator would size on navigation. */
	async function twoTurnEntries() {
		const harness = await createHarness({
			models: [{ id: "session-model", name: "Session Model" }],
			persistSession: true,
		});
		harnesses.push(harness);
		harness.setResponses([fauxAssistantMessage("one response"), fauxAssistantMessage("two response")]);
		await harness.session.prompt("one");
		await harness.session.prompt("two");
		return { harness, entries: harness.sessionManager.getEntries() };
	}

	it("grows with the conversation and counts the completion budget", async () => {
		const { harness, entries } = await twoTurnEntries();
		const estimate = estimateBranchSummaryRequestTokens(entries, {
			contextWindow: SESSION_CONTEXT_WINDOW,
			reserveTokens: BRANCH_SUMMARY_RESERVE_TOKENS,
		});

		// The request must at least carry the summarizer system prompt and the
		// 2048-token completion budget before any conversation text is added.
		expect(estimate).toBeGreaterThan(Math.ceil(SUMMARIZATION_SYSTEM_PROMPT.length / 4) + 2048);

		harness.setResponses([
			fauxAssistantMessage(longTurnText()),
			fauxAssistantMessage(longTurnText()),
			fauxAssistantMessage(longTurnText()),
		]);
		const longText = longTurnText();
		await harness.session.prompt(longText);
		await harness.session.prompt(longText);
		await harness.session.prompt(longText);

		const grown = estimateBranchSummaryRequestTokens(harness.sessionManager.getEntries(), {
			contextWindow: SESSION_CONTEXT_WINDOW,
			reserveTokens: BRANCH_SUMMARY_RESERVE_TOKENS,
		});
		expect(grown).toBeGreaterThan(estimate);
		// Intent of the too-small routing case: this branch genuinely needs more
		// than a 8192-token auxiliary window.
		expect(grown).toBeGreaterThan(8192);
	}, 60000);

	it("returns 0 when no model-visible entry remains", () => {
		expect(estimateBranchSummaryRequestTokens([], { contextWindow: SESSION_CONTEXT_WINDOW })).toBe(0);
		// A branch the summarizer filters out entirely issues no wire call, so it
		// must not enlarge the required window.
		const invisible = [
			{
				type: "model_change" as const,
				id: "m1",
				parentId: null,
				timestamp: new Date().toISOString(),
				provider: "faux",
				modelId: "session-model",
			},
		];
		expect(estimateBranchSummaryRequestTokens(invisible, { contextWindow: SESSION_CONTEXT_WINDOW })).toBe(0);
	});

	it("counts the instructions the call site forwards", async () => {
		const { entries } = await twoTurnEntries();
		const base = estimateBranchSummaryRequestTokens(entries, {
			contextWindow: SESSION_CONTEXT_WINDOW,
			reserveTokens: BRANCH_SUMMARY_RESERVE_TOKENS,
		});
		const focus = "focus on the parser refactor ".repeat(40);
		expect(
			estimateBranchSummaryRequestTokens(entries, {
				contextWindow: SESSION_CONTEXT_WINDOW,
				reserveTokens: BRANCH_SUMMARY_RESERVE_TOKENS,
				customInstructions: focus,
			}),
		).toBeGreaterThan(base);
		// The replacement form swaps the default prompt for the custom text, so a
		// longer replacement must grow the request too.
		const replacement = "replacement instructions ".repeat(120);
		expect(
			estimateBranchSummaryRequestTokens(entries, {
				contextWindow: SESSION_CONTEXT_WINDOW,
				reserveTokens: BRANCH_SUMMARY_RESERVE_TOKENS,
				customInstructions: replacement,
				replaceInstructions: true,
			}),
		).toBeGreaterThan(base);
	}, 60000);

	function longTurnText(): string {
		return "long branch turn text ".repeat(600);
	}
});
