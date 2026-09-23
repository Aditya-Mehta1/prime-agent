export interface DispatchBinding {
	version: 2;
	ownsBox: boolean;
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
}
