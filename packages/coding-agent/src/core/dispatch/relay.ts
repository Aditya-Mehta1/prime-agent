import { once } from "node:events";
import type { Readable, Writable } from "node:stream";
import { fileURLToPath } from "node:url";
import { SailClient } from "./sail-client.js";

export interface SailRelayConfig {
	boxId: string;
	command: string;
	cwd: string;
	env: Record<string, string>;
}

async function writeBytes(stream: Writable, bytes: Uint8Array): Promise<void> {
	if (!stream.write(bytes)) await once(stream, "drain");
}

/** Forward bytes once. Reconnecting or replaying stdin could execute a cell twice. */
export async function runSailRelay(
	client: Pick<SailClient, "exec" | "writeStdin">,
	config: SailRelayConfig,
	input: Readable,
	output: Writable,
	diagnostics: Writable,
	onStarted: (id: string) => void,
	onExit: () => void,
): Promise<number> {
	const controller = new AbortController();
	let inputTask: Promise<void> | undefined;
	let inputError: unknown;
	let started = false;
	const sequences: Partial<Record<"stdout" | "stderr", number>> = {};
	try {
		for await (const event of client.exec(
			config.boxId,
			{
				command: config.command,
				cwd: config.cwd,
				env: config.env,
				open_stdin: true,
			},
			controller.signal,
		)) {
			if (event.type === "started") {
				if (started) throw new Error("Sail relay received duplicate started event");
				started = true;
				onStarted(event.exec_request_id);
				inputTask = (async () => {
					let offset = 0;
					for await (const chunk of input) {
						const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
						const accepted = await client.writeStdin(config.boxId, event.exec_request_id, bytes, offset);
						if (accepted !== offset + bytes.length)
							throw new Error("Sail stdin acknowledgment mismatch; cell delivery is uncertain");
						offset = accepted;
					}
					await client.writeStdin(config.boxId, event.exec_request_id, new Uint8Array(), offset, true);
				})();
				void inputTask.catch((error: unknown) => {
					inputError = error;
					controller.abort();
				});
			} else if (event.type === "stdout" || event.type === "stderr") {
				if (!started) throw new Error("Sail output arrived before its exec ID");
				if (event.seq !== undefined) {
					const previous = sequences[event.type];
					if (previous !== undefined && event.seq !== previous + 1)
						throw new Error("Sail output sequence changed; refusing to replay or omit kernel bytes");
					sequences[event.type] = event.seq;
				}
				await writeBytes(event.type === "stdout" ? output : diagnostics, Buffer.from(event.data, "base64"));
			} else if (event.type === "exit") {
				onExit();
				return event.return_code ?? 1;
			} else if (event.type === "error") {
				throw new Error("Sail exec transport failed; the kernel will not be restarted automatically");
			}
		}
		throw new Error("Sail exec stream ended without an exit event; cell delivery is uncertain");
	} catch (error) {
		throw inputError ?? error;
	} finally {
		controller.abort();
		input.destroy();
		await inputTask?.catch(() => undefined);
	}
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
	process.once("message", (config: SailRelayConfig) => {
		const client = new SailClient();
		let execId: string | undefined;
		let exited = false;
		let cancelling = false;
		const parentDisconnected = () => {
			if (!execId || exited || cancelling) return;
			cancelling = true;
			void client.cancelExec(config.boxId, execId, true).then(
				() => process.exit(0),
				(error: unknown) => {
					process.stderr.write(
						`Sail kernel cleanup failed: ${error instanceof Error ? error.message : String(error)}\n`,
					);
					process.exit(1);
				},
			);
		};
		process.once("disconnect", parentDisconnected);
		void runSailRelay(
			client,
			config,
			process.stdin,
			process.stdout,
			process.stderr,
			(id) => {
				execId = id;
				if (process.connected) process.send?.({ type: "started", execId: id });
				else parentDisconnected();
			},
			() => {
				exited = true;
				if (process.connected) process.send?.({ type: "remote-exit" });
			},
		).then(
			(code) => {
				process.exitCode = code;
				process.removeListener("disconnect", parentDisconnected);
				if (process.connected) process.disconnect();
			},
			(error: unknown) => {
				process.stderr.write(`Sail kernel relay: ${error instanceof Error ? error.message : String(error)}\n`);
				process.exitCode = 1;
				process.removeListener("disconnect", parentDisconnected);
				if (process.connected) process.disconnect();
			},
		);
	});
}
