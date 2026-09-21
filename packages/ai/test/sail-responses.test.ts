import { readFileSync } from "node:fs";
import { Type } from "typebox";
import { afterEach, describe, expect, it, vi } from "vitest";
import { getEnvApiKey } from "../src/env-api-keys.js";
import { getModel } from "../src/models.js";
import { streamSailResponses } from "../src/providers/sail-responses.js";
import { streamSimple } from "../src/stream.js";
import type { Context } from "../src/types.js";

const model = getModel("sail", "zai-org/GLM-5.3-Flash");
const context: Context = { messages: [{ role: "user", content: "Add 17 + 25", timestamp: 1 }] };
const call = { type: "function_call", id: "fc_add", call_id: "call_add", name: "add", arguments: '{"a":17,"b":25}' };
const thinking = { type: "reasoning", id: "rs_add", summary: [{ type: "summary_text", text: "Use add." }] };
const message = {
	type: "message",
	id: "msg_add",
	role: "assistant",
	status: "completed",
	content: [{ type: "output_text", text: "42", annotations: [] }],
};
function response(status = "completed", output: unknown[] = [message]) {
	return {
		id: "resp_add",
		status,
		output,
		usage: { input_tokens: 20, output_tokens: 8, total_tokens: 28, input_tokens_details: { cached_tokens: 5 } },
	};
}
function json(body: unknown, status = 200) {
	return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
}

describe("Sail Responses", () => {
	afterEach(() => {
		vi.useRealTimers();
		vi.unstubAllGlobals();
		vi.unstubAllEnvs();
	});

	it("uses foreground ASAP for the installed root default even after payload hooks", async () => {
		const installer = readFileSync(new URL("../../../install-local.sh", import.meta.url), "utf8");
		const defaults = JSON.parse(installer.match(/'(\{"defaultProvider"[^']+\})'/)![1]);
		const rootModel = getModel(defaults.defaultProvider, defaults.defaultModel);
		expect(defaults.subagentDefaultModel).toBe("sail/zai-org/GLM-5.3-Flash");
		vi.stubEnv("SAIL_API_KEY", "test-sail-key");
		const bodies: Record<string, unknown>[] = [];
		vi.stubGlobal("fetch", async (input: Parameters<typeof fetch>[0], init?: RequestInit) => {
			const request = new Request(input, init);
			expect(request.headers.get("authorization")).toBe("Bearer test-sail-key");
			bodies.push(JSON.parse(await request.text()));
			const events = [
				{ type: "response.created", response: response("in_progress", []) },
				{ type: "response.output_item.added", output_index: 0, item: { ...message, content: [] } },
				{ type: "response.content_part.added", output_index: 0, part: { ...message.content[0], text: "" } },
				{ type: "response.output_text.delta", output_index: 0, delta: "42" },
				{ type: "response.output_item.done", output_index: 0, item: message },
				{ type: "response.completed", response: response() },
			];
			return new Response(events.map((event) => `data: ${JSON.stringify(event)}\n\n`).join(""), {
				headers: { "Content-Type": "text/event-stream" },
			});
		});
		const stream = streamSimple(rootModel, context, {
			onPayload: (payload) => ({
				...(payload as object),
				background: true,
				metadata: { completion_window: "flex" },
			}),
		});
		const events: string[] = [];
		for await (const event of stream) events.push(event.type);
		const result = await stream.result();
		expect(result.stopReason, result.errorMessage).toBe("stop");
		expect(result.content[0]).toMatchObject({ type: "text", text: "42" });
		expect(result.usage.cost.total).toBeCloseTo(0.00000455, 10);
		expect(events).toEqual(["start", "text_start", "text_delta", "text_end", "done"]);
		expect(bodies).toEqual([
			expect.objectContaining({ background: false, stream: true, metadata: { completion_window: "asap" } }),
		]);
	});

	it("registers the Flex model and replays tools after polling with usage and stream events", async () => {
		vi.useFakeTimers();
		vi.stubEnv("SAIL_API_KEY", "test-sail-key");
		expect(getEnvApiKey("sail")).toBe("test-sail-key");
		const requests: Request[] = [];
		const bodies: Record<string, unknown>[] = [];
		vi.stubGlobal("fetch", async (input: Parameters<typeof fetch>[0], init?: RequestInit) => {
			const request = new Request(input, init);
			requests.push(request);
			if (request.method === "POST") bodies.push(JSON.parse(await request.text()));
			return json(
				requests.length === 1
					? response("queued", [])
					: requests.length === 2
						? response("in_progress", [])
						: requests.length === 3
							? response("completed", [thinking, call])
							: response(),
			);
		});
		const turn: Context = {
			...context,
			messages: [...context.messages],
			tools: [
				{
					name: "add",
					description: "Add numbers",
					parameters: Type.Object({ a: Type.Number(), b: Type.Number() }),
				},
			],
		};
		const stream = streamSimple(model, turn, {
			reasoning: "off",
			maxTokens: 64,
			sessionId: "session-add",
			onPayload: (payload) => ({
				...(payload as object),
				background: false,
				stream: true,
				metadata: { completion_window: "asap", label: "test" },
				previous_response_id: "old",
				conversation: "old",
			}),
		});
		const events: string[] = [];
		const consume = (async () => {
			for await (const event of stream) {
				events.push(event.type);
				if (event.type === "start") {
					await vi.advanceTimersByTimeAsync(1000);
					await vi.advanceTimersByTimeAsync(2000);
				}
			}
		})();
		const first = await stream.result();
		await consume;
		expect(first.stopReason, first.errorMessage).toBe("toolUse");
		expect(first.content).toEqual([
			{ type: "thinking", thinking: "Use add.", thinkingSignature: JSON.stringify(thinking) },
			{ type: "toolCall", id: "call_add|fc_add", name: "add", arguments: { a: 17, b: 25 } },
		]);
		expect(events).toEqual([
			"start",
			"thinking_start",
			"thinking_delta",
			"thinking_end",
			"toolcall_start",
			"toolcall_delta",
			"toolcall_end",
			"done",
		]);
		expect(first.usage).toMatchObject({ input: 15, output: 8, cacheRead: 5, totalTokens: 28 });
		expect(first.usage.cost.total).toBeCloseTo(0.00000224, 10);
		expect(requests.map((request) => [request.method, request.url])).toEqual([
			["POST", `${model.baseUrl}/responses`],
			["GET", `${model.baseUrl}/responses/resp_add`],
			["GET", `${model.baseUrl}/responses/resp_add`],
		]);
		expect(requests[0].headers.get("authorization")).toBe("Bearer test-sail-key");
		expect(requests[0].headers.get("idempotency-key")).toBeTruthy();
		expect(bodies[0]).toMatchObject({
			model: model.id,
			background: true,
			stream: false,
			max_output_tokens: 64,
			reasoning: { effort: "none" },
			prompt_cache_key: "session-add",
			metadata: { completion_window: "flex", label: "test" },
		});
		expect(bodies[0]).not.toHaveProperty("previous_response_id");
		expect(bodies[0]).not.toHaveProperty("conversation");
		turn.messages.push(first, {
			role: "toolResult",
			toolCallId: "call_add|fc_add",
			toolName: "add",
			content: [{ type: "text", text: "42" }],
			isError: false,
			timestamp: 2,
		});
		const secondStream = streamSimple(model, turn);
		const secondEvents = [];
		for await (const event of secondStream) secondEvents.push(event.type);
		expect((await secondStream.result()).content[0]).toMatchObject({ type: "text", text: "42" });
		expect(secondEvents).toEqual(["start", "text_start", "text_delta", "text_end", "done"]);
		expect(bodies[1].input).toEqual(
			expect.arrayContaining([
				expect.objectContaining({ type: "function_call", call_id: "call_add", arguments: call.arguments }),
				{ type: "function_call_output", call_id: "call_add", output: "42" },
			]),
		);
		expect(requests[3].headers.get("idempotency-key")).not.toBe(requests[0].headers.get("idempotency-key"));
	});

	it.each([
		["incomplete", "length"],
		["failed", "error"],
		["cancelled", "error"],
		["unknown", "error"],
	])("handles terminal status %s without losing usage", async (status, stopReason) => {
		vi.stubGlobal("fetch", async () =>
			json({ ...response(status), error: { code: "server_error", message: "generation failed" } }),
		);
		const result = await streamSimple(model, context, { apiKey: "test-key" }).result();
		expect(result.stopReason).toBe(stopReason);
		if (status !== "unknown") expect(result.usage.totalTokens).toBe(28);
		if (status === "failed") expect(result.errorMessage).toContain("generation failed");
	});

	it.each(["before-submit", "poll-wait", "poll-request"])(
		"aborts at %s and retains the submitted response ID",
		async (stage) => {
			vi.useFakeTimers();
			const controller = new AbortController();
			const requests: string[] = [];
			vi.stubGlobal("fetch", async (input: Parameters<typeof fetch>[0], init?: RequestInit) => {
				const request = new Request(input, init);
				requests.push(request.method);
				if (request.method === "GET") {
					controller.abort();
					throw new DOMException("Aborted", "AbortError");
				}
				return json(response("queued", []));
			});
			if (stage === "before-submit") controller.abort();
			const stream = streamSimple(model, context, { apiKey: "test-key", signal: controller.signal });
			for await (const event of stream) {
				if (event.type === "start") {
					if (stage === "poll-wait") controller.abort();
					else await vi.advanceTimersByTimeAsync(1000);
				}
			}
			const result = await stream.result();
			expect(result.stopReason).toBe("aborted");
			expect(result.responseId).toBe(stage === "before-submit" ? undefined : "resp_add");
			expect(result.content).toEqual([]);
			expect(requests).toEqual(stage === "before-submit" ? [] : stage === "poll-wait" ? ["POST"] : ["POST", "GET"]);
		},
	);

	it("does not retry failed submissions and sends an explicit idempotency key", async () => {
		const requests: Request[] = [];
		vi.stubGlobal("fetch", async (input: Parameters<typeof fetch>[0], init?: RequestInit) => {
			requests.push(new Request(input, init));
			return json({ error: { message: "Temporarily unavailable" } }, 503);
		});
		const result = await streamSailResponses(model, context, {
			apiKey: "test-key",
			idempotencyKey: "logical-turn-1",
		}).result();
		expect(result.stopReason).toBe("error");
		expect(requests).toHaveLength(1);
		expect(requests[0].headers.get("idempotency-key")).toBe("logical-turn-1");
	});
});
