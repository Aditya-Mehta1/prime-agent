import { describe, expect, it } from "vitest";
import { isLikelyAuthenticationError } from "../src/core/auth-guidance.js";
import { classifyTelemetryError, TELEMETRY_ERROR_MESSAGES } from "../src/core/telemetry-error-classification.js";

const SECRET = "sensitive-canary-api-key-private-path-request-id-prompt";

describe("safe telemetry error classification", () => {
	it.each([
		[402, "insufficient_funds", "insufficient_balance", "other"],
		[402, "insufficient_quota", "quota_exceeded", "rate_limit"],
		[503, "authentication_error", "provider_unavailable", "provider_unavailable"],
		[403, "authentication_error", "permission_denied", "other"],
		[403, "model_access_denied", "model_access_denied", "other"],
		[401, undefined, "authentication_rejected", "authentication"],
		[401, "invalid_api_key", "credential_invalid", "authentication"],
		[401, "token_expired", "credential_expired", "authentication"],
		[402, undefined, "unknown", "other"],
		[402, "authentication_error", "unknown", "other"],
		[429, undefined, "rate_limited", "rate_limit"],
		[504, undefined, "timeout", "timeout"],
	] as const)("classifies status %s and reason %s without trusting prose", (status, code, subtype, category) => {
		const error = Object.assign(new Error(`Authentication failed: API key ${SECRET}`), {
			status,
			error: { code, message: SECRET },
			requestID: SECRET,
			headers: { authorization: SECRET },
		});
		const result = classifyTelemetryError(error);
		expect(result.error_subtype).toBe(subtype);
		expect(result.http_status).toBe(status);
		// The category follows the structured subtype, not the prose: a billing
		// or permission failure must not keep the credential category just
		// because the message says "Authentication".
		expect(result.error_category).toBe(category);
		expect(JSON.stringify(result)).not.toContain(SECRET);
		expect(result.diagnostic_message).toBe(TELEMETRY_ERROR_MESSAGES[subtype]);
	});

	it("reads provider diagnostics without forwarding diagnostic errors or raw details", () => {
		const result = classifyTelemetryError({
			errorMessage: SECRET,
			diagnostics: [
				{
					type: "provider_stream_failure",
					error: { message: SECRET, stack: SECRET },
					details: { status: 402, providerErrorType: "insufficient_funds", requestId: SECRET, raw: SECRET },
				},
			],
		});
		expect(result.error_subtype).toBe("insufficient_balance");
		expect(result.classification_source).toBe("structured_reason");
		expect(JSON.stringify(result)).not.toContain(SECRET);
	});

	it("uses bounded nested causes and native network codes", () => {
		const cause = Object.assign(new Error(SECRET), { code: "ECONNRESET" });
		const error = new Error(SECRET, { cause });
		cause.cause = error;
		expect(classifyTelemetryError(error)).toMatchObject({ error_subtype: "network_error", error_code: "ECONNRESET" });
	});

	it("does not guess an underlying cause from arbitrary error prose or unknown codes", () => {
		const result = classifyTelemetryError({
			message: `Invalid token ${SECRET}; HTTP 402 insufficient funds`,
			code: SECRET,
		});
		expect(result).toMatchObject({
			error_subtype: "unknown",
			error_code: "unknown",
			http_status: null,
			classification_source: "unknown",
		});
		expect(JSON.stringify(result)).not.toContain(SECRET);
	});

	it("replaces variable values in reviewed application templates", () => {
		const result = classifyTelemetryError(new Error(`No API key for provider: ${SECRET}`));
		expect(result.error_subtype).toBe("credential_missing");
		expect(result.diagnostic_message).toBe("No API key found for [provider].");
		expect(JSON.stringify(result)).not.toContain(SECRET);
	});

	it("handles unreadable error properties and invalid statuses", () => {
		const error = Object.defineProperty({ status: Infinity, code: SECRET }, "message", {
			get() {
				throw new Error(SECRET);
			},
		});
		expect(classifyTelemetryError(error)).toMatchObject({ error_subtype: "unknown", http_status: null });
	});

	it("uses the same evidence for login guidance", () => {
		const text = "Authentication failed for API key";
		expect(isLikelyAuthenticationError(text, { message: text, status: 503 })).toBe(false);
		expect(isLikelyAuthenticationError(text, { message: text, status: 402, code: "insufficient_funds" })).toBe(false);
		expect(isLikelyAuthenticationError(text, { message: text, status: 403 })).toBe(false);
		expect(isLikelyAuthenticationError(text, { message: text, status: 401 })).toBe(false);
		expect(isLikelyAuthenticationError(text, { message: text, status: 401, code: "invalid_api_key" })).toBe(true);
	});

	it("uses a nested specific reason before an outer generic auth type", () => {
		expect(
			classifyTelemetryError({ type: "authentication_error", status: 402, error: { code: "insufficient_funds" } })
				.error_subtype,
		).toBe("insufficient_balance");
	});
});
