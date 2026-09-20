import { spawn } from "node:child_process";
import { isRecord, type RouterEnvironment, type RouterExecution, type RouterObservation } from "./types.js";

/**
 * JSON-lines adapter protocol (the documented environment boundary):
 * request  {"id": <n>, "type": "init"|"reset"|"observe"|"execute"|"close", ...payload}
 * response {"id": <n>, "ok": true, ...result} | {"id": <n>, "ok": false, "error": "<message>"}
 * One JSON object per line on stdin, one per line on stdout. Adapters may be
 * any program that speaks it; the node-mgba GBA adapter
 * (examples/system-router-gba) is one such program.
 */

interface PendingRequest {
	resolve: (value: Record<string, unknown>) => void;
	reject: (error: Error) => void;
}

/** A reply line longer than this is a protocol violation, not a message to buffer. */
const MAX_REPLY_LINE_CHARS = 1_000_000;

export class StdioRouterEnvironment implements RouterEnvironment {
	private child: ReturnType<typeof spawn> | undefined;
	private nextId = 0;
	private readonly pending = new Map<number, PendingRequest>();
	private stderrTail = "";
	private closed = false;

	constructor(
		private readonly options: {
			command: string[];
			cwd?: string;
			requestTimeoutMs: number;
			init?: unknown;
		},
	) {}

	private ensureChild(): ReturnType<typeof spawn> {
		if (this.child) return this.child;
		const [command, ...args] = this.options.command;
		const child = spawn(command, args, {
			cwd: this.options.cwd,
			stdio: ["pipe", "pipe", "pipe"],
			// Adapter commands such as `docker run` need the environment they were given.
		});
		this.child = child;
		let buffer = "";
		child.stdout?.setEncoding("utf8");
		child.stdout?.on("data", (chunk: string) => {
			buffer += chunk;
			let newline = buffer.indexOf("\n");
			while (newline !== -1) {
				const line = buffer.slice(0, newline).trim();
				buffer = buffer.slice(newline + 1);
				if (line) this.dispatchLine(line);
				newline = buffer.indexOf("\n");
			}
			// An unterminated line is buffered until its newline arrives; a
			// protocol-violating line that never ends must not grow without bound.
			if (buffer.length > MAX_REPLY_LINE_CHARS) {
				const overflow = `environment adapter wrote an unterminated reply line over ${MAX_REPLY_LINE_CHARS} chars`;
				buffer = "";
				this.appendKernelAdapterDiagnostic(overflow);
				this.failAll(new Error(overflow));
			}
		});
		child.stderr?.setEncoding("utf8");
		child.stderr?.on("data", (chunk: string) => {
			this.stderrTail = `${this.stderrTail}${chunk}`.slice(-2_000);
		});
		child.on("error", (error) => {
			this.failAll(new Error(`environment adapter failed to start: ${error.message}`));
		});
		child.on("exit", (code) => {
			if (!this.closed) {
				this.failAll(
					new Error(`environment adapter exited early (code ${code ?? "null"}): ${this.stderrTail.trim()}`),
				);
			}
		});
		return child;
	}

	private dispatchLine(line: string): void {
		let parsed: unknown;
		try {
			parsed = JSON.parse(line);
		} catch {
			return;
		}
		if (typeof parsed !== "object" || parsed === null) return;
		const record = parsed as Record<string, unknown>;
		const id = typeof record.id === "number" ? record.id : undefined;
		const pending = id !== undefined ? this.pending.get(id) : undefined;
		if (id === undefined || !pending) return;
		this.pending.delete(id);
		if (record.ok === true) {
			pending.resolve(record);
		} else {
			const detail = typeof record.error === "string" ? record.error.slice(0, 2_000) : "adapter error";
			pending.reject(new Error(detail));
		}
	}

	private failAll(error: Error): void {
		const pending = [...this.pending.values()];
		this.pending.clear();
		for (const request of pending) request.reject(error);
	}

	/** Keep a bounded tail of protocol violations alongside the stderr tail. */
	private appendKernelAdapterDiagnostic(message: string): void {
		this.stderrTail = `${this.stderrTail}\n${message}`.slice(-2_000);
	}

	private async request(type: string, payload: Record<string, unknown> = {}): Promise<Record<string, unknown>> {
		const child = this.ensureChild();
		if (child.stdin === null) throw new Error("environment adapter stdin is not a pipe");
		const id = this.nextId;
		this.nextId += 1;
		const request = { id, type, ...payload };
		const response = new Promise<Record<string, unknown>>((resolve, reject) => {
			this.pending.set(id, { resolve, reject });
			const line = `${JSON.stringify(request)}\n`;
			child.stdin?.write(line, (error) => {
				if (error) {
					this.pending.delete(id);
					reject(new Error(`failed writing to the environment adapter: ${error.message}`));
				}
			});
		});
		// The losing side of the timeout race keeps a handler, so a late adapter
		// reply or rejection can never surface as an unhandled rejection.
		const timeout = new Promise<never>((_, reject) => {
			const timer = setTimeout(() => {
				this.pending.delete(id);
				reject(new Error(`environment adapter ${type} timed out after ${this.options.requestTimeoutMs}ms`));
			}, this.options.requestTimeoutMs);
			if (typeof timer === "object" && "unref" in timer) timer.unref();
		});
		void timeout.catch(() => {});
		try {
			return await Promise.race([response, timeout]);
		} finally {
			// The response promise is handled here or by the race; a late resolution is a no-op.
			void response.catch(() => {});
		}
	}

	/** Initialize the adapter and merge its default action space, if it supplies one. */
	async init(): Promise<Record<string, unknown> | undefined> {
		const reply = await this.request("init", this.options.init ? { init: this.options.init } : {});
		return reply.environment as Record<string, unknown> | undefined;
	}

	async reset(goal: string): Promise<void> {
		await this.request("reset", { goal });
	}

	async observe(): Promise<RouterObservation> {
		const reply = await this.request("observe");
		const observation = reply.observation;
		if (!isRecord(observation) || typeof observation.text !== "string") {
			throw new Error("environment adapter observe must reply with {ok: true, observation: {text: string}}");
		}
		const record: Record<string, unknown> = observation;
		const observationOut: RouterObservation = { text: record.text as string };
		if (typeof record.fields === "object" && record.fields !== null) {
			observationOut.fields = record.fields as RouterObservation["fields"];
		}
		if (typeof record.image === "string" && record.image) {
			observationOut.image = record.image;
		}
		if (record.terminal === true) observationOut.terminal = true;
		return observationOut;
	}

	async execute(action: string, params: Record<string, string>): Promise<RouterExecution> {
		const reply = await this.request("execute", { action, params });
		if (typeof reply.text !== "string") {
			throw new Error("environment adapter execute must reply with {ok: true, text: string}");
		}
		return { text: reply.text, ...(reply.terminal === true ? { terminal: true } : {}) };
	}

	async close(): Promise<void> {
		this.closed = true;
		const child = this.child;
		if (!child) return;
		if (child.exitCode !== null || child.signalCode !== null) {
			// The adapter already exited; its exit event was handled at exit time.
			this.failAll(new Error("environment adapter closed"));
			return;
		}
		// Ask the adapter to exit, then enforce a bounded shutdown.
		child.stdin?.end(`${JSON.stringify({ id: this.nextId, type: "close" })}\n`);
		const exited = new Promise<void>((resolve) => {
			child.once("exit", () => resolve());
		});
		await Promise.race([
			exited,
			new Promise<void>((resolve) => {
				const timer = setTimeout(() => resolve(), 1_500);
				if (typeof timer === "object" && "unref" in timer) timer.unref();
			}),
		]);
		if (child.exitCode === null && child.signalCode === null) {
			child.kill("SIGKILL");
		}
		this.failAll(new Error("environment adapter closed"));
	}
}
