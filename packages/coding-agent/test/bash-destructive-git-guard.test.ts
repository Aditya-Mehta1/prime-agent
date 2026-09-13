import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
	BASH_DESTRUCTIVE_GIT_BYPASS_ENV,
	type BashOperations,
	createBashTool,
	isDestructiveGitDiscardCommand,
} from "../src/core/tools/bash.js";

function runGit(cwd: string, ...args: string[]): void {
	execFileSync("git", args, { cwd, stdio: ["ignore", "ignore", "pipe"] });
}

/** Create a git repository with one committed file plus two uncommitted changes. */
function initDirtyGitRepo(root: string): void {
	runGit(root, "init");
	runGit(root, "config", "user.email", "test@example.com");
	runGit(root, "config", "user.name", "Test");
	runGit(root, "config", "commit.gpgsign", "false");
	writeFileSync(join(root, "tracked.txt"), "committed\n");
	runGit(root, "add", "tracked.txt");
	runGit(root, "commit", "-m", "init");
	writeFileSync(join(root, "tracked.txt"), "modified\n");
	writeFileSync(join(root, "untracked.txt"), "uncommitted\n");
}

describe("isDestructiveGitDiscardCommand", () => {
	it.each([
		"git checkout -- .",
		"git checkout .",
		"git checkout HEAD -- .",
		"git restore .",
		"git restore --source=HEAD~1 .",
		"git clean -f",
		"git clean -fd",
		"git clean -fdx",
		"git clean --force",
		"git reset --hard",
		"git reset --hard HEAD~1",
		"git checkout -b tmp 2>/dev/null; git checkout -- .",
		"git checkout main && git reset --hard",
		"echo start\ngit clean -fd",
		"npm test & git clean -fd &",
		"git checkout :/",
		"git checkout -- :/",
		"git checkout HEAD -- :/",
		"git restore :/",
		"git restore -s@ .",
		"git restore -s@ :/",
		"git restore --source=HEAD :/",
		"git restore -s HEAD~1 :/",
		"git restore -- .",
		"git checkout -- ./",
		"git checkout ./",
		"git restore ./",
		"git -C sub reset --hard",
		"git --git-dir=sub/.git reset --hard",
	])("matches %s", (command) => {
		expect(isDestructiveGitDiscardCommand(command)).toBe(true);
	});

	it.each([
		"git status",
		"git log --oneline",
		"git checkout -b new-branch",
		"git checkout main",
		"git checkout -- single-file.txt",
		"git checkout ./nested",
		"git restore --staged .",
		"git restore --staged :/",
		"git restore single-file.txt",
		"git clean -n",
		"git clean --dry-run",
		"git clean -d",
		"git reset",
		"git reset --soft HEAD~1",
		"git stash",
		"git add .",
		"echo hello world",
		"npm run check",
	])("does not match %s", (command) => {
		expect(isDestructiveGitDiscardCommand(command)).toBe(false);
	});
});

describe("bash tool destructive-git dirty-tree guard", () => {
	let testDir: string;

	beforeEach(() => {
		testDir = join(tmpdir(), `bash-git-guard-test-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		mkdirSync(testDir, { recursive: true });
	});

	afterEach(() => {
		delete process.env[BASH_DESTRUCTIVE_GIT_BYPASS_ENV];
		rmSync(testDir, { recursive: true, force: true });
	});

	it.each(["git checkout -- .", "git checkout .", "git clean -fd", "git reset --hard", "git restore ."])(
		"refuses %s on a dirty tree and preserves the work",
		async (command) => {
			initDirtyGitRepo(testDir);
			const bash = createBashTool(testDir);

			await expect(bash.execute(`guard-${command}`, { command })).rejects.toThrow(
				/Refusing to run this destructive git command/,
			);

			expect(readModifiedTracked()).toBe("modified\n");
			expect(existsSync(join(testDir, "untracked.txt"))).toBe(true);
		},
	);

	it("lists the dirty paths and both bypasses in the refusal", async () => {
		initDirtyGitRepo(testDir);
		const bash = createBashTool(testDir);

		const error = await bash
			.execute("guard-refusal-text", { command: "git checkout -- ." })
			.catch((err: Error) => err);

		expect(error).toBeInstanceOf(Error);
		const message = (error as Error).message;
		expect(message).toContain("2 uncommitted change(s)");
		expect(message).toContain("tracked.txt");
		expect(message).toContain("untracked.txt");
		expect(message).toContain("allowDestructiveGit: true");
		expect(message).toContain(BASH_DESTRUCTIVE_GIT_BYPASS_ENV);
	});

	it("elides long dirty path lists", async () => {
		initDirtyGitRepo(testDir);
		for (let i = 0; i < 12; i++) writeFileSync(join(testDir, `extra-${i}.txt`), "x\n");
		const bash = createBashTool(testDir);

		const error = await bash.execute("guard-elide", { command: "git checkout -- ." }).catch((err: Error) => err);
		const message = (error as Error).message;

		expect(message).toContain("... and 4 more");
	});

	it("runs the discard without a probe when the tree is clean", async () => {
		initDirtyGitRepo(testDir);
		runGit(testDir, "add", "-A");
		runGit(testDir, "commit", "-m", "second");
		const bash = createBashTool(testDir);

		await expect(bash.execute("guard-clean", { command: "git checkout -- ." })).resolves.toBeDefined();
	});

	it("bypasses the guard with allowDestructiveGit: true and discards", async () => {
		initDirtyGitRepo(testDir);
		const bash = createBashTool(testDir);

		await expect(
			bash.execute("guard-bypass-arg", { command: "git reset --hard", allowDestructiveGit: true }),
		).resolves.toBeDefined();

		expect(readModifiedTracked()).toBe("committed\n");
	});

	it(`bypasses the guard with ${BASH_DESTRUCTIVE_GIT_BYPASS_ENV}=1`, async () => {
		initDirtyGitRepo(testDir);
		process.env[BASH_DESTRUCTIVE_GIT_BYPASS_ENV] = "1";
		const bash = createBashTool(testDir);

		await expect(bash.execute("guard-bypass-env", { command: "git reset --hard" })).resolves.toBeDefined();

		expect(readModifiedTracked()).toBe("committed\n");
	});

	it("fails open outside a git repository", async () => {
		const bash = createBashTool(testDir);

		const error = await bash.execute("guard-no-repo", { command: "git checkout -- ." }).then(
			() => undefined,
			(err: Error) => err,
		);

		expect(error).toBeInstanceOf(Error);
		expect((error as Error).message).not.toMatch(/Refusing to run/);
	});

	it("runs no probe for non-discard commands", async () => {
		initDirtyGitRepo(testDir);
		const calls: string[] = [];
		const operations: BashOperations = {
			exec: async (command, _cwd, _options) => {
				calls.push(command);
				return { exitCode: 0 };
			},
		};
		const bash = createBashTool(testDir, { operations });

		await bash.execute("guard-no-probe", { command: "echo hi" });

		expect(calls).toEqual(["echo hi"]);
	});

	it("fails open when the probe itself fails", async () => {
		const calls: string[] = [];
		const operations: BashOperations = {
			exec: async (command, _cwd, { onData }) => {
				calls.push(command);
				if (command === "git status --porcelain") {
					onData(Buffer.from("fatal: not a git repository\n", "utf8"));
					return { exitCode: 128 };
				}
				return { exitCode: 0 };
			},
		};
		const bash = createBashTool(testDir, { operations });

		await expect(bash.execute("guard-probe-fail", { command: "git checkout -- ." })).resolves.toBeDefined();
		expect(calls).toEqual(["git status --porcelain", "git checkout -- ."]);
	});

	it("prepends the command prefix to the probe for shell-setup parity", async () => {
		const calls: string[] = [];
		const operations: BashOperations = {
			exec: async (command, _cwd, _options) => {
				calls.push(command);
				return { exitCode: 0 };
			},
		};
		const bash = createBashTool(testDir, { commandPrefix: "export GUARD_TEST_VAR=1", operations });

		await expect(bash.execute("guard-prefix", { command: "git checkout -- ." })).resolves.toBeDefined();

		expect(calls).toEqual([
			"export GUARD_TEST_VAR=1\ngit status --porcelain",
			"export GUARD_TEST_VAR=1\ngit checkout -- .",
		]);
	});

	it("follows cd relocations: refuses a discard in a dirty nested repository", async () => {
		const sub = join(testDir, "sub");
		mkdirSync(sub);
		initDirtyGitRepo(sub);
		const bash = createBashTool(testDir);

		const error = await bash.execute("guard-cd-dirty", { command: "cd sub && git reset --hard" }).then(
			() => undefined,
			(err: Error) => err,
		);

		expect(error).toBeInstanceOf(Error);
		const message = (error as Error).message;
		expect(message).toMatch(/Refusing to run this destructive git command/);
		expect(message).toContain("tracked.txt");
		expect(readFileSync(join(sub, "tracked.txt"), "utf-8")).toBe("modified\n");
	});

	it("follows git -C relocations: refuses a discard in a dirty nested repository", async () => {
		const sub = join(testDir, "sub");
		mkdirSync(sub);
		initDirtyGitRepo(sub);
		const bash = createBashTool(testDir);

		await expect(bash.execute("guard-git-c-dirty", { command: "git -C sub reset --hard" })).rejects.toThrow(
			/tracked\.txt/,
		);
		expect(readFileSync(join(sub, "tracked.txt"), "utf-8")).toBe("modified\n");
	});

	it("allows a relocated discard when the target repository is clean", async () => {
		const sub = join(testDir, "sub");
		mkdirSync(sub);
		initDirtyGitRepo(sub);
		runGit(sub, "add", "-A");
		runGit(sub, "commit", "-m", "second");
		const bash = createBashTool(testDir);

		await expect(bash.execute("guard-cd-clean", { command: "cd sub && git reset --hard" })).resolves.toBeDefined();
	});

	it("probes every discarded repository in multi-discard commands", async () => {
		const sub = join(testDir, "sub");
		mkdirSync(sub);
		initDirtyGitRepo(sub);
		const bash = createBashTool(testDir);

		await expect(
			bash.execute("guard-multi-discard", { command: "git checkout -- . && cd sub && git reset --hard" }),
		).rejects.toThrow(/Refusing to run this destructive git command/);
	});

	it("conservatively refuses relocations it cannot replay safely", async () => {
		const bash = createBashTool(testDir);

		for (const command of [
			"cd $(pwd)/sub && git reset --hard",
			"git --git-dir=sub/.git reset --hard",
			"(cd sub && git reset --hard)",
			"cd sub || git reset --hard",
			"pushd sub && git reset --hard",
		]) {
			const error = await bash.execute(`guard-unresolvable`, { command }).then(
				() => undefined,
				(err: Error) => err,
			);
			expect(error).toBeInstanceOf(Error);
			expect((error as Error).message).toContain("changes directory (or repository) first");
		}
	});

	it("replays cd chains in the probe", async () => {
		const calls: string[] = [];
		const operations: BashOperations = {
			exec: async (command, _cwd, _options) => {
				calls.push(command);
				return { exitCode: 0 };
			},
		};
		const bash = createBashTool(testDir, { operations });

		await bash.execute("guard-cd-chain", { command: "cd a && cd b && git checkout -- ." });

		expect(calls).toEqual(["cd a && cd b && git status --porcelain", "cd a && cd b && git checkout -- ."]);
	});

	it("replays git -C in the probe", async () => {
		const calls: string[] = [];
		const operations: BashOperations = {
			exec: async (command, _cwd, _options) => {
				calls.push(command);
				return { exitCode: 0 };
			},
		};
		const bash = createBashTool(testDir, { operations });

		await bash.execute("guard-git-c-probe", { command: "git -C sub reset --hard" });

		expect(calls).toEqual(["git -C sub status --porcelain", "git -C sub reset --hard"]);
	});

	it("runs the probe through the spawn hook like the discard itself", async () => {
		const calls: string[] = [];
		const operations: BashOperations = {
			exec: async (command, _cwd, _options) => {
				calls.push(command);
				return { exitCode: 0 };
			},
		};
		const bash = createBashTool(testDir, {
			operations,
			spawnHook: (ctx) => ({ ...ctx, command: `source ~/.profile\n${ctx.command}` }),
		});

		await bash.execute("guard-hook", { command: "git checkout -- ." });

		expect(calls).toEqual(["source ~/.profile\ngit status --porcelain", "source ~/.profile\ngit checkout -- ."]);
	});

	it("propagates aborts raised while probing", async () => {
		const operations: BashOperations = {
			exec: async (command, _cwd, _options) => {
				if (command === "git status --porcelain") throw new Error("aborted");
				return { exitCode: 0 };
			},
		};
		const bash = createBashTool(testDir, { operations });

		await expect(bash.execute("guard-abort", { command: "git checkout -- ." })).rejects.toThrow("aborted");
	});

	function readModifiedTracked(): string {
		return readFileSync(join(testDir, "tracked.txt"), "utf-8");
	}
});
