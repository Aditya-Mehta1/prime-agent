import { createHmac } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { mkdtempSync, rmSync, utimesSync, writeFileSync } from "fs";
import { afterAll, beforeAll, describe, expect, test, vi } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.js";
import { ModelRegistry } from "../src/core/model-registry.js";

// existsSync is tallied too: an absent models.json never reaches readFileSync.
const counters = vi.hoisted(() => ({ reads: new Map<string, number>(), exists: new Map<string, number>(), hashes: 0 }));
function tallying<A extends unknown[], R>(map: Map<string, number>, fn: (...args: A) => R): (...args: A) => R {
	return (...args: A) => {
		const key = String(args[0]);
		map.set(key, (map.get(key) ?? 0) + 1);
		return fn(...args);
	};
}
vi.mock("fs", async (importOriginal) => {
	const actual = await importOriginal<typeof import("fs")>();
	return {
		...actual,
		readFileSync: tallying(counters.reads, actual.readFileSync),
		existsSync: tallying(counters.exists, actual.existsSync),
	};
});
vi.mock("node:crypto", async (importOriginal) => {
	const actual = await importOriginal<typeof import("node:crypto")>();
	return {
		...actual,
		createHash: ((algorithm, options) => {
			counters.hashes += 1;
			return actual.createHash(algorithm, options);
		}) as typeof actual.createHash,
	};
});
const fingerprint = createHmac("sha256", "k").update("prime-agent:private-prime-authorization:v1\0t").digest("hex");
const counted = (map: Map<string, number>, path: string) => map.get(path) ?? 0;
const touch = (path: string, iso: string) => utimesSync(path, new Date(iso), new Date(iso));
const tempDir = mkdtempSync(join(tmpdir(), "pi-test-catalog-cache-"));
const modelsJsonPath = join(tempDir, "models.json");
const privateCachePath = join(tempDir, "prime-inference-private-models.json");
const primeTeam = { teamId: "t", name: "n" };
const authStorage = AuthStorage.inMemory({ "prime-inference": { type: "api_key", key: "k", primeTeam } });
beforeAll(() => {
	vi.stubEnv("PRIME_API_KEY", undefined);
	vi.stubEnv("PRIME_TEAM_ID", undefined);
	vi.stubEnv("PI_OFFLINE", "1");
});
afterAll(() => {
	vi.unstubAllEnvs();
	rmSync(tempDir, { recursive: true, force: true });
});

function writePrivateCache(displayName: string, mtimeIso: string): void {
	const pricing = { input_usd_per_mtok: 1, output_usd_per_mtok: 2 };
	const data = [{ id: "internal/glm-5.2-fast", display_name: displayName, pricing }];
	writeFileSync(privateCachePath, JSON.stringify({ fingerprint, data, refreshedAt: Date.now() }));
	touch(privateCachePath, mtimeIso);
}
const writeOpenrouterOverride = (baseUrl: string) =>
	writeFileSync(modelsJsonPath, JSON.stringify({ providers: { openrouter: { baseUrl } } }));

describe("model registry catalog and auth-source caching", () => {
	test("serves the authorization cache without rebuilding until an input changes", async () => {
		writePrivateCache("Cache Entry One", "2026-01-01T00:00:00.000Z");
		const registry = ModelRegistry.create(authStorage, modelsJsonPath);
		const glm = async () => (await registry.getExecutableModels()).find((m) => m.id === "internal/glm-5.2-fast");
		expect(await glm()).toBeDefined();
		expect(counted(counters.reads, privateCachePath)).toBe(1);
		expect(counted(counters.exists, modelsJsonPath)).toBeGreaterThan(0);
		for (const map of [counters.reads, counters.exists]) map.clear();
		const hashes = counters.hashes;
		await registry.getExecutableModels();
		expect(counted(counters.reads, privateCachePath)).toBe(0);
		expect(counted(counters.exists, modelsJsonPath)).toBe(0);
		expect(counters.hashes).toBe(hashes);
		writePrivateCache("Cache Entry Two", "2026-01-02T00:00:00.000Z");
		expect((await glm())?.name).toBe("Cache Entry Two");
		writeOpenrouterOverride("https://created.example/v1");
		touch(modelsJsonPath, "2026-01-03T00:00:00.000Z");
		await registry.getExecutableModels();
		expect(counted(counters.reads, modelsJsonPath)).toBe(1);
		writeOpenrouterOverride("https://rotated.example/v1");
		touch(modelsJsonPath, "2026-01-04T00:00:00.000Z");
		await registry.getExecutableModels();
		expect(counted(counters.reads, modelsJsonPath)).toBe(2);
		expect(registry.getAll().find((m) => m.provider === "openrouter")?.baseUrl).toBe("https://rotated.example/v1");
		const hasAuthSpy = vi.spyOn(authStorage, "hasAuth");
		await registry.getExecutableModels();
		expect(hasAuthSpy.mock.calls.filter((call) => call[0] === "openrouter")).toHaveLength(1);
	});
});
