import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { cp, lstat, mkdir, mkdtemp, readdir, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, join, relative, resolve, sep } from "node:path";
import { promisify } from "node:util";
import { getBundledSkillsDir, getPackageDir } from "../../config.js";
import { DEFAULT_RLM_EXTRA_UV_ARGS } from "../kernel/bootstrap.js";
import { SailClient, SailHttpError } from "./sail-client.js";
import type { DispatchBinding } from "./types.js";

const executeFile = promisify(execFile);
const GUEST_REPO = "/workspace/repo";
const GUEST_INSTALL = "/opt/prime-dispatch";
const GUEST_STATE = "/var/lib/prime-dispatch/state";

export function shellQuote(value: string): string {
	return `'${value.replaceAll("'", "'\\''")}'`;
}

async function git(cwd: string, args: string[]): Promise<string> {
	const result = await executeFile("git", ["-C", cwd, ...args], { maxBuffer: 64 * 1024 * 1024 });
	return result.stdout;
}

interface CapturedProject {
	sourceHead: string;
	cwdRelative: string;
	origin: string;
}

function safeOrigin(value: string): string {
	const origin = value.trim();
	const github = /^(?:git@github\.com:|ssh:\/\/git@github\.com\/)(.+)$/.exec(origin);
	if (github) return `https://github.com/${github[1]}`;
	if (/^https?:\/\//.test(origin)) {
		const url = new URL(origin);
		url.username = "";
		url.password = "";
		return url.toString();
	}
	return /^(git@[^:]+:|ssh:\/\/git@)/.test(origin) ? origin : "";
}

/** Capture live files without changing the user's Git index or copying .git config/hooks. */
export async function captureLocalProject(sourceCwd: string, destination: string): Promise<CapturedProject> {
	const sourceRoot = (await git(sourceCwd, ["rev-parse", "--show-toplevel"])).trim();
	const sourceHead = (await git(sourceRoot, ["rev-parse", "HEAD"])).trim();
	const entries = await git(sourceRoot, ["ls-files", "--stage"]);
	if (/^160000 /m.test(entries))
		throw new Error("Dispatch MVP does not copy Git submodules; use a plain working tree");
	const cwdRelative = relative(sourceRoot, await realpath(sourceCwd));
	await mkdir(join(destination, "tree"), { recursive: true });
	const files = (await git(sourceRoot, ["ls-files", "-z", "--cached", "--others", "--exclude-standard"]))
		.split("\0")
		.filter(Boolean);
	for (const file of new Set(files)) {
		const source = join(sourceRoot, file);
		const target = join(destination, "tree", file);
		let stat: Awaited<ReturnType<typeof lstat>>;
		try {
			stat = await lstat(source);
		} catch (error) {
			if ((error as NodeJS.ErrnoException).code === "ENOENT") continue;
			throw error;
		}
		if (!stat.isFile() && !stat.isSymbolicLink())
			throw new Error(`Dispatch cannot snapshot directory entry: ${file}`);
		await mkdir(dirname(target), { recursive: true });
		await cp(source, target, { dereference: false, verbatimSymlinks: true });
	}
	await git(sourceRoot, ["bundle", "create", join(destination, "base.bundle"), "HEAD"]);
	const origin = safeOrigin(await git(sourceRoot, ["remote", "get-url", "origin"]).catch(() => ""));
	return { sourceHead, cwdRelative, origin };
}

const REMOTE_CAPTURE = `import json, os, pathlib, shutil, subprocess, sys, tarfile
original_cwd, out = sys.argv[1:]
cwd = original_cwd
def git(*args):
    return subprocess.check_output(['git', '-C', cwd, *args]).decode()
root = git('rev-parse', '--show-toplevel').strip()
cwd = root
head = git('rev-parse', 'HEAD').strip()
if any(line.startswith('160000 ') for line in git('ls-files', '--stage').splitlines()):
    raise RuntimeError('Dispatch MVP does not copy Git submodules')
os.makedirs(out + '/tree')
for name in set(git('ls-files', '-z', '--cached', '--others', '--exclude-standard').split('\\0')):
    if not name: continue
    source = pathlib.Path(root) / name
    target = pathlib.Path(out) / 'tree' / name
    if not source.exists() and not source.is_symlink(): continue
    if source.is_dir() and not source.is_symlink(): raise RuntimeError('Cannot snapshot directory entry: ' + name)
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, target, follow_symlinks=False)
subprocess.check_call(['git', '-C', root, 'bundle', 'create', out + '/base.bundle', 'HEAD'])
try: origin = git('remote', 'get-url', 'origin').strip()
except subprocess.CalledProcessError: origin = ''
with tarfile.open(out + '.tar.gz', 'w:gz', dereference=False) as archive:
    archive.add(out, arcname='.')
print(json.dumps(dict(sourceHead=head, cwdRelative=os.path.relpath(original_cwd, root), origin=origin)))
`;

async function captureRemoteProject(
	client: SailClient,
	source: DispatchBinding,
	destination: string,
	signal?: AbortSignal,
): Promise<CapturedProject> {
	const path = `/tmp/prime-capture-${randomUUID()}`;
	try {
		const result = await client.run(
			source.boxId,
			{
				command: `${shellQuote(source.guestPython)} -c ${shellQuote(REMOTE_CAPTURE)} ${shellQuote(source.guestCwd)} ${shellQuote(path)}`,
			},
			signal,
		);
		const metadata = JSON.parse(result.stdout.trim()) as CapturedProject;
		metadata.origin = safeOrigin(metadata.origin);
		await writeFile(
			join(destination, "remote.tar.gz"),
			await client.readFile(source.boxId, `${path}.tar.gz`, signal),
		);
		await executeFile("tar", ["-xzf", join(destination, "remote.tar.gz"), "-C", destination]);
		await rm(join(destination, "remote.tar.gz"));
		return metadata;
	} finally {
		await client
			.run(source.boxId, { command: `rm -rf -- ${shellQuote(path)} ${shellQuote(`${path}.tar.gz`)}`, timeout: 30 })
			.catch(() => undefined);
	}
}

function copyFilter(source: string): boolean {
	return ![".git", "__pycache__", ".venv", "node_modules", ".pytest_cache"].includes(basename(source));
}

function findRuntime(): string {
	const packageDir = getPackageDir();
	const candidates = [
		join(packageDir, "prime-agent-runtime"),
		join(packageDir, "dist", "prime-agent-runtime"),
		resolve(packageDir, "../..", "prime-agent-runtime"),
	];
	const found = candidates.find((candidate) => existsSync(join(candidate, "pyproject.toml")));
	if (!found) throw new Error("Dispatch requires the matching bundled prime-agent-runtime source");
	return found;
}

async function captureInputs(
	client: SailClient,
	inputs: Record<string, string>,
	sourceCwd: string,
	destination: string,
	sourceBinding?: DispatchBinding,
	signal?: AbortSignal,
): Promise<Record<string, string>> {
	const paths: Record<string, string> = {};
	for (const [name, path] of Object.entries(inputs)) {
		if (!/^[a-zA-Z0-9_-]+$/.test(name)) throw new Error(`Invalid dispatch input name: ${name}`);
		const target = join(destination, name);
		if (sourceBinding) {
			const source = path.startsWith("/") ? path : `${sourceBinding.guestCwd}/${path}`;
			const archive = `/tmp/prime-input-${randomUUID()}.tar.gz`;
			try {
				await client.run(
					sourceBinding.boxId,
					{
						command: `tar -czf ${shellQuote(archive)} --exclude=.git -C ${shellQuote(dirname(source))} -- ${shellQuote(basename(source))}`,
					},
					signal,
				);
				const localArchive = `${target}.tar.gz`;
				await writeFile(localArchive, await client.readFile(sourceBinding.boxId, archive, signal));
				await mkdir(target, { recursive: true });
				await executeFile("tar", ["-xzf", localArchive, "-C", target]);
				await rm(localArchive);
				paths[name] = `/workspace/inputs/${name}/${basename(source)}`;
			} finally {
				await client
					.run(sourceBinding.boxId, { command: `rm -f -- ${shellQuote(archive)}`, timeout: 30 })
					.catch(() => undefined);
			}
		} else {
			const source = resolve(sourceCwd, path);
			await cp(source, target, { recursive: true, dereference: false, verbatimSymlinks: true, filter: copyFilter });
			paths[name] = `/workspace/inputs/${name}`;
		}
	}
	return paths;
}

export interface PrepareDispatchOptions {
	sourceCwd: string;
	sessionDir: string;
	inputs?: Record<string, string>;
	sourceBinding?: DispatchBinding;
	model: { provider: string; id: string };
	signal?: AbortSignal;
}

export async function prepareDispatchWorkspace(options: PrepareDispatchOptions): Promise<DispatchBinding> {
	const client = new SailClient();
	const staging = await mkdtemp(join(tmpdir(), "prime-dispatch-"));
	let binding: DispatchBinding | undefined;
	let success = false;
	try {
		options.signal?.throwIfAborted();
		const project = join(staging, "project");
		await mkdir(project);
		const capture = options.sourceBinding
			? await captureRemoteProject(client, options.sourceBinding, project, options.signal)
			: await captureLocalProject(options.sourceCwd, project);
		const runtime = join(staging, "runtime");
		await cp(findRuntime(), runtime, { recursive: true, filter: copyFilter });
		await cp(getBundledSkillsDir(), join(staging, "skills"), { recursive: true, filter: copyFilter });
		await mkdir(join(staging, "inputs"));
		const inputs = await captureInputs(
			client,
			options.inputs ?? {},
			options.sourceCwd,
			join(staging, "inputs"),
			options.sourceBinding,
			options.signal,
		);
		const branch = `dispatch/${randomUUID()}`;
		const app = await client.findApp();
		options.signal?.throwIfAborted();
		// Do not abort the create response: retain its ID and terminate a late allocation.
		const box = await client.createBox(app.id, `prime-${basename(options.sessionDir)}`);
		binding = {
			version: 1,
			appId: app.id,
			boxId: box.sailbox_id,
			guestRepoDir: GUEST_REPO,
			guestCwd: `${GUEST_REPO}${capture.cwdRelative && capture.cwdRelative !== "." ? `/${capture.cwdRelative.split(sep).join("/")}` : ""}`,
			guestStateDir: GUEST_STATE,
			guestPython: `${GUEST_INSTALL}/venv/bin/python`,
			guestSkillsDir: `${GUEST_INSTALL}/skills`,
			hostResourceDir: options.sourceBinding?.hostResourceDir ?? resolve(options.sourceCwd),
			sourceHead: capture.sourceHead,
			baselineCommit: "",
			initialBranch: branch,
			inputs,
			model: options.model,
		};
		await mkdir(options.sessionDir, { recursive: true });
		await writeFile(join(options.sessionDir, "dispatch.json"), JSON.stringify(binding, null, 2));
		options.signal?.throwIfAborted();
		if (box.status !== "running")
			throw new Error(`Sailbox setup failed (${box.status}): ${box.error_message ?? box.sailbox_id}`);
		const archive = join(staging, "payload.tar.gz");
		await executeFile(
			"tar",
			["--no-xattrs", "-czf", archive, "-C", staging, "project", "runtime", "skills", "inputs"],
			{
				env: { ...process.env, COPYFILE_DISABLE: "1" },
			},
		);
		await client.writeFile(box.sailbox_id, "/tmp/prime-dispatch.tar.gz", await readFile(archive), options.signal);
		const skillPackages = (await readdir(join(staging, "skills")))
			.filter((name) => existsSync(join(staging, "skills", name, "pyproject.toml")))
			.map((name) => `${GUEST_INSTALL}/skills/${name}`);
		const bootstrap = `set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq python3 python3-venv git gh ca-certificates curl
mkdir -p ${GUEST_INSTALL} ${GUEST_STATE} /workspace
tar --no-same-owner -xzf /tmp/prime-dispatch.tar.gz -C ${GUEST_INSTALL}
mv ${GUEST_INSTALL}/inputs /workspace/inputs
mkdir -p ${GUEST_REPO}
git -C ${GUEST_REPO} init -q
git -C ${GUEST_REPO} fetch -q ${GUEST_INSTALL}/project/base.bundle HEAD
git -C ${GUEST_REPO} checkout -q -b ${shellQuote(branch)} FETCH_HEAD
find ${GUEST_REPO} -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf -- {} +
cp -a ${GUEST_INSTALL}/project/tree/. ${GUEST_REPO}/
git -C ${GUEST_REPO} config user.name 'Prime Dispatch'
git -C ${GUEST_REPO} config user.email 'dispatch@localhost'
git -C ${GUEST_REPO} add -Af
git -C ${GUEST_REPO} commit --allow-empty -qm 'Starting working tree snapshot'
${capture.origin ? `git -C ${GUEST_REPO} remote add origin ${shellQuote(capture.origin)}` : ""}
git -C ${GUEST_REPO} config credential.https://github.com.helper '!gh auth git-credential'
python3 -m venv ${GUEST_INSTALL}/venv
${binding.guestPython} -m pip install -q ${[`${GUEST_INSTALL}/runtime`, "dill", ...DEFAULT_RLM_EXTRA_UV_ARGS, ...skillPackages].map(shellQuote).join(" ")}
rm -rf ${GUEST_INSTALL}/project /tmp/prime-dispatch.tar.gz
git -C ${GUEST_REPO} rev-parse HEAD
`;
		const result = await client.run(box.sailbox_id, { command: bootstrap, timeout: 600 }, options.signal);
		binding.baselineCommit = result.stdout.trim().split("\n").at(-1) ?? "";
		if (!/^[a-f0-9]{40,64}$/.test(binding.baselineCommit))
			throw new Error("Sailbox bootstrap did not produce a baseline commit");
		await writeFile(join(options.sessionDir, "dispatch.json"), JSON.stringify(binding, null, 2));
		options.signal?.throwIfAborted();
		success = true;
		return binding;
	} finally {
		await rm(staging, { recursive: true, force: true });
		if (!success && binding) await terminateDispatchWorkspace(binding);
	}
}

export function loadDispatchBinding(sessionDir: string): DispatchBinding | undefined {
	const path = join(sessionDir, "dispatch.json");
	if (!existsSync(path)) return undefined;
	const binding = JSON.parse(readFileSync(path, "utf8")) as DispatchBinding;
	if (binding.version !== 1 || !binding.boxId || !binding.guestCwd)
		throw new Error(`Invalid dispatch binding: ${path}`);
	return binding;
}

export async function terminateDispatchWorkspace(binding: DispatchBinding): Promise<void> {
	try {
		await new SailClient().terminateBox(binding.boxId);
	} catch (error) {
		if (!(error instanceof SailHttpError && error.status === 404)) throw error;
	}
}

export async function sleepDispatchWorkspace(binding: DispatchBinding): Promise<void> {
	const client = new SailClient();
	try {
		await client.sleepBox(binding.boxId);
	} catch (error) {
		if (error instanceof SailHttpError && error.status === 404) return;
		const state = await client.getBox(binding.boxId);
		if (["terminated", "terminating", "sleeping"].includes(state.status)) return;
		throw error;
	}
}
