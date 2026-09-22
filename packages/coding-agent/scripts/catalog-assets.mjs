#!/usr/bin/env node
import { cpSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const packageDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");

export const bundledCatalogFiles = ["models.bundled.json", "mcp-services.bundled.json"];
export const MIN_BUNDLED_MODEL_TRANSPORT_TUPLES = 42;
export const MIN_BUNDLED_MCP_SERVICES = 20;
export const DEFAULT_MODEL_CATALOG_URL =
	"https://raw.githubusercontent.com/PrimeIntellect-ai/prime-agent-catalog/main/models/catalog.v1.json";
export const DEFAULT_MCP_SERVICE_CATALOG_URL =
	"https://raw.githubusercontent.com/PrimeIntellect-ai/prime-agent-catalog/main/plugins/catalog.v2.json";
export const MAX_REMOTE_CATALOG_BYTES = 20 * 1024 * 1024;

function catalogSourcePaths(catalogDir) {
	return {
		models: join(catalogDir, "models", "catalog.v1.json"),
		mcpServices: join(catalogDir, "plugins", "catalog.v2.json"),
	};
}

function bundledTargets(outDir) {
	return {
		models: join(outDir, "models.bundled.json"),
		mcpServices: join(outDir, "mcp-services.bundled.json"),
	};
}

function isTrustedCatalogUrl(url) {
	try {
		const parsed = new URL(url);
		return (
			parsed.origin === "https://raw.githubusercontent.com" &&
			parsed.pathname.startsWith("/PrimeIntellect-ai/prime-agent-catalog/")
		);
	} catch {
		return false;
	}
}

function authHeaders(url, options = {}) {
	const token = process.env.GITHUB_TOKEN || process.env.PRIME_CATALOG_REPO_TOKEN;
	return {
		accept: "application/json",
		...(token && (options.allowTokenForUrl === true || isTrustedCatalogUrl(url))
			? { authorization: `Bearer ${token}` }
			: {}),
	};
}

async function readBoundedResponseText(response, label) {
	const contentLength = Number(response.headers.get("content-length") ?? "0");
	if (contentLength > MAX_REMOTE_CATALOG_BYTES) {
		throw new Error(`${label} catalog is too large: ${contentLength} bytes exceeds ${MAX_REMOTE_CATALOG_BYTES}`);
	}
	if (!response.body) {
		const body = await response.text();
		const bytes = Buffer.byteLength(body, "utf8");
		if (bytes > MAX_REMOTE_CATALOG_BYTES) {
			throw new Error(`${label} catalog is too large: ${bytes} bytes exceeds ${MAX_REMOTE_CATALOG_BYTES}`);
		}
		return body;
	}
	const reader = response.body.getReader();
	const chunks = [];
	let bytes = 0;
	try {
		for (;;) {
			const { value, done } = await reader.read();
			if (done) break;
			if (!value) continue;
			bytes += value.byteLength;
			if (bytes > MAX_REMOTE_CATALOG_BYTES) {
				await reader.cancel().catch(() => undefined);
				throw new Error(`${label} catalog is too large: ${bytes} bytes exceeds ${MAX_REMOTE_CATALOG_BYTES}`);
			}
			chunks.push(value);
		}
	} finally {
		reader.releaseLock();
	}
	return new TextDecoder().decode(Buffer.concat(chunks));
}

async function fetchCatalog(url, label, options = {}) {
	let response;
	try {
		response = await fetch(url, {
			headers: authHeaders(url, options),
			signal: AbortSignal.timeout(5_000),
			redirect: "error",
		});
	} catch (error) {
		const reason = error instanceof Error ? error.message : String(error);
		throw new Error(`Failed to fetch ${label} catalog from ${url}: ${reason}`);
	}
	if (!response.ok) {
		const privateRepoHint =
			response.status === 401 || response.status === 404
				? " The catalog repo is private; set GITHUB_TOKEN or PRIME_CATALOG_REPO_TOKEN."
				: "";
		throw new Error(`Failed to fetch ${label} catalog from ${url}: HTTP ${response.status}.${privateRepoHint}`);
	}
	const body = await readBoundedResponseText(response, label);
	return body.endsWith("\n") ? body : `${body}\n`;
}

function readJson(path) {
	return JSON.parse(readFileSync(path, "utf8"));
}

export function validateBundledModelCatalog(path, options = {}) {
	const catalog = readJson(path);
	if (catalog?.schemaVersion !== 1 || !Array.isArray(catalog.models)) throw new Error(`Invalid bundled model catalog: ${path}`);
	const tuples = new Set();
	for (const model of catalog.models) {
		if (!model || typeof model !== "object") continue;
		if (typeof model.provider === "string" && typeof model.api === "string" && typeof model.baseUrl === "string") {
			tuples.add(JSON.stringify([model.provider, model.api, model.baseUrl]));
		}
	}
	if (!options.allowSmallFixture && tuples.size < MIN_BUNDLED_MODEL_TRANSPORT_TUPLES) {
		throw new Error(`Bundled model catalog has ${tuples.size} transport tuples; expected at least ${MIN_BUNDLED_MODEL_TRANSPORT_TUPLES}`);
	}
	return { models: catalog.models.length, transportTuples: tuples.size };
}

export function validateBundledMcpCatalog(path, options = {}) {
	const catalog = readJson(path);
	if (catalog?.version !== 2 || !Array.isArray(catalog.entries)) throw new Error(`Invalid bundled MCP service catalog: ${path}`);
	if (!options.allowSmallFixture && catalog.entries.length < MIN_BUNDLED_MCP_SERVICES) {
		throw new Error(`Bundled MCP service catalog has ${catalog.entries.length} entries; expected at least ${MIN_BUNDLED_MCP_SERVICES}`);
	}
	return { services: catalog.entries.length };
}

export function validateBundledCatalogDir(directory, options = {}) {
	return {
		models: validateBundledModelCatalog(join(directory, "models.bundled.json"), options),
		mcpServices: validateBundledMcpCatalog(join(directory, "mcp-services.bundled.json"), options),
	};
}

function fixtureModel({ id, name, provider, api, baseUrl, reasoning = false, input = ["text"] }) {
	return {
		id,
		name,
		api,
		provider,
		baseUrl,
		reasoning,
		input,
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 128000,
		maxTokens: 4096,
	};
}

function fixtureMcpEntry({ server, label, url, auth = "oauth", setup = { status: "ready", readiness: "oauth-ready" }, transport }) {
	return {
		server,
		service: server,
		label,
		url,
		category: "Fixture",
		aliases: [],
		publisher: "Prime Intellect",
		transport: transport ?? { type: "http", url },
		auth: { strategy: auth, clientRegistration: auth === "oauth" ? "dynamic" : "unknown" },
		setup,
		verification: { status: "unverified" },
		legacyBuiltin: false,
		provenance: [{ source: "prime" }],
		...(auth === "oauth" ? { oauth: { kind: "oauth" } } : {}),
	};
}

function fixtureCatalogBodies() {
	return {
		models: `${JSON.stringify(
			{
				schemaVersion: 1,
				models: [
					fixtureModel({
						id: "fixture-gpt",
						name: "Fixture GPT",
						provider: "openai",
						api: "openai-responses",
						baseUrl: "https://api.openai.com/v1",
						reasoning: true,
					}),
					fixtureModel({
						id: "fixture-claude",
						name: "Fixture Claude",
						provider: "anthropic",
						api: "anthropic-messages",
						baseUrl: "https://api.anthropic.com",
						input: ["text", "image"],
					}),
					fixtureModel({
						id: "fixture-prime",
						name: "Fixture Prime",
						provider: "prime-inference",
						api: "openai-completions",
						baseUrl: "https://api.primeintellect.ai/api/v1",
					}),
					fixtureModel({
						id: "fixture-gemini",
						name: "Fixture Gemini",
						provider: "google-gemini",
						api: "gemini",
						baseUrl: "https://generativelanguage.googleapis.com/v1beta",
					}),
				],
			},
			null,
			2,
		)}\n`,
		mcpServices: `${JSON.stringify(
			{
				version: 2,
				counts: { entries: 4 },
				entries: [
					fixtureMcpEntry({ server: "linear", label: "Linear", url: "https://mcp.linear.app/mcp" }),
					fixtureMcpEntry({ server: "notion", label: "Notion", url: "https://mcp.notion.com/mcp" }),
					fixtureMcpEntry({
						server: "github",
						label: "GitHub",
						url: "https://api.githubcopilot.com/mcp/",
						auth: "api_key",
						setup: {
							status: "requires-setup",
							reason: "Paste a GitHub personal access token.",
							readiness: "user-setup",
							requirement: "bearer-token",
							fields: [
								{
									id: "GITHUB_PAT_TOKEN",
									label: "GitHub personal access token",
									required: true,
									kind: "bearer-token",
									credentialSet: "github-pat",
								},
							],
						},
					}),
					fixtureMcpEntry({
						server: "local-tools",
						label: "Local Tools",
						url: "",
						auth: "none",
						setup: { status: "requires-setup", reason: "Requires a local stdio runtime.", readiness: "user-setup", requirement: "local-runtime" },
						transport: { type: "stdio", servers: [{ name: "local-tools", command: "local-tools-mcp" }] },
					}),
				],
			},
			null,
			2,
		)}\n`,
	};
}

function copyCatalogSourcesToTargets(sourceDir, outDir) {
	const paths = catalogSourcePaths(resolve(sourceDir));
	const targets = bundledTargets(outDir);
	cpSync(paths.models, targets.models);
	cpSync(paths.mcpServices, targets.mcpServices);
}

export async function generateBundledCatalogAssets(options = {}) {
	const outDir = resolve(options.outDir ?? join(packageDir, "dist"));
	mkdirSync(outDir, { recursive: true });
	const targets = bundledTargets(outDir);
	if (options.fixture) {
		const fixture = fixtureCatalogBodies();
		writeFileSync(targets.models, fixture.models);
		writeFileSync(targets.mcpServices, fixture.mcpServices);
	} else if (options.catalogDir) {
		copyCatalogSourcesToTargets(options.catalogDir, outDir);
	} else {
		const [modelBody, mcpServiceBody] = await Promise.all([
			fetchCatalog(options.modelsUrl ?? DEFAULT_MODEL_CATALOG_URL, "model", options),
			fetchCatalog(options.mcpServicesUrl ?? DEFAULT_MCP_SERVICE_CATALOG_URL, "MCP service", options),
		]);
		writeFileSync(targets.models, modelBody);
		writeFileSync(targets.mcpServices, mcpServiceBody);
	}
	return validateBundledCatalogDir(outDir, { allowSmallFixture: options.allowSmallFixture === true || options.fixture === true });
}

export async function copySourceCatalogAssets(options = {}) {
	const outDir = resolve(options.outDir ?? join(packageDir, "dist"));
	mkdirSync(outDir, { recursive: true });
	const targets = bundledTargets(outDir);
	// Prefer the generated source catalog over stale dist copies: incremental
	// builds must package the freshly generated snapshot, not whatever dist
	// happened to keep from a previous build.
	const sourceDir = join(packageDir, "catalog");
	const allSourcesPresent = bundledCatalogFiles.every((file) => existsSync(join(sourceDir, file)));
	if (allSourcesPresent) {
		cpSync(join(sourceDir, "models.bundled.json"), targets.models);
		cpSync(join(sourceDir, "mcp-services.bundled.json"), targets.mcpServices);
		return validateBundledCatalogDir(outDir, options);
	}

	const allTargetsPresent = bundledCatalogFiles.every((file) => existsSync(join(outDir, file)));
	if (allTargetsPresent) return validateBundledCatalogDir(outDir, options);

	try {
		const [modelBody, mcpServiceBody] = await Promise.all([
			fetchCatalog(options.modelsUrl ?? DEFAULT_MODEL_CATALOG_URL, "model", options),
			fetchCatalog(options.mcpServicesUrl ?? DEFAULT_MCP_SERVICE_CATALOG_URL, "MCP service", options),
		]);
		writeFileSync(targets.models, modelBody);
		writeFileSync(targets.mcpServices, mcpServiceBody);
		return validateBundledCatalogDir(outDir, options);
	} catch (error) {
		const message =
			"Missing generated catalog assets and failed to fetch public catalog assets. Run `npm run catalog:assets -- --catalog-dir /path/to/prime-agent-catalog` or set PRIME_CATALOG_REPO_TOKEN and run `npm run catalog:assets`.";
		if (options.optional) {
			const reason = error instanceof Error ? error.message : String(error);
			console.warn(`${message} ${reason} Continuing without bundled catalog assets; source runs will use the compiled fallback and runtime cache refresh.`);
			return { skipped: true, reason: "missing-source-catalog" };
		}
		throw new Error(message, { cause: error });
	}
}

function parseArgs(argv) {
	let [command, ...rest] = argv;
	if (!command || command.startsWith("--")) {
		command = "generate";
		rest = argv;
	}
	const options = {};
	for (let i = 0; i < rest.length; i += 1) {
		const arg = rest[i];
		if (arg === "--out") options.outDir = rest[++i];
		else if (arg === "--catalog-dir") options.catalogDir = rest[++i];
		else if (arg === "--models-url") options.modelsUrl = rest[++i];
		else if (arg === "--mcp-services-url") options.mcpServicesUrl = rest[++i];
		else if (arg === "--fixture") options.fixture = true;
		else if (arg === "--allow-small-fixture") options.allowSmallFixture = true;
		else if (arg === "--allow-token-for-url") options.allowTokenForUrl = true;
		else if (arg === "--optional") options.optional = true;
		else throw new Error(`Unknown catalog-assets argument: ${arg}`);
	}
	return { command, options };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
	const { command, options } = parseArgs(process.argv.slice(2));
	const result =
		command === "generate"
			? await generateBundledCatalogAssets(options)
			: command === "copy-source"
				? await copySourceCatalogAssets(options)
				: command === "verify"
					? validateBundledCatalogDir(resolve(options.outDir ?? join(packageDir, "dist")), options)
					: undefined;
	if (!result) throw new Error("Usage: catalog-assets.mjs [generate|copy-source|verify] [--out DIR] [--catalog-dir DIR] [--fixture]");
	console.log(JSON.stringify(result, null, 2));
}
