/**
 * Image turns on a text-only session model: one bounded child pinned to the
 * image model reads the images, and only its text reaches the session.
 */

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Agent } from "@earendil-works/pi-agent-core";
import type { AssistantMessage, AssistantMessageEvent, ImageContent } from "@earendil-works/pi-ai";
import { EventStream } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AgentSession } from "../src/core/agent-session.js";
import { AuthStorage } from "../src/core/auth-storage.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import { SessionManager } from "../src/core/session-manager.js";
import { SettingsManager } from "../src/core/settings-manager.js";
import { getCodingAgentFixtureModel } from "./fixture-models.js";
import { assistantMsg, createTestResourceLoader } from "./utilities.js";

const IMAGE: ImageContent = { type: "image", mimeType: "image/png", data: "aGk=" };
const SET = { imageModel: "claude-haiku-4-5" };
const VISION = "anthropic/claude-haiku-4-5";

type ChildSessionStub = { session: { getLastAssistantText: () => string | undefined; dispose: () => void } };

interface ImageTurnHarness {
	session: AgentSession;
	servedModelIds: string[];
	requests: Array<{ model: string; content: unknown[] }[]>;
	spawns: Array<{ prompt: string; kwargs: Record<string, unknown> }>;
	spawnChild: (options?: { reading?: string; settled?: boolean; status?: string; error?: string }) => void;
	dispose: () => void;
}

/**
 * A session whose model cannot see images, with a spied child runtime so the
 * spawn shape is observable without a real child process.
 */
function createImageTurnHarness(
	settings: Record<string, unknown>,
	options: { vision?: boolean } = {},
): ImageTurnHarness {
	const dir = mkdtempSync(join(tmpdir(), "pi-image-turn-child-"));
	writeFileSync(join(dir, "settings.json"), JSON.stringify(settings));
	const base = getCodingAgentFixtureModel("anthropic", "claude-opus-4-7");
	const sessionModel = (
		options.vision ? base : { ...base, id: "claude-opus-4-7-text-only", input: ["text"] }
	) as typeof base;
	const servedModelIds: string[] = [];
	const requests: Array<{ model: string; content: unknown[] }[]> = [];
	const agent = new Agent({
		getApiKey: () => "test-key",
		initialState: { model: sessionModel, systemPrompt: "Test", tools: [] },
		streamFn: (model, context) => {
			servedModelIds.push(model.id);
			requests.push(
				context.messages.map((message) => ({ model: model.id, content: [...(message.content as unknown[])] })),
			);
			const stream = new EventStream<AssistantMessageEvent, AssistantMessage>(
				(event) => event.type === "done",
				(event: any) => event.message,
			);
			stream.push({ type: "done", reason: "stop", message: assistantMsg("ok") });
			return stream;
		},
	});
	const auth = AuthStorage.create(join(dir, "auth.json"));
	auth.setRuntimeApiKey("anthropic", "test-key");
	auth.setRuntimeApiKey("deepseek", "test-key");
	const session = new AgentSession({
		agent,
		sessionManager: SessionManager.inMemory(),
		settingsManager: SettingsManager.create(dir, dir),
		cwd: dir,
		modelRegistry: ModelRegistry.create(auth, dir),
		resourceLoader: createTestResourceLoader(),
	});
	const spawns: ImageTurnHarness["spawns"] = [];
	const children = session as unknown as {
		runRlmChild: (prompt: string, kwargs: Record<string, unknown>) => Promise<{ rlm_child_id: string }>;
		collectRlmChildren: (targets: string[], timeoutMs: number) => Promise<{ results: unknown[] }>;
		deleteRlmSubagent: (target: string) => Promise<unknown>;
		_rlmChildSessions: Map<string, ChildSessionStub>;
	};
	const harness: ImageTurnHarness = {
		session,
		servedModelIds,
		requests,
		spawns,
		spawnChild: (options = {}) => {
			children.runRlmChild = vi.fn(async (prompt: string, kwargs: Record<string, unknown>) => {
				spawns.push({ prompt, kwargs });
				const id = `child-${spawns.length}`;
				children._rlmChildSessions.set(id, {
					session: {
						getLastAssistantText: () => options.reading ?? "A tall bridge at sunset.",
						dispose: () => {},
					},
				});
				children.collectRlmChildren = vi.fn(async () => ({
					results: [
						{
							rlm_child_id: id,
							status: options.status ?? "done",
							settled: options.settled ?? true,
							error: options.error,
						},
					],
				}));
				children.deleteRlmSubagent = vi.fn(async () => ({}));
				return { rlm_child_id: id };
			});
		},
		dispose: () => {
			session.dispose();
			rmSync(dir, { recursive: true, force: true });
		},
	};
	return harness;
}

const harnesses: ImageTurnHarness[] = [];
function harnessFor(settings: Record<string, unknown>, options: { vision?: boolean } = {}): ImageTurnHarness {
	const harness = createImageTurnHarness(settings, options);
	harnesses.push(harness);
	return harness;
}

afterEach(() => {
	for (const harness of harnesses.splice(0)) harness.dispose();
});

describe("image turns on a text-only session model", () => {
	it("spawns one bounded child pinned to the image model and keeps only its text", async () => {
		const harness = harnessFor(SET);
		harness.spawnChild({ reading: "A dashboard with a red error banner." });
		const { session } = harness;
		await session.prompt("Earlier unrelated work.", { images: undefined });
		await session.prompt("What does this screenshot show?", { images: [IMAGE] });

		expect(harness.spawns).toHaveLength(1);
		expect(harness.spawns[0]?.kwargs).toEqual({ model: VISION });
		// The child gets the materialized image and the user's question, never the
		// transcript that the in-place routed turn used to re-read.
		expect(harness.spawns[0]?.prompt).toContain("What does this screenshot show?");
		expect(harness.spawns[0]?.prompt).toMatch(/image-1\.png/);
		expect(harness.spawns[0]?.prompt).not.toContain("Earlier unrelated work.");
		// The session model serves the turn, so no model change is recorded.
		expect(harness.servedModelIds).toEqual(["claude-opus-4-7-text-only", "claude-opus-4-7-text-only"]);
		expect(session.sessionManager.getEntries().filter((entry) => entry.type === "model_change")).toHaveLength(0);

		const userMessage = session.messages.at(-2) as { content: Array<{ type: string; text?: string }> };
		const content = userMessage.content;
		expect(content.some((block) => block.type === "image")).toBe(false);
		expect(content.map((block) => block.text ?? "").join("\n")).toContain("A dashboard with a red error banner.");
		// The provider request carries the reading, not the image payload.
		const lastRequest = harness.requests.at(-1) ?? [];
		expect(
			lastRequest.some((message) => message.content.some((block) => (block as { type: string }).type === "image")),
		).toBe(false);
		expect(
			lastRequest.some((message) =>
				JSON.stringify(message.content).includes("A dashboard with a red error banner."),
			),
		).toBe(true);
	});

	it("leaves the actionable settings error in place when no image model resolves", async () => {
		const harness = harnessFor({});
		harness.spawnChild();
		await expect(harness.session.prompt("look", { images: [IMAGE] })).rejects.toThrow(/does not accept image input/);
		expect(harness.spawns).toHaveLength(0);
	});

	it("refuses an unusable pinned model before any image leaves the process", async () => {
		const harness = harnessFor({ imageModel: "openai/gpt-5.4" });
		harness.spawnChild();
		await expect(harness.session.prompt("look", { images: [IMAGE] })).rejects.toThrow(/could not be resolved/);
		expect(harness.spawns).toHaveLength(0);
	});

	it("spawns nothing when images are blocked", async () => {
		const harness = harnessFor({ ...SET, images: { blockImages: true } });
		harness.spawnChild();
		await harness.session.prompt("look", { images: [IMAGE] });
		expect(harness.spawns).toHaveLength(0);
		expect(harness.servedModelIds).toEqual(["claude-opus-4-7-text-only"]);
	});

	it("keeps one child for every image in the turn", async () => {
		const harness = harnessFor(SET);
		harness.spawnChild({ reading: "Two panels." });
		await harness.session.prompt("compare these", { images: [IMAGE, { ...IMAGE, mimeType: "image/jpeg" }] });
		expect(harness.spawns).toHaveLength(1);
		expect(harness.spawns[0]?.prompt).toMatch(/image-1\.png/);
		expect(harness.spawns[0]?.prompt).toMatch(/image-2\.jpeg/);
		expect(harness.spawns[0]?.prompt).toContain("2 images");
	});

	it("fails the turn with the setting named when the child never settles", async () => {
		const harness = harnessFor(SET);
		harness.spawnChild({ settled: false });
		await expect(harness.session.prompt("look", { images: [IMAGE] })).rejects.toThrow(
			/did not finish reading the attached image\(s\).*image-model/s,
		);
		expect(harness.servedModelIds).toEqual([]);
	});
});

/** Content blocks of every user message the session recorded. */
function userContents(session: AgentSession): Array<Array<{ type: string; text?: string }>> {
	return session.messages.flatMap((message) => {
		const entry = message as { role?: string; content?: Array<{ type: string; text?: string }> };
		return entry.role === "user" && Array.isArray(entry.content) ? [entry.content] : [];
	});
}

function contentText(content: Array<{ type: string; text?: string }>): string {
	return content.map((block) => block.text ?? "").join("\n");
}

describe("image steers and follow-ups", () => {
	it("queues a steered image inline on a vision-capable session model", async () => {
		const harness = harnessFor(SET, { vision: true });
		harness.spawnChild();
		await harness.session.steer("look at this", [IMAGE]);
		await harness.session.prompt("continue");
		expect(harness.spawns).toHaveLength(0);
		const steered = userContents(harness.session).find((content) => contentText(content).includes("look at this"));
		expect(steered?.some((block) => block.type === "image")).toBe(true);
	});

	it("reads steered images with the child on a text-only session model", async () => {
		const harness = harnessFor(SET);
		harness.spawnChild({ reading: "A red error banner." });
		await harness.session.steer("look at this", [IMAGE]);
		await harness.session.prompt("continue");
		expect(harness.spawns).toHaveLength(1);
		const steered = userContents(harness.session).find((content) => contentText(content).includes("look at this"));
		expect(steered?.some((block) => block.type === "image")).toBe(false);
		expect(contentText(steered ?? [])).toContain("A red error banner.");
	});

	it("reads follow-up images with the child on a text-only session model", async () => {
		const harness = harnessFor(SET);
		harness.spawnChild({ reading: "A chart trending up." });
		await harness.session.followUp("look at this", [IMAGE]);
		await harness.session.prompt("continue");
		expect(harness.spawns).toHaveLength(1);
		const queued = userContents(harness.session).find((content) => contentText(content).includes("look at this"));
		expect(queued?.some((block) => block.type === "image")).toBe(false);
		expect(contentText(queued ?? [])).toContain("A chart trending up.");
	});

	it("keeps a follow-up image inline on a vision-capable session model", async () => {
		const harness = harnessFor(SET, { vision: true });
		harness.spawnChild();
		await harness.session.followUp("look at this", [IMAGE]);
		await harness.session.prompt("continue");
		expect(harness.spawns).toHaveLength(0);
		const queued = userContents(harness.session).find((content) => contentText(content).includes("look at this"));
		expect(queued?.some((block) => block.type === "image")).toBe(true);
	});
});
