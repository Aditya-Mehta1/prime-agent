export interface DispatchBinding {
	version: 1;
	appId: string;
	boxId: string;
	guestRepoDir: string;
	guestCwd: string;
	guestStateDir: string;
	guestPython: string;
	guestSkillsDir: string;
	hostResourceDir: string;
	sourceHead: string;
	baselineCommit: string;
	initialBranch: string;
	inputs: Record<string, string>;
	model: { provider: string; id: string };
}
