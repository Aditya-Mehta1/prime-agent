import { spawnSync } from "node:child_process";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { normalizeRlmDispatchInputs } from "../src/core/rlm-runtime.js";

describe("RLM dispatch protocol", () => {
	it("serializes Python dispatch arguments and preserves the existing handle and spawn wire contract", () => {
		const program = `import asyncio, json, sys, types
from pathlib import Path
import rlm
requests = []
async def request(payload):
    requests.append(payload)
    return {"status": "ok", "result": {"rlm_child_id": "child-1", "name": "worker", "session_dir": "/host/session", "model": "sail/model"}}
bridge = types.ModuleType("rlm.repl")
bridge.host_request = request
sys.modules["rlm.repl"] = bridge
async def main():
    handle = await rlm.rlm.dispatch("task", name="worker", model="sail/model", thinking="high", inputs={"notes": Path("notes.txt")})
    await rlm.spawn("local", name="worker")
    print(json.dumps({"requests": requests, "handle": {"id": handle.rlm_child_id, "path": str(handle.session_dir), "typed": isinstance(handle, rlm.RLMSpawnHandle)}}))
asyncio.run(main())`;
		const child = spawnSync("python3", ["-c", program], {
			env: { ...process.env, PYTHONPATH: resolve("../../prime-agent-runtime/src") },
			encoding: "utf8",
			timeout: 10_000,
		});
		expect(child.status, child.stderr).toBe(0);
		expect(JSON.parse(child.stdout)).toEqual({
			requests: [
				{
					type: "rlm.dispatch",
					prompt: "task",
					kwargs: { name: "worker", model: "sail/model", thinking: "high", inputs: { notes: "notes.txt" } },
				},
				{ type: "rlm.run", prompt: "local", kwargs: { name: "worker" } },
			],
			handle: { id: "child-1", path: "/host/session", typed: true },
		});
	});

	it.each([null, [], { "../notes": "path" }, { notes: 42 }, { notes: " " }])(
		"rejects invalid input maps %j",
		(inputs) => {
			expect(() => normalizeRlmDispatchInputs(inputs)).toThrow("inputs");
		},
	);
});
