import { fauxAssistantMessage, fauxToolCall } from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { describe, expect, it, vi } from "vitest";
import { AgentSessionRuntime, createAgentSessionServices } from "../../src/core/agent-session-runtime.js";
import type { DispatchBinding } from "../../src/core/dispatch/types.js";
import type { CreateRlmSubagentRuntimeOptions } from "../../src/core/rlm-runtime.js";
import { createHarness } from "./harness.js";

const provider = "faux-eng-subagent-default-model";

const remoteBinding: DispatchBinding = {
	version: 2,
	ownsBox: true,
	appId: "app",
	boxId: "box",
	guestRepoDir: "/repo",
	guestCwd: "/repo",
	guestStateDir: "/state",
	guestPython: "/python",
	guestSkillsDir: "/skills",
	hostResourceDir: "/captured",
	sourceHead: "head",
	baselineCommit: "baseline",
	initialBranch: "main",
	inputs: {},
};

describe("subagent default model setting", () => {
	it("prevents host-only native tools from executing in a dispatched session", async () => {
		let hostWrites = 0;
		let kernelCalls = 0;
		const tool = (name: string) => ({
			name,
			label: name,
			description: name,
			parameters: Type.Object({}),
			execute: async () => {
				if (name === "ipython") kernelCalls++;
				else hostWrites++;
				return { content: [{ type: "text" as const, text: "host write" }], details: {} };
			},
		});
		const harness = await createHarness({
			models: [{ id: "parent" }, { id: "other" }],
			tools: [tool("ipython"), tool("host_write")],
			extensionFactories: [
				(pi) => {
					pi.on("user_bash", () => {
						hostWrites++;
					});
					pi.registerTool({
						...tool("ipython"),
						execute: async () => {
							hostWrites++;
							return { content: [{ type: "text", text: "host override" }], details: {} };
						},
					});
				},
			],
			dispatchBinding: remoteBinding,
		});
		try {
			await expect(harness.session.executeBash("printf probe")).rejects.toThrow("ipython bash()");
			await expect(harness.session.runUserBash("printf probe")).rejects.toThrow("ipython bash()");
			harness.session.setActiveToolsByName(["host_write", "ipython"]);
			harness.setResponses([
				fauxAssistantMessage([fauxToolCall("host_write", {}), fauxToolCall("ipython", {})], {
					stopReason: "toolUse",
				}),
				fauxAssistantMessage("done"),
			]);
			await harness.session.prompt("work");
			expect(harness.session.getActiveToolNames()).toEqual(["ipython"]);
			expect(hostWrites).toBe(0);
			expect(kernelCalls).toBe(1);
			await expect(harness.session.setModel(harness.models[1]!)).rejects.toThrow("Sail Flex");
			expect(await harness.session.cycleModel()).toBeUndefined();
			const services = await createAgentSessionServices({
				cwd: harness.tempDir,
				agentDir: harness.tempDir,
				authStorage: harness.authStorage,
				settingsManager: harness.settingsManager,
				resourceLoaderOptions: { noExtensions: true, noSkills: true, noPromptTemplates: true, noThemes: true },
				telemetryDisabled: true,
			});
			const runtime = new AgentSessionRuntime(harness.session, services, async () => {
				throw new Error("unexpected rebuild");
			});
			let disposedReason: string | undefined;
			runtime.setSubagentRuntimeHost({
				createRlmSubagentRuntime: async () => {
					throw new Error("unexpected child");
				},
				deleteRlmSubagentRuntime: async () => {},
				disposeRlmSubagentRuntimes: async (reason) => {
					disposedReason = reason;
				},
			});
			await expect(runtime.newSession()).rejects.toThrow("session replacement is unsupported");
			await expect(runtime.switchSession("missing.jsonl")).rejects.toThrow("session replacement is unsupported");
			await expect(runtime.fork("missing")).rejects.toThrow("session replacement is unsupported");
			await expect(runtime.importFromJsonl("missing.jsonl")).rejects.toThrow("session replacement is unsupported");
			expect(runtime.session).toBe(harness.session);
			await runtime.dispose({ reason: "shutdown" });
			expect(disposedReason).toBe("shutdown");
		} finally {
			harness.cleanup();
		}
	});

	it.each(["dispatch", "spawn"] as const)(
		"admits remote %s and cancels through the existing child handle",
		async (operation) => {
			let started!: (options: CreateRlmSubagentRuntimeOptions) => void;
			const preparing = new Promise<CreateRlmSubagentRuntimeOptions>((resolve) => {
				started = resolve;
			});
			const harness = await createHarness({
				dispatchBinding: remoteBinding,
				api: "sail-responses",
				provider: "sail",
				models: [{ id: "worker" }],
				subagentRuntimeHost: {
					supportsDispatch: true,
					deleteRlmSubagentRuntime: async () => {},
					createRlmSubagentRuntime: async (options) => {
						started(options);
						return new Promise((_resolve, reject) => {
							options.preparationSignal!.addEventListener(
								"abort",
								() => reject(new Error("preparation cancelled")),
								{ once: true },
							);
						});
					},
				},
			});
			try {
				const handle =
					operation === "dispatch"
						? await harness.session.dispatchRlmChild("work", { name: "worker", inputs: { notes: "notes.txt" } })
						: await harness.session.runRlmChild("work", { name: "worker" });
				const options = await preparing;
				expect(handle.model).toBe("sail/worker");
				expect(options.dispatch).toEqual(operation === "dispatch" ? { inputs: { notes: "notes.txt" } } : undefined);
				expect(harness.session.getRlmChildRunStatus(handle.rlm_child_id)).toBe("queued");
				await harness.session.deleteRlmSubagent(handle.rlm_child_id);
				expect(options.preparationSignal!.aborted).toBe(true);
				expect((await harness.session.listRlmSubagents()).subagents).toEqual([]);
			} finally {
				harness.cleanup();
			}
		},
	);

	it("requires the daemon dispatch capability before admitting a child", async () => {
		const harness = await createHarness();
		try {
			await expect(harness.session.dispatchRlmChild("work", { name: "worker" })).rejects.toThrow("daemon runtime");
			expect((await harness.session.listRlmSubagents()).subagents).toEqual([]);
		} finally {
			harness.cleanup();
		}
	});

	it("surfaces a Sail inference failure without automatic retry or backup routing", async () => {
		const harness = await createHarness({
			api: "sail-responses",
			provider: "sail",
			models: [{ id: "worker" }, { id: "backup" }],
			settings: { providerBackupModel: "sail/backup", retry: { enabled: true, maxRetries: 1, baseDelayMs: 0 } },
		});
		try {
			harness.setResponses([
				fauxAssistantMessage("", { stopReason: "error", errorMessage: "Sail poll connection lost" }),
				fauxAssistantMessage("must not run"),
			]);
			await harness.session.prompt("work");
			expect(harness.faux.state.callCount).toBe(1);
			expect(harness.eventsOfType("auto_retry_start")).toEqual([]);
			expect(harness.session.model?.id).toBe("worker");
		} finally {
			harness.cleanup();
		}
	});

	it("resolves unpinned spawns against subagentDefaultModel", { timeout: 30_000 }, async () => {
		const harness = await createHarness({
			provider,
			models: [{ id: "parent-model" }, { id: "child-model" }],
			settings: { subagentDefaultModel: `${provider}/child-model` },
		});
		try {
			harness.setResponses([fauxAssistantMessage("child answer")]);

			const result = await harness.session.runRlmChild("do the work");

			expect(result.model).toBe(`${provider}/child-model`);
			await vi.waitFor(
				async () => {
					const childEntry = (await harness.session.listRlmSubagents()).subagents[0];
					expect(childEntry?.status).toBe("completed");
					expect(harness.session.getRlmChildSession(childEntry!.rlm_child_id)?.model?.id).toBe("child-model");
				},
				{ timeout: 10_000 },
			);
		} finally {
			harness.cleanup();
		}
	});

	it("keeps an explicit spawn model ahead of subagentDefaultModel", { timeout: 30_000 }, async () => {
		const harness = await createHarness({
			provider,
			models: [{ id: "parent-model" }, { id: "child-model" }],
			settings: { subagentDefaultModel: `${provider}/child-model` },
		});
		try {
			harness.setResponses([fauxAssistantMessage("parent answer")]);

			const result = await harness.session.runRlmChild("do the work", { model: `${provider}/parent-model` });

			expect(result.model).toBe(`${provider}/parent-model`);
			await vi.waitFor(
				async () => {
					const childEntry = (await harness.session.listRlmSubagents()).subagents[0];
					expect(childEntry?.status).toBe("completed");
					expect(harness.session.getRlmChildSession(childEntry!.rlm_child_id)?.model?.id).toBe("parent-model");
				},
				{ timeout: 10_000 },
			);
		} finally {
			harness.cleanup();
		}
	});

	it("fails an unpinned spawn when the configured default is unavailable", { timeout: 30_000 }, async () => {
		const harness = await createHarness({
			provider,
			models: [{ id: "parent-model" }],
			settings: { subagentDefaultModel: `${provider}/missing-model` },
		});
		try {
			await expect(harness.session.runRlmChild("do the work")).rejects.toThrow(
				`Requested subagent model "${provider}/missing-model" is unavailable, unauthenticated, or expired`,
			);
			expect((await harness.session.listRlmSubagents()).subagents).toEqual([]);
		} finally {
			harness.cleanup();
		}
	});

	it("fails an unpinned spawn when the configured parent-model default is stale", { timeout: 30_000 }, async () => {
		// A default naming the parent model must pass the same availability and
		// authentication preflight as any other reference; the parent-model
		// equality shortcut would start a child that fails its model request.
		const harness = await createHarness({
			provider,
			models: [{ id: "parent-model" }, { id: "child-model" }],
			settings: { subagentDefaultModel: `${provider}/parent-model` },
		});
		try {
			expect(harness.session.modelRegistry.markProviderAuthStale(provider)).toBe(true);
			expect(harness.session.modelRegistry.markProviderAuthStale(provider)).toBe(true);
			expect(harness.session.modelRegistry.getProviderAuthStatus(provider)).toMatchObject({
				source: "stale",
				label: "expired",
			});
			harness.setResponses([fauxAssistantMessage("child answer")]);

			await expect(harness.session.runRlmChild("do the work")).rejects.toThrow(
				`Requested subagent model "${provider}/parent-model" is unavailable, unauthenticated, or expired`,
			);
			expect((await harness.session.listRlmSubagents()).subagents).toEqual([]);
		} finally {
			harness.cleanup();
		}
	});

	it("inherits the parent model when no default is configured", { timeout: 30_000 }, async () => {
		const harness = await createHarness({
			provider,
			models: [{ id: "parent-model" }, { id: "child-model" }],
		});
		try {
			harness.setResponses([fauxAssistantMessage("parent answer")]);

			const result = await harness.session.runRlmChild("do the work");

			expect(result.model).toBe(`${provider}/parent-model`);
			await vi.waitFor(
				async () => {
					const childEntry = (await harness.session.listRlmSubagents()).subagents[0];
					expect(childEntry?.status).toBe("completed");
				},
				{ timeout: 10_000 },
			);
		} finally {
			harness.cleanup();
		}
	});

	it("treats a non-string subagentDefaultModel setting as unset on unpinned spawns", { timeout: 30_000 }, async () => {
		// Corrupted settings (e.g. 42) must degrade to "unset" in the spawn hot
		// path: the child inherits the parent model instead of throwing.
		const harness = await createHarness({
			provider,
			models: [{ id: "parent-model" }, { id: "child-model" }],
			settings: { subagentDefaultModel: 42 as unknown as string },
		});
		try {
			harness.setResponses([fauxAssistantMessage("parent answer")]);

			const result = await harness.session.runRlmChild("do the work");

			expect(result.model).toBe(`${provider}/parent-model`);
			await vi.waitFor(
				async () => {
					const childEntry = (await harness.session.listRlmSubagents()).subagents[0];
					expect(childEntry?.status).toBe("completed");
				},
				{ timeout: 10_000 },
			);
		} finally {
			harness.cleanup();
		}
	});
});
