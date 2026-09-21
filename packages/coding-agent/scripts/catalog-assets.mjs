#!/usr/bin/env node
import { cpSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const packageDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");

export const bundledCatalogFiles = ["models.bundled.json", "mcp-services.bundled.json"];
export const MIN_BUNDLED_MODEL_TRANSPORT_TUPLES = 42;
export const MIN_BUNDLED_MCP_SERVICES = 20;
export const DEFAULT_MODEL_CATALOG_URL =
	"https://raw.githubusercontent.com/PrimeIntellect-ai/prime-agent-catalog/main/models/catalog.v1.json";
export const DEFAULT_MCP_SERVICE_CATALOG_URL =
	"https://raw.githubusercontent.com/PrimeIntellect-ai/prime-agent-catalog/main/plugins/catalog.v2.json";

function catalogSourcePaths(catalogDir) {
	return {
		models: join(catalogDir, "models", "catalog.v1.json"),
		mcpServices: join(catalogDir, "plugins", "catalog.v2.json"),
	};
}

async function fetchCatalog(url, label) {
	const response = await fetch(url, {
		headers: { accept: "application/json" },
		signal: AbortSignal.timeout(10_000),
		redirect: "error",
	});
	if (!response.ok) throw new Error(`Failed to fetch ${label} catalog: HTTP ${response.status}`);
	return `${await response.text()}
`;
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

export async function generateBundledCatalogAssets(options = {}) {
	const outDir = resolve(options.outDir ?? join(packageDir, "dist"));
	mkdirSync(outDir, { recursive: true });
	const modelTarget = join(outDir, "models.bundled.json");
	const mcpTarget = join(outDir, "mcp-services.bundled.json");
	if (options.catalogDir) {
		const paths = catalogSourcePaths(resolve(options.catalogDir));
		cpSync(paths.models, modelTarget);
		cpSync(paths.mcpServices, mcpTarget);
	} else if (!options.modelsUrl && !options.mcpServicesUrl) {
		cpSync(join(packageDir, "catalog", "models.bundled.json"), modelTarget);
		cpSync(join(packageDir, "catalog", "mcp-services.bundled.json"), mcpTarget);
	} else {
		const [modelBody, mcpServiceBody] = await Promise.all([
			fetchCatalog(options.modelsUrl ?? DEFAULT_MODEL_CATALOG_URL, "model"),
			fetchCatalog(options.mcpServicesUrl ?? DEFAULT_MCP_SERVICE_CATALOG_URL, "MCP service"),
		]);
		writeFileSync(modelTarget, modelBody);
		writeFileSync(mcpTarget, mcpServiceBody);
	}
	return validateBundledCatalogDir(outDir, { allowSmallFixture: options.allowSmallFixture === true });
}

function parseArgs(argv) {
	const [command, ...rest] = argv;
	const options = {};
	for (let i = 0; i < rest.length; i += 1) {
		const arg = rest[i];
		if (arg === "--out") options.outDir = rest[++i];
		else if (arg === "--catalog-dir") options.catalogDir = rest[++i];
		else if (arg === "--models-url") options.modelsUrl = rest[++i];
		else if (arg === "--mcp-services-url") options.mcpServicesUrl = rest[++i];
		else if (arg === "--allow-small-fixture") options.allowSmallFixture = true;
		else throw new Error(`Unknown catalog-assets argument: ${arg}`);
	}
	return { command, options };
}

if (import.meta.url === `file://${process.argv[1]}`) {
	const { command, options } = parseArgs(process.argv.slice(2));
	const result = command === "generate" ? await generateBundledCatalogAssets(options) : command === "verify" ? validateBundledCatalogDir(resolve(options.outDir ?? join(packageDir, "dist")), options) : undefined;
	if (!result) throw new Error("Usage: catalog-assets.mjs <generate|verify> [--out DIR] [--catalog-dir DIR]");
	console.log(JSON.stringify(result, null, 2));
}
