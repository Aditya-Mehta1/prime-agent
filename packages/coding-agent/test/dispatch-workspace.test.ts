import { execFileSync } from "node:child_process";
import { chmod, lstat, mkdir, mkdtemp, readFile, readlink, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { captureLocalProject, loadDispatchBinding } from "../src/core/dispatch/workspace.js";

describe("dispatch working tree capture", () => {
	it("preserves current tracked edits, deletions, new files, links and modes without changing the source index", async () => {
		const directory = await mkdtemp(join(tmpdir(), "dispatch-snapshot-"));
		const repo = join(directory, "repo");
		await mkdir(repo);
		const git = (...args: string[]) => execFileSync("git", ["-C", repo, ...args], { encoding: "utf8" });
		try {
			git("init", "-q");
			await writeFile(join(repo, ".gitignore"), ".env\nignored/\nforced-new\n");
			await writeFile(join(repo, "tracked"), "initial");
			await writeFile(join(repo, "deleted"), "delete");
			await mkdir(join(repo, "sub"));
			await writeFile(join(repo, "sub", "script"), "#!/bin/sh\n");
			await chmod(join(repo, "sub", "script"), 0o755);
			git("add", ".");
			git("-c", "user.name=Fixture", "-c", "user.email=fixture@localhost", "commit", "-qm", "base");
			git("remote", "add", "origin", "https://user:fixture-secret@example.invalid/repo.git");
			await writeFile(join(repo, "tracked"), "staged");
			git("add", "tracked");
			await writeFile(join(repo, "tracked"), "working tree");
			await writeFile(join(repo, "forced-new"), "explicitly tracked");
			git("add", "-f", "forced-new");
			await rm(join(repo, "deleted"));
			await writeFile(join(repo, "new file\nname"), "new");
			await symlink("tracked", join(repo, "link"));
			await writeFile(join(repo, ".env"), "ignored fixture");
			await mkdir(join(repo, "ignored"));
			await writeFile(join(repo, "ignored", "cache"), "cache");
			const before = git("status", "--porcelain=v1");
			const index = await readFile(join(repo, ".git", "index"));
			const snapshot = join(directory, "snapshot");
			const metadata = await captureLocalProject(join(repo, "sub"), snapshot);
			expect(metadata.cwdRelative).toBe("sub");
			expect(metadata.origin).toBe("https://example.invalid/repo.git");
			expect(metadata.sourceHead).toBe(git("rev-parse", "HEAD").trim());
			expect(await readFile(join(snapshot, "tree", "tracked"), "utf8")).toBe("working tree");
			expect(await readFile(join(snapshot, "tree", "forced-new"), "utf8")).toBe("explicitly tracked");
			expect(await readFile(join(snapshot, "tree", "new file\nname"), "utf8")).toBe("new");
			expect(await readlink(join(snapshot, "tree", "link"))).toBe("tracked");
			expect((await lstat(join(snapshot, "tree", "sub", "script"))).mode & 0o111).toBe(0o111);
			for (const absent of ["deleted", ".env", "ignored", ".git"]) {
				await expect(lstat(join(snapshot, "tree", absent))).rejects.toMatchObject({ code: "ENOENT" });
			}
			expect(await readFile(join(repo, ".git", "index"))).toEqual(index);
			expect(git("status", "--porcelain=v1")).toBe(before);
			const restored = join(directory, "restored");
			execFileSync("git", ["clone", "-q", join(snapshot, "base.bundle"), restored]);
			expect(await readFile(join(restored, "tracked"), "utf8")).toBe("initial");
		} finally {
			await rm(directory, { recursive: true, force: true });
		}
	});

	it("loads persisted identity and rejects an unsupported binding version", async () => {
		const directory = await mkdtemp(join(tmpdir(), "dispatch-binding-"));
		try {
			expect(loadDispatchBinding(directory)).toBeUndefined();
			await writeFile(
				join(directory, "dispatch.json"),
				JSON.stringify({
					version: 2,
					ownsBox: true,
					boxId: "box-retained",
					guestCwd: "/repo",
				}),
			);
			expect(loadDispatchBinding(directory)?.boxId).toBe("box-retained");
			await writeFile(
				join(directory, "dispatch.json"),
				JSON.stringify({ version: 1, boxId: "box-retained", guestCwd: "/repo" }),
			);
			expect(() => loadDispatchBinding(directory)).toThrow("Invalid dispatch binding");
		} finally {
			await rm(directory, { recursive: true, force: true });
		}
	});
});
