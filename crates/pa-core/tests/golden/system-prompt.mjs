#!/usr/bin/env node
/**
 * Golden system-prompt corpus generator (lane model-surface-parity).
 *
 * Assembles the REAL TypeScript system prompt (from the /tmp/pa-golden copy
 * of prime-agent) for the same fixture state the Rust golden test uses:
 * the vendored bundled skills directory, a fixture cwd, and a fixture
 * conversation-log path. Output: corpus/system-prompt.json.
 *
 * Run: node system-prompt.mjs <skills-dir>
 * Determinism: two runs over the same skills dir produce identical files.
 */
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createRequire } from "node:module";

const PA_ROOT = "/tmp/pa-golden";
const OUT = path.join(path.dirname(fileURLToPath(import.meta.url)), "corpus", "system-prompt.json");

const req = createRequire(path.join(PA_ROOT, "package.json"));
const { createJiti } = req("jiti");
const jiti = createJiti(pathToFileURL(path.join(PA_ROOT, "harness-entry.mjs")).href, {
	interopDefaults: true,
	alias: {
		"@earendil-works/pi-agent-core": path.join(PA_ROOT, "packages/agent/src/index.ts"),
		"@earendil-works/pi-ai": path.join(PA_ROOT, "packages/ai/src/index.ts"),
		"@earendil-works/pi-tui": path.join(PA_ROOT, "packages/tui/src/index.ts"),
	},
});

const skillsDir = process.argv[2];
if (!skillsDir) {
	console.error("usage: node system-prompt.mjs <skills-dir>");
	process.exit(1);
}

async function main() {
	const skillsMod = await jiti.import(path.join(PA_ROOT, "packages/coding-agent/src/core/skills.ts"));
	const promptMod = await jiti.import(path.join(PA_ROOT, "packages/coding-agent/src/core/system-prompt.ts"));
	const loadSkillsFromDir = skillsMod.loadSkillsFromDir ?? skillsMod.default?.loadSkillsFromDir;
	const buildSystemPrompt = promptMod.buildSystemPrompt ?? promptMod.default?.buildSystemPrompt;
	const { skills } = loadSkillsFromDir({ dir: skillsDir, source: "package" });
	const systemPrompt = buildSystemPrompt({
		cwd: "/w",
		messagesPath: "/w/sessions/fixture-session.jsonl",
		skills,
		selectedTools: ["ipython"],
		allowRecursion: true,
		rlmDepth: 0,
		rlmParentAgent: undefined,
		contextFiles: [],
		genericMcpServers: [],
		promptGuidelines: [],
	}).replaceAll(skillsDir, "<skills-dir>");
	fs.mkdirSync(path.dirname(OUT), { recursive: true });
	fs.writeFileSync(
		OUT,
		JSON.stringify(
			{
				fixture: {
					cwd: "/w",
					messagesPath: "/w/sessions/fixture-session.jsonl",
					selectedTools: ["ipython"],
					skillsSource: "bundled skills directory (<skills-dir>)",
				},
				skillCount: skills.length,
				systemPrompt,
			},
			null,
			"\t",
		) + "\n",
	);
	console.log(`wrote ${OUT} (${systemPrompt.length} chars, ${skills.length} skills)`);
}

main().catch((error) => {
	console.error(error);
	process.exit(1);
});
