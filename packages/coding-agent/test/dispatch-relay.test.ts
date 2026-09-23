import { mkdtemp, rm } from "node:fs/promises";
import { createServer, type ServerResponse } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { dispatchKernelOptions } from "../src/core/dispatch/kernel.js";
import type { DispatchBinding } from "../src/core/dispatch/types.js";
import { IpythonKernelProvisioner } from "../src/core/tools/ipython.js";

async function fixture() {
	const dir = await mkdtemp(join(tmpdir(), "sail-relay-"));
	let output: ServerResponse | undefined;
	let buffered = "";
	let offset = 0;
	let sequence = 0;
	let activeId = "";
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
	const waitBodyLengths: number[] = [];
	let disconnected!: () => void;
	const disconnect = new Promise<void>((resolve) => {
		disconnected = resolve;
	});
	const event = (value: object) => output?.write(`${JSON.stringify(value)}\n`);
	const protocol = (value: object) => {
		const bytes = Buffer.from(`${JSON.stringify(value)}\n`);
		const middle = Math.floor(bytes.length / 2);
		for (const chunk of [bytes.subarray(0, middle), bytes.subarray(middle)])
			event({ type: "stdout", seq: sequence++, data: chunk.toString("base64") });
	};
	const done = (id: unknown, extra: object = {}) => protocol({ event: "done", id, status: "ok", ...extra });
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
			output = response;
			offset = 0;
			buffered = "";
			response.writeHead(200, { "Content-Type": "application/x-ndjson" });
			event({ type: "started", exec_request_id: `exec-${commands.length}` });
			protocol({ event: "ready", protocol: 3 });
			response.once("close", disconnected);
			return;
		}
		if (url.pathname.endsWith("/cancel")) {
			cancellations.push(url.searchParams.get("exec_request_id")!);
			response.writeHead(rejectCancel ? 503 : 200, { "Content-Type": "application/json" });
			response.end("{}");
			if (!rejectCancel) {
				event({ type: "exit", return_code: 0, status: "cancelled" });
				output?.end();
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
		offset += data.length;
		response.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify({ accepted_through: offset }));
		buffered += data.toString();
		let newline = buffered.indexOf("\n");
		while (newline >= 0) {
			const frame = JSON.parse(buffered.slice(0, newline)) as Record<string, unknown>;
			buffered = buffered.slice(newline + 1);
			frames.push(frame);
			newline = buffered.indexOf("\n");
			if (frame.type === "execute" && frame.code === "needs-host") {
				activeId = String(frame.id);
				protocol({ event: "host_request", id: "host-1", data: { type: "rlm.run", prompt: "remote child" } });
			} else if (frame.type === "host_reply") {
				protocol({ event: "result", id: activeId, text: "λ remote result" });
				done(activeId);
			} else if (frame.type === "execute" && frame.code === "drop-transport") {
				output?.destroy();
			} else if (frame.type === "restore") done(frame.id, { restored: ["counter"], failed: [] });
			else if (frame.type === "snapshot") done(frame.id, { saved: ["counter"], skipped: [], bytes: 12 });
			else if (frame.type === "shutdown") {
				done(frame.id);
				event({ type: "exit", return_code: 0, status: "completed" });
				output?.end();
			} else done(frame.id);
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
		version: 1,
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
		model: { provider: "sail", id: "deepseek-ai/DeepSeek-V4-Flash-0731" },
	};
	return {
		dir,
		binding,
		frames,
		commands,
		writes,
		cancellations,
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
			output?.destroy();
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
			...dispatchKernelOptions(f.binding),
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

	it("waits for verified cancellation before an explicit kernel replacement", async () => {
		const f = await fixture();
		const provisioner = new IpythonKernelProvisioner(f.dir, dispatchKernelOptions(f.binding));
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

	it("latches transport loss and a failed remote cancellation without starting or replaying another kernel", async () => {
		const f = await fixture();
		f.setRejectCancel(true);
		const provisioner = new IpythonKernelProvisioner(f.dir, dispatchKernelOptions(f.binding));
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
