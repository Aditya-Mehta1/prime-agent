import OpenAI from "openai";
import type {
	Response,
	ResponseCreateParamsNonStreaming,
	ResponseStreamEvent,
} from "openai/resources/responses/responses.js";
import { getEnvApiKey } from "../env-api-keys.js";
import { clampThinkingLevel } from "../models.js";
import type {
	AssistantMessage,
	ModelThinkingLevel,
	SimpleStreamOptions,
	StreamFunction,
	StreamOptions,
} from "../types.js";
import { AssistantMessageEventStream } from "../utils/event-stream.js";
import { headersToRecord } from "../utils/headers.js";
import { formatStreamFailureMessage, recordStreamFailure } from "../utils/stream-failure.js";
import { convertResponsesMessages, convertResponsesTools, processResponsesStream } from "./openai-responses-shared.js";
import { buildBaseOptions } from "./simple-options.js";

export interface SailResponsesOptions extends StreamOptions {
	reasoningEffort?: ModelThinkingLevel;
	/** Reuse only when retrying the same logical request with an identical payload. */
	idempotencyKey?: string;
}

const TOOL_CALL_PROVIDERS = new Set(["sail", "sail-asap"]);

function waitForPoll(ms: number, signal?: AbortSignal): Promise<void> {
	return new Promise((resolve, reject) => {
		const onAbort = () => {
			clearTimeout(timer);
			reject(new Error("Request was aborted"));
		};
		const timer = setTimeout(() => {
			signal?.removeEventListener("abort", onAbort);
			resolve();
		}, ms);
		signal?.addEventListener("abort", onAbort, { once: true });
		if (signal?.aborted) onAbort();
	});
}

// Background Responses have no token stream. Feed complete items through the
// shared parser so replay signatures, tool IDs, usage, and events stay identical.
async function* responseEvents(response: Response): AsyncIterable<ResponseStreamEvent> {
	let sequence_number = 0;
	yield { type: "response.created", response, sequence_number: sequence_number++ };
	for (const [output_index, source] of response.output.entries()) {
		const item = { ...source, id: source.id ?? `fc_${output_index}` };
		const startItem =
			item.type === "message"
				? { ...item, content: [] }
				: item.type === "function_call"
					? { ...item, arguments: "" }
					: structuredClone(item);
		yield { type: "response.output_item.added", item: startItem, output_index, sequence_number: sequence_number++ };
		const position = { output_index, item_id: item.id };
		if (item.type === "message") {
			for (const [content_index, part] of item.content.entries()) {
				yield {
					type: "response.content_part.added",
					...position,
					content_index,
					part: part.type === "output_text" ? { ...part, text: "" } : { ...part, refusal: "" },
					sequence_number: sequence_number++,
				};
				yield {
					type: part.type === "output_text" ? "response.output_text.delta" : "response.refusal.delta",
					...position,
					content_index,
					delta: part.type === "output_text" ? part.text : part.refusal,
					logprobs: [],
					sequence_number: sequence_number++,
				};
			}
		} else if (item.type === "function_call") {
			yield {
				type: "response.function_call_arguments.delta",
				...position,
				delta: item.arguments,
				sequence_number: sequence_number++,
			};
		} else if (item.type === "reasoning") {
			yield {
				type: "response.reasoning_text.delta",
				...position,
				content_index: 0,
				delta:
					item.summary?.map((part) => part.text).join("\n\n") ||
					item.content?.map((part) => part.text).join("\n\n") ||
					"",
				sequence_number: sequence_number++,
			};
		}
		yield { type: "response.output_item.done", item, output_index, sequence_number: sequence_number++ };
	}
	yield {
		type: response.status === "incomplete" ? "response.incomplete" : "response.completed",
		response,
		sequence_number: sequence_number++,
	};
}

export const streamSailResponses: StreamFunction<"sail-responses", SailResponsesOptions> = (
	model,
	context,
	options,
) => {
	const stream = new AssistantMessageEventStream();
	const output: AssistantMessage = {
		role: "assistant",
		content: [],
		api: model.api,
		provider: model.provider,
		model: model.id,
		usage: {
			input: 0,
			output: 0,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 0,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		stopReason: "stop",
		timestamp: Date.now(),
	};
	(async () => {
		try {
			const completionWindow = model.provider === "sail-asap" ? "asap" : "flex";
			const apiKey = options?.apiKey || getEnvApiKey(model.provider);
			if (!apiKey) throw new Error("Sail API key is required. Set SAIL_API_KEY or configure Sail authentication.");
			options?.signal?.throwIfAborted();
			const client = new OpenAI({
				apiKey,
				baseURL: model.baseUrl,
				defaultHeaders: { ...model.headers, ...options?.headers },
				dangerouslyAllowBrowser: true,
				maxRetries: 0,
			});
			let params: ResponseCreateParamsNonStreaming = {
				model: model.id,
				input: convertResponsesMessages(model, context, TOOL_CALL_PROVIDERS),
				tools: context.tools?.length ? convertResponsesTools(context.tools) : undefined,
				background: completionWindow === "flex",
				stream: false,
				max_output_tokens: options?.maxTokens,
				temperature: options?.temperature,
				prompt_cache_key: options?.cacheRetention === "none" ? undefined : options?.sessionId,
				metadata: Object.fromEntries(
					Object.entries(options?.metadata ?? {}).filter(
						(entry): entry is [string, string] => typeof entry[1] === "string",
					),
				),
			};
			if (model.reasoning && options?.reasoningEffort) {
				const level = clampThinkingLevel(model, options.reasoningEffort);
				params.reasoning = {
					effort: (model.thinkingLevelMap?.[level] ?? (level === "off" ? "none" : level)) as NonNullable<
						ResponseCreateParamsNonStreaming["reasoning"]
					>["effort"],
				};
			}
			params.metadata = { ...params.metadata, completion_window: completionWindow };
			const replacement = await options?.onPayload?.(params, model);
			if (replacement !== undefined) params = replacement as ResponseCreateParamsNonStreaming;
			// Hooks may customize inference, but cannot silently change the billing window.
			params = {
				...params,
				background: completionWindow === "flex",
				stream: false,
				metadata: { ...params.metadata, completion_window: completionWindow },
			};
			delete params.previous_response_id;
			delete params.conversation;
			const requestOptions = {
				signal: options?.signal,
				...(options?.timeoutMs !== undefined ? { timeout: options.timeoutMs } : {}),
				headers: { "Idempotency-Key": options?.idempotencyKey ?? globalThis.crypto.randomUUID() },
			};
			if (completionWindow === "asap") {
				const { data, response } = await client.responses
					.create({ ...params, stream: true }, requestOptions)
					.withResponse();
				await options?.onResponse?.({ status: response.status, headers: headersToRecord(response.headers) }, model);
				options?.signal?.throwIfAborted();
				stream.push({ type: "start", partial: output });
				await processResponsesStream(data, output, stream, model);
				options?.signal?.throwIfAborted();
				if (output.stopReason === "error" || output.stopReason === "aborted") {
					throw new Error(output.errorMessage ?? "Sail response failed");
				}
				stream.push({ type: "done", reason: output.stopReason, message: output });
				stream.end();
				return;
			}
			const { data, response: httpResponse } = await client.responses.create(params, requestOptions).withResponse();
			output.responseId = data.id;
			await options?.onResponse?.(
				{ status: httpResponse.status, headers: headersToRecord(httpResponse.headers) },
				model,
			);
			options?.signal?.throwIfAborted();
			stream.push({ type: "start", partial: output });
			let response = data;
			let pollMs = 1000;
			while (response.status === "queued" || response.status === "in_progress") {
				await waitForPoll(pollMs, options?.signal);
				response = await client.responses.retrieve(data.id, {}, requestOptions);
				options?.signal?.throwIfAborted();
				pollMs = Math.min(pollMs * 2, 5000);
			}
			options?.signal?.throwIfAborted();
			if (!response.status || !["completed", "incomplete", "failed", "cancelled"].includes(response.status)) {
				throw new Error(`Unknown Sail response status: ${response.status}`);
			}
			await processResponsesStream(responseEvents(response), output, stream, model);
			options?.signal?.throwIfAborted();
			if (response.status === "failed" || response.status === "cancelled") {
				throw new Error(response.error?.message ?? `Sail response ${response.status}`);
			}
			if (output.stopReason === "error" || output.stopReason === "aborted")
				throw new Error(`Sail response ${response.status}`);
			stream.push({ type: "done", reason: output.stopReason, message: output });
			stream.end();
		} catch (error) {
			output.stopReason = options?.signal?.aborted ? "aborted" : "error";
			output.errorMessage = formatStreamFailureMessage(error);
			recordStreamFailure(model, output, error);
			stream.push({ type: "error", reason: output.stopReason, error: output });
			stream.end();
		}
	})();
	return stream;
};

export const streamSimpleSailResponses: StreamFunction<"sail-responses", SimpleStreamOptions> = (
	model,
	context,
	options,
) => streamSailResponses(model, context, { ...buildBaseOptions(model, options), reasoningEffort: options?.reasoning });
