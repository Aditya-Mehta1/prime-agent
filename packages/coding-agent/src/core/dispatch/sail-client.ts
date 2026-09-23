import { randomUUID } from "node:crypto";

export type SailExecEvent =
	| { type: "started"; exec_request_id: string }
	| { type: "stdout" | "stderr"; data: string; seq?: number }
	| { type: "exit"; return_code: number; status: string }
	| { type: "heartbeat" }
	| { type: "error"; message?: string; error_code?: string };

export interface SailExecOptions {
	command: string;
	cwd?: string;
	env?: Record<string, string>;
	open_stdin?: boolean;
	timeout?: number;
	idempotency_key?: string;
}

export interface SailboxInfo {
	sailbox_id: string;
	status: string;
	error_message?: string;
}

export class SailHttpError extends Error {
	constructor(
		readonly status: number,
		message: string,
	) {
		super(message);
	}
}

/** Small single-attachment client: never retries an exec or replays Python input. */
export class SailClient {
	private readonly apiKey: string;
	private readonly baseUrl: string;
	private readonly appsUrl: string;

	constructor(options: { apiKey?: string; baseUrl?: string; appsUrl?: string } = {}) {
		this.apiKey = options.apiKey ?? process.env.SAIL_API_KEY ?? "";
		if (!this.apiKey) throw new Error("Dispatch requires SAIL_API_KEY");
		this.baseUrl =
			options.baseUrl ?? process.env.PRIME_AGENT_SAILBOX_URL ?? "https://sailbox-api.sailresearch.com/v1";
		this.appsUrl = options.appsUrl ?? process.env.PRIME_AGENT_SAIL_APPS_URL ?? "https://api.sailresearch.com/v1";
	}

	private async request(path: string, init: RequestInit = {}, apps = false): Promise<Response> {
		const response = await fetch(`${apps ? this.appsUrl : this.baseUrl}${path}`, {
			...init,
			headers: { Authorization: `Bearer ${this.apiKey}`, ...init.headers },
		});
		if (!response.ok) {
			const detail = (await response.text()).slice(0, 2000).replaceAll(this.apiKey, "[redacted]");
			throw new SailHttpError(response.status, `Sail ${response.status}: ${detail}`);
		}
		return response;
	}

	private async post<T>(path: string, body: object = {}, apps = false, signal?: AbortSignal): Promise<T> {
		const response = await this.request(
			path,
			{
				method: "POST",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify(body),
				signal,
			},
			apps,
		);
		return (await response.json()) as T;
	}

	async findApp(name = "prime-agent-dispatch"): Promise<{ id: string }> {
		return this.post("/apps/find", { name, mint_if_missing: true }, true);
	}

	async createBox(appId: string, name: string): Promise<SailboxInfo> {
		const response = await this.request("/sailboxes", {
			method: "POST",
			headers: { "Content-Type": "application/json", "Idempotency-Key": randomUUID() },
			body: JSON.stringify({
				app_id: appId,
				name,
				size: "s",
				memory_limit_gib: 2,
				state_disk_limit_gib: 8,
				image: { base: "BASE_IMAGE_DEVBOX" },
			}),
		});
		return (await response.json()) as SailboxInfo;
	}

	async getBox(boxId: string): Promise<SailboxInfo> {
		return (await (await this.request(`/sailboxes/${encodeURIComponent(boxId)}`)).json()) as SailboxInfo;
	}

	async terminateBox(boxId: string): Promise<void> {
		await this.post(`/sailboxes/${encodeURIComponent(boxId)}/terminate`);
	}

	async sleepBox(boxId: string): Promise<void> {
		await this.post(`/sailboxes/${encodeURIComponent(boxId)}/sleep`);
	}

	async writeFile(boxId: string, path: string, data: Uint8Array, signal?: AbortSignal): Promise<void> {
		await this.request(
			`/sailboxes/${encodeURIComponent(boxId)}/files?${new URLSearchParams({ path, mode: "420" })}`,
			{
				method: "PUT",
				headers: { "Content-Type": "application/octet-stream" },
				body: Buffer.from(data),
				signal,
			},
		);
	}

	async *exec(boxId: string, options: SailExecOptions, signal?: AbortSignal): AsyncGenerator<SailExecEvent> {
		const response = await this.request(`/sailboxes/${encodeURIComponent(boxId)}/exec`, {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			signal,
			body: JSON.stringify({ ...options, idempotency_key: options.idempotency_key ?? randomUUID() }),
		});
		if (!response.body) throw new Error("Sail exec returned no stream");
		const reader = response.body.getReader();
		const decoder = new TextDecoder();
		let pending = "";
		let exited = false;
		try {
			while (true) {
				const chunk = await reader.read();
				pending += decoder.decode(chunk.value, { stream: !chunk.done });
				let newline = pending.indexOf("\n");
				while (newline >= 0) {
					const line = pending.slice(0, newline).trim();
					pending = pending.slice(newline + 1);
					if (line) {
						const event = JSON.parse(line) as SailExecEvent;
						if (event.type === "exit") exited = true;
						yield event;
					}
					newline = pending.indexOf("\n");
				}
				if (chunk.done) break;
			}
			if (pending.trim()) {
				const event = JSON.parse(pending) as SailExecEvent;
				if (event.type === "exit") exited = true;
				yield event;
			}
			if (!exited) throw new Error("Sail exec connection ended before exit; command state is unknown");
		} finally {
			await reader.cancel().catch(() => undefined);
			reader.releaseLock();
		}
	}

	async writeStdin(boxId: string, execId: string, data: Uint8Array, offset: number, eof = false): Promise<number> {
		const query = new URLSearchParams({ exec_request_id: execId, offset: String(offset), eof: String(eof) });
		const response = await this.request(`/sailboxes/${encodeURIComponent(boxId)}/exec/-/stdin?${query}`, {
			method: "PUT",
			headers: { "Content-Type": "application/octet-stream" },
			body: Buffer.from(data),
			signal: AbortSignal.timeout(30_000),
		});
		const result = (await response.json()) as { accepted_through: number };
		if (result.accepted_through !== offset + data.byteLength) throw new Error("Sail stdin offset mismatch");
		return result.accepted_through;
	}

	async cancelExec(boxId: string, execId: string, force = true): Promise<void> {
		const path = `/sailboxes/${encodeURIComponent(boxId)}/exec/-`;
		const query = new URLSearchParams({ exec_request_id: execId });
		const signal = AbortSignal.timeout(30_000);
		await this.post(`${path}/cancel?${query}`, { force }, false, signal);
		const response = await this.request(`${path}/wait?${query}`, { method: "POST", signal });
		const result = (await response.json()) as { status: string };
		if (!["succeeded", "failed", "timed_out"].includes(result.status)) {
			throw new Error(`Sail exec termination is unconfirmed (${result.status})`);
		}
	}

	async run(
		boxId: string,
		options: SailExecOptions,
		signal?: AbortSignal,
	): Promise<{ stdout: string; stderr: string }> {
		const output = { stdout: "", stderr: "" };
		for await (const event of this.exec(boxId, { timeout: 300, ...options }, signal)) {
			if (event.type === "stdout" || event.type === "stderr")
				output[event.type] += Buffer.from(event.data, "base64").toString("utf8");
			if (event.type === "error") throw new Error(`Sail exec: ${event.message ?? event.error_code}`);
			if (event.type === "exit" && event.return_code !== 0)
				throw new Error(
					`Sail command exited ${event.return_code}: ${output.stderr.slice(-4000)}${output.stdout.slice(-4000)}`,
				);
		}
		return output;
	}
}
