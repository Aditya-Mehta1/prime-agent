import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { SESSION_CWD_CHANGED_CUSTOM_TYPE } from "../../src/core/messages.js";
import { createHarness, getMessageText } from "./harness.js";

describe("AgentSession.setCwd", () => {
	it("resolves a relative /cwd against the session cwd and retargets its owners", async () => {
		const harness = await createHarness();
		try {
			const child = join(harness.tempDir, "child");
			mkdirSync(child);
			const cwd = await harness.session.setCwd("child");
			expect(cwd).toBe(child);
			expect(harness.sessionManager.getCwd()).toBe(child);
			expect(harness.eventsOfType("cwd_changed")).toEqual([{ type: "cwd_changed", cwd: child }]);
			const notice = harness.session
				.getPendingNextTurnMessageSnapshots()
				.find((message) => message.customType === SESSION_CWD_CHANGED_CUSTOM_TYPE);
			const content = notice ? getMessageText(notice) : "";
			expect(content.startsWith("[cwd-changed]")).toBe(true);
			expect(content).toContain(child);
		} finally {
			harness.cleanup();
		}
	});

	it("resumes a session in its persisted /cwd directory", async () => {
		const first = await createHarness({ persistSession: true });
		const child = join(first.tempDir, "child");
		mkdirSync(child);
		await first.session.setCwd("child");
		const resumed = await createHarness({ existingSessionFile: first.session.sessionFile! });
		try {
			expect(resumed.sessionManager.getCwd()).toBe(child);
		} finally {
			resumed.cleanup();
			first.cleanup();
		}
	});

	it("rejects a missing or non-directory path without changing state", async () => {
		const harness = await createHarness();
		try {
			writeFileSync(join(harness.tempDir, "file.txt"), "x");
			const initialCwd = harness.sessionManager.getCwd();
			await expect(harness.session.setCwd("file.txt")).rejects.toThrow(
				`Not a directory: ${join(harness.tempDir, "file.txt")}`,
			);
			await expect(harness.session.setCwd("nope")).rejects.toThrow("Not a directory");
			expect(harness.sessionManager.getCwd()).toBe(initialCwd);
			expect(harness.eventsOfType("cwd_changed")).toEqual([]);
		} finally {
			harness.cleanup();
		}
	});
});
