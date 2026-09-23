import { mkdtemp, rm } from "node:fs/promises";
import { createServer, type ServerResponse } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fauxAssistantMessage, fauxToolCall } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it, vi } from "vitest";
import { hasDispatchKernels } from "../src/core/dispatch/kernel.js";
import type { DispatchBinding } from "../src/core/dispatch/types.js";
import {
	inheritDispatchWorkspace,
	sleepDispatchWorkspace,
	terminateDispatchWorkspace,
} from "../src/core/dispatch/workspace.js";
import { IpythonKernelProvisioner } from "../src/core/tools/ipython.js";
import { createHarness } from "./suite/harness.js";

async function fixture() {
	const dir = await mkdtemp(join(tmpdir(), "sail-relay-"));
	const executions = new Map<
		string,
		{
			output: ServerResponse;
			buffered: string;
			offset: number;
			sequence: number;
			activeId: string;
			hostSequence: number;
		}
	>();
	let rejectCancel = false;
	let waitResponse: ServerResponse | undefined;
	let waitStarted!: () => void;
	const waiting = new Promise<void>((resolve) => {
		waitStarted = resolve;
	});
	const commands: Record<string, unknown>[] = [];
	const frames: Record<string, unknown>[] = [];
	const writes: { offset: number; data: Buffer }[] = [];
	const cancellations: string[] = [];
	const boxActions: string[] = [];
	const waitBodyLengths: number[] = [];
	let disconnected!: () => void;
	const disconnect = new Promise<void>((resolve) => {
		disconnected = resolve;
	});
	const event = (exec: NonNullable<ReturnType<typeof executions.get>>, value: object) =>
		exec.output.write(`${JSON.stringify(value)}\n`);
	const protocol = (exec: NonNullable<ReturnType<typeof executions.get>>, value: object) => {
		const bytes = Buffer.from(`${JSON.stringify(value)}\n`);
		const middle = Math.floor(bytes.length / 2);
		for (const chunk of [bytes.subarray(0, middle), bytes.subarray(middle)])
			event(exec, { type: "stdout", seq: exec.sequence++, data: chunk.toString("base64") });
	};
	const done = (exec: NonNullable<ReturnType<typeof executions.get>>, id: unknown, extra: object = {}) =>
		protocol(exec, { event: "done", id, status: "ok", ...extra });
	const server = createServer(async (request, response) => {
		const chunks = [];
		for await (const chunk of request) chunks.push(Buffer.from(chunk));
		const data = Buffer.concat(chunks);
		const url = new URL(request.url!, "http://localhost");
		if (url.pathname.endsWith("/exec")) {
			const command = JSON.parse(data.toString()) as Record<string, unknown>;
			commands.push(command);
			if (!command.open_stdin) {
				response
					.writeHead(200, { "Content-Type": "application/x-ndjson" })
					.end(`${JSON.stringify({ type: "exit", return_code: 0, status: "succeeded" })}\n`);
				return;
			}
			const exec = { output: response, offset: 0, buffered: "", sequence: 0, activeId: "", hostSequence: 0 };
			executions.set(`exec-${commands.length}`, exec);
			response.writeHead(200, { "Content-Type": "application/x-ndjson" });
			event(exec, { type: "started", exec_request_id: `exec-${commands.length}` });
			protocol(exec, { event: "ready", protocol: 3 });
			response.once("close", disconnected);
			return;
		}
		if (url.pathname.endsWith("/sleep") || url.pathname.endsWith("/terminate")) {
			boxActions.push(url.pathname);
			response.writeHead(200, { "Content-Type": "application/json" }).end("{}");
			return;
		}
		const exec = executions.get(url.searchParams.get("exec_request_id")!)!;
		if (url.pathname.endsWith("/cancel")) {
			cancellations.push(url.searchParams.get("exec_request_id")!);
			response.writeHead(rejectCancel ? 503 : 200, { "Content-Type": "application/json" });
			response.end("{}");
			if (!rejectCancel) {
				event(exec, { type: "exit", return_code: 0, status: "cancelled" });
				exec.output.end();
			}
			return;
		}
		if (url.pathname.endsWith("/wait")) {
			waitBodyLengths.push(data.length);
			waitResponse = response;
			waitStarted();
			return;
		}
		if (!url.pathname.endsWith("/stdin")) {
			response.writeHead(404).end();
			return;
		}
		writes.push({ offset: Number(url.searchParams.get("offset")), data });
		exec.offset += data.length;
		response
			.writeHead(200, { "Content-Type": "application/json" })
			.end(JSON.stringify({ accepted_through: exec.offset }));
		exec.buffered += data.toString();
		let newline = exec.buffered.indexOf("\n");
		while (newline >= 0) {
			const frame = JSON.parse(exec.buffered.slice(0, newline)) as Record<string, unknown>;
			exec.buffered = exec.buffered.slice(newline + 1);
			frames.push(frame);
			newline = exec.buffered.indexOf("\n");
			if (frame.type === "execute" && frame.code === "needs-host") {
				exec.activeId = String(frame.id);
				protocol(exec, {
					event: "host_request",
					id: `host-${++exec.hostSequence}`,
					data: { type: "rlm.run", prompt: "remote child" },
				});
			} else if (frame.type === "host_reply") {
				protocol(exec, { event: "result", id: exec.activeId, text: "λ remote result" });
				done(exec, exec.activeId);
			} else if (frame.type === "execute" && frame.code === "drop-transport") {
				exec.output.destroy();
			} else if (frame.type === "restore") done(exec, frame.id, { restored: ["counter"], failed: [] });
			else if (frame.type === "snapshot") done(exec, frame.id, { saved: ["counter"], skipped: [], bytes: 12 });
			else if (frame.type === "shutdown") {
				done(exec, frame.id);
				event(exec, { type: "exit", return_code: 0, status: "completed" });
				exec.output.end();
			} else done(exec, frame.id);
		}
	});
	await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
	const address = server.address();
	if (!address || typeof address === "string") throw new Error("Missing fixture address");
	vi.stubEnv("SAIL_API_KEY", "fixture-key");
	vi.stubEnv("GH_TOKEN", "fixture-gh-token");
	vi.stubEnv("GITHUB_TOKEN", "fixture-github-token");
	vi.stubEnv("PRIME_AGENT_SAILBOX_URL", `http://127.0.0.1:${address.port}/v1`);
	const binding: DispatchBinding = {
		version: 2,
		ownsBox: true,
		appId: "test-app",
		boxId: dir,
		guestRepoDir: "/state/repo",
		guestCwd: "/state/repo/subdir",
		guestStateDir: "/state/session",
		guestPython: "/state/venv/bin/python",
		guestSkillsDir: "/state/skills",
		hostResourceDir: dir,
		sourceHead: "abc",
		baselineCommit: "def",
		initialBranch: "main",
		inputs: {},
	};
	return {
		dir,
		binding,
		frames,
		commands,
		writes,
		cancellations,
		boxActions,
		waitBodyLengths,
		waiting,
		finishWait() {
			waitResponse
				?.writeHead(200, { "Content-Type": "application/json" })
				.end(JSON.stringify({ status: "succeeded" }));
			waitResponse = undefined;
		},
		disconnect,
		setRejectCancel(value: boolean) {
			rejectCancel = value;
		},
		async close() {
			for (const exec of executions.values()) exec.output.destroy();
			server.closeAllConnections();
			await new Promise<void>((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())));
			await rm(dir, { recursive: true, force: true });
		},
	};
}

describe("Sail kernel relay", () => {
	afterEach(() => {
		vi.unstubAllEnvs();
	});
	it("runs the unchanged REPL protocol through a real relay, including host replies and guest snapshots", async () => {
		const f = await fixture();
		const hostRequests: Record<string, unknown>[] = [];
		const provisioner = new IpythonKernelProvisioner(f.dir, {
			dispatchBinding: f.binding,
			pythonSkills: [
				{
					name: "project-skill",
					importName: "project_skill",
					packagePath: join(f.dir, "skills", "project-skill"),
					pyprojectPath: join(f.dir, "skills", "project-skill", "pyproject.toml"),
				},
			],
			env: {
				SAIL_API_KEY: "never-forward",
				PRIME_AGENT_KERNEL_OWNER_PID: "123",
				RLM_SESSION_DIR: "/host/session",
				GH_TOKEN: "explicit-guest-token",
			},
			hostHandlers: {
				"rlm.run": async (payload) => {
					hostRequests.push(payload);
					return { result: "child result" };
				},
			},
		});
		try {
			const manager = await provisioner.ensure();
			expect(provisioner.lastRestore?.restored).toEqual(["counter"]);
			const result = await manager.execute("needs-host");
			expect(result).toMatchObject({ status: "ok", result: "λ remote result" });
			expect(hostRequests).toEqual([{ type: "rlm.run", prompt: "remote child", cellSourceCode: "needs-host" }]);
			expect(f.frames).toContainEqual({
				type: "host_reply",
				id: "host-1",
				data: { status: "ok", result: { result: "child result" } },
			});
			await provisioner.dispose();
			expect(f.commands).toHaveLength(2);
			expect(f.commands[0].command).toBe(
				"'/state/venv/bin/python' -m pip install --disable-pip-version-check -q '/state/repo/subdir/skills/project-skill'",
			);
			expect(f.commands[1]).toMatchObject({
				command: "exec '/state/venv/bin/python' -u -m rlm.repl",
				cwd: "/state/repo/subdir",
				open_stdin: true,
				env: { GH_TOKEN: "explicit-guest-token", RLM_SESSION_DIR: "/state/session" },
			});
			expect(f.commands[1].env).not.toHaveProperty("SAIL_API_KEY");
			expect(f.commands[1].env).not.toHaveProperty("PRIME_AGENT_KERNEL_OWNER_PID");
			expect(f.frames).toEqual(
				expect.arrayContaining([
					expect.objectContaining({
						type: "snapshot",
						path: "/state/session/kernel-state.dill",
						manifest_path: "/state/session/kernel-state.json",
					}),
					expect.objectContaining({ type: "restore", path: "/state/session/kernel-state.dill" }),
				]),
			);
			let offset = 0;
			for (const write of f.writes) {
				expect(write.offset).toBe(offset);
				offset += write.data.length;
			}
			expect(f.cancellations).toEqual([]);
		} finally {
			await provisioner.dispose().catch(() => undefined);
			await f.close();
		}
	});

	it("isolates kernels and cancellation while children share the owner's box and workspace", async () => {
		const f = await fixture();
		const binding = await inheritDispatchWorkspace(f.binding, join(f.dir, "child"));
		const calls: string[] = [];
		const make = (b: DispatchBinding) =>
			new IpythonKernelProvisioner(f.dir, {
				dispatchBinding: b,
				hostHandlers: {
					"rlm.run": async () => {
						calls.push(b.guestStateDir);
						return {};
					},
				},
			});
		const owner = make(f.binding),
			child = make(binding);
		try {
			expect(binding.guestStateDir).not.toBe(f.binding.guestStateDir);
			const parentKernel = await owner.ensure(),
				childKernel = await child.ensure();
			await Promise.all([parentKernel.execute("needs-host"), childKernel.execute("needs-host")]);
			expect(calls.sort()).toEqual([binding.guestStateDir, f.binding.guestStateDir].sort());
			expect(f.commands.map((c) => c.cwd)).toEqual([f.binding.guestCwd, f.binding.guestCwd]);
			expect(f.frames.filter((frame) => frame.type === "restore").map((frame) => frame.path)).toEqual([
				`${f.binding.guestStateDir}/kernel-state.dill`,
				`${binding.guestStateDir}/kernel-state.dill`,
			]);
			await sleepDispatchWorkspace(binding);
			await terminateDispatchWorkspace(binding);
			expect(f.boxActions).toEqual([]);
			const stopped = child.kill();
			await f.waiting;
			f.finishWait();
			await stopped;
			expect(f.cancellations).toEqual(["exec-2"]);
			expect(hasDispatchKernels(f.binding.boxId)).toBe(true);
			expect((await parentKernel.execute("needs-host")).status).toBe("ok");
			await owner.dispose();
			expect(hasDispatchKernels(f.binding.boxId)).toBe(false);
		} finally {
			f.finishWait();
			await Promise.allSettled([owner.dispose(), child.dispose()]);
			await f.close();
		}
	});

	it("waits for verified cancellation before an explicit kernel replacement", async () => {
		const f = await fixture();
		const provisioner = new IpythonKernelProvisioner(f.dir, { dispatchBinding: f.binding });
		try {
			await provisioner.ensure();
			let killed = false;
			const stopped = provisioner.kill().then(() => {
				killed = true;
			});
			await f.waiting;
			expect(f.waitBodyLengths).toEqual([0]);
			expect(killed).toBe(false);
			await expect(provisioner.ensure()).rejects.toThrow("transport terminated");
			expect(f.commands).toHaveLength(1);
			f.finishWait();
			await stopped;
			const replacement = await provisioner.ensure();
			expect(replacement.isRunning).toBe(true);
			expect(f.commands).toHaveLength(2);
			expect(f.cancellations).toEqual(["exec-1"]);
		} finally {
			f.finishWait();
			await provisioner.dispose().catch(() => undefined);
			await f.close();
		}
	});

	it("keeps failed remote session disposal visible on a repeated delete", async () => {
		const f = await fixture();
		f.setRejectCancel(true);
		const harness = await createHarness({ dispatchBinding: { ...f.binding, ownsBox: false } });
		try {
			harness.setResponses([
				fauxAssistantMessage([fauxToolCall("ipython", { code: "drop-transport" })], { stopReason: "toolUse" }),
				fauxAssistantMessage("done"),
			]);
			await harness.session.prompt("work");
			await f.disconnect;
			await expect(harness.session.disposeAsync({ kernelSnapshot: false })).rejects.toThrow("503");
			await expect(harness.session.disposeAsync({ kernelSnapshot: false })).rejects.toThrow("503");
			expect(hasDispatchKernels(f.binding.boxId)).toBe(true);
		} finally {
			harness.cleanup();
			await f.close();
		}
	});

	it("latches transport loss and a failed remote cancellation without starting or replaying another kernel", async () => {
		const f = await fixture();
		f.setRejectCancel(true);
		const provisioner = new IpythonKernelProvisioner(f.dir, { dispatchBinding: f.binding });
		try {
			const manager = await provisioner.ensure();
			await expect(manager.execute("drop-transport")).rejects.toThrow();
			await f.disconnect;
			await expect(provisioner.ensure()).rejects.toThrow("transport terminated");
			await expect(provisioner.kill()).rejects.toThrow("503");
			await expect(provisioner.ensure()).rejects.toThrow("transport terminated");
			expect(f.commands).toHaveLength(1);
			expect(f.frames.filter((frame) => frame.code === "drop-transport")).toHaveLength(1);
			expect(f.cancellations).toEqual(["exec-1"]);
		} finally {
			await provisioner.dispose().catch(() => undefined);
			await f.close();
		}
	});
});
