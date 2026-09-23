import type { ChildProcess } from "node:child_process";
import { existsSync } from "node:fs";
import { isAbsolute, posix, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { getBundledSkillsDir } from "../../config.js";
import { spawnHidden } from "../../utils/child-process.js";
import type { KernelManagerOptions } from "../kernel/shared.js";
import type { PythonSkillRuntimeInfo } from "../skills.js";
import type { IpythonToolOptions } from "../tools/ipython.js";
import type { SailRelayConfig } from "./relay.js";
import { SailClient } from "./sail-client.js";
import type { DispatchBinding } from "./types.js";
import { shellQuote } from "./workspace.js";

export function dispatchKernelOptions(binding: DispatchBinding): Partial<IpythonToolOptions> {
	return {
		dispatchBinding: binding,
		snapshotDir: binding.guestStateDir,
	};
}

export async function prepareDispatchPythonSkills(
	binding: DispatchBinding,
	skills: readonly PythonSkillRuntimeInfo[],
	signal?: AbortSignal,
): Promise<void> {
	const packages = new Set<string>();
	for (const skill of skills) {
		const bundled = relative(getBundledSkillsDir(), skill.packagePath);
		if (!isAbsolute(bundled) && bundled !== ".." && !bundled.startsWith(`..${sep}`)) continue;
		const path = posix.resolve(
			binding.guestCwd,
			relative(binding.hostResourceDir, skill.packagePath).split(sep).join("/"),
		);
		if (path !== binding.guestRepoDir && !path.startsWith(`${binding.guestRepoDir}/`))
			throw new Error(`Python skill ${skill.name} is outside the copied dispatch project`);
		packages.add(path);
	}
	if (packages.size > 0)
		await new SailClient().run(
			binding.boxId,
			{
				command: `${shellQuote(binding.guestPython)} -m pip install --disable-pip-version-check -q ${[...packages].map(shellQuote).join(" ")}`,
				cwd: binding.guestCwd,
			},
			signal,
		);
}

/** A box cannot receive a new kernel until the previous exec is known to be stopped. */
const activeBoxes = new Map<string, symbol>();

export function createDispatchKernelLauncher(
	binding: DispatchBinding,
	env: Record<string, string>,
	client = new SailClient(),
): NonNullable<KernelManagerOptions["processLauncher"]> {
	const states = new WeakMap<
		ChildProcess,
		{ id: Promise<string>; exited: boolean; release(): void; stop?: Promise<void> }
	>();
	return {
		spawn() {
			if (activeBoxes.has(binding.boxId))
				throw new Error("Previous Sail kernel termination is unverified; cannot start another kernel");
			const owner = Symbol(binding.boxId);
			activeBoxes.set(binding.boxId, owner);
			const release = () => {
				if (activeBoxes.get(binding.boxId) === owner) activeBoxes.delete(binding.boxId);
			};
			const compiled = fileURLToPath(new URL("./relay.js", import.meta.url));
			const entry = existsSync(compiled) ? compiled : fileURLToPath(new URL("./relay.ts", import.meta.url));
			const args = entry.endsWith(".ts") ? ["--import", import.meta.resolve("tsx"), entry] : [entry];
			let child: ChildProcess;
			try {
				child = spawnHidden(process.execPath, args, { stdio: ["pipe", "pipe", "pipe", "ipc"] });
			} catch (error) {
				release();
				throw error;
			}
			let resolveId!: (id: string) => void;
			let rejectId!: (error: Error) => void;
			const id = new Promise<string>((resolve, reject) => {
				resolveId = resolve;
				rejectId = reject;
			});
			void id.catch(() => undefined);
			const state = { id, exited: false, release };
			states.set(child, state);
			const deadline = setTimeout(
				() => rejectId(new Error("Sail kernel submission did not return an exec ID; termination is unverified")),
				30_000,
			);
			deadline.unref();
			child.on("message", (value: unknown) => {
				if (!value || typeof value !== "object") return;
				const message = value as { type?: string; execId?: string };
				if (message.type === "started" && message.execId) {
					clearTimeout(deadline);
					resolveId(message.execId);
				}
				if (message.type === "remote-exit") {
					state.exited = true;
					release();
				}
			});
			child.once("error", () => {
				clearTimeout(deadline);
				rejectId(new Error("Sail relay could not start"));
			});
			child.once("exit", () => {
				clearTimeout(deadline);
				rejectId(new Error("Sail relay exited before reporting an exec ID; termination is unverified"));
			});
			const config: SailRelayConfig = {
				boxId: binding.boxId,
				cwd: binding.guestCwd,
				command: `exec ${shellQuote(binding.guestPython)} -u -m rlm.repl`,
				env: {
					...(process.env.GH_TOKEN ? { GH_TOKEN: process.env.GH_TOKEN } : {}),
					...(process.env.GITHUB_TOKEN ? { GITHUB_TOKEN: process.env.GITHUB_TOKEN } : {}),
					...Object.fromEntries(
						Object.entries(env).filter(([key]) =>
							[
								"RLM_DEPTH",
								"RLM_MAX_DEPTH",
								"PRIME_AGENT_BASH_SHELL",
								"PRIME_AGENT_BASH_COMMAND_PREFIX",
								"GH_TOKEN",
								"GITHUB_TOKEN",
								"SERPER_API_KEY",
							].includes(key),
						),
					),
					RLM_SESSION_DIR: binding.guestStateDir,
					RLM_HARNESS_STATE_DIR: `${binding.guestStateDir}/harness`,
					RLM_GLOBAL_HARNESS_STATE_DIR: `${binding.guestStateDir}/global-harness`,
					PRIME_AGENT_CODING_AGENT_DIR: binding.guestStateDir,
				},
			};
			child.send?.(config, (error) => {
				if (error) rejectId(error);
			});
			return child;
		},
		stop(child) {
			const state = states.get(child);
			if (!state) return Promise.reject(new Error("Unknown Sail relay process"));
			state.stop ??= (async () => {
				if (!state.exited) await client.cancelExec(binding.boxId, await state.id, true);
				state.release();
			})();
			return state.stop;
		},
	};
}
