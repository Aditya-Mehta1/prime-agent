import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

export const PROJECT_INPUT_NAME = "prime_project";

const INITIAL_BRIEF = "# Project\n\n## Goal\n\n## Now\n\n## Details\n";

export function ensureForegroundProjectWorkspace(sessionArtifactDir: string | undefined): string | undefined {
	if (!sessionArtifactDir) return undefined;
	const workspace = join(sessionArtifactDir, "workspace");
	mkdirSync(join(workspace, "project"), { recursive: true });
	const brief = join(workspace, "PROJECT.md");
	if (!existsSync(brief)) {
		try {
			writeFileSync(brief, INITIAL_BRIEF, { flag: "wx" });
		} catch (error) {
			if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
		}
	}
	return workspace;
}
