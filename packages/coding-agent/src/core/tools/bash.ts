import { existsSync } from "node:fs";
import type { AgentTool } from "@earendil-works/pi-agent-core";
import { Container, Text, truncateToWidth } from "@earendil-works/pi-tui";
import { type Static, Type } from "typebox";
import { expandCollapseHint } from "../../modes/interactive/components/keybinding-hints.js";
import { truncateToVisualLines } from "../../modes/interactive/components/visual-truncate.js";
import { theme } from "../../modes/interactive/theme/theme.js";
import { spawnHidden, waitForChildProcess } from "../../utils/child-process.js";
import {
	getShellConfig,
	getShellEnv,
	killProcessTree,
	trackDetachedChildPid,
	untrackDetachedChildPid,
} from "../../utils/shell.js";
import type { ToolDefinition, ToolRenderResultOptions } from "../extensions/types.js";
import { previewBashCommand } from "./code-preview.js";
import { OutputAccumulator } from "./output-accumulator.js";
import { getTextOutput, invalidArgText, str } from "./render-utils.js";
import { wrapToolDefinition } from "./tool-definition-wrapper.js";
import { DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, formatSize, type TruncationResult } from "./truncate.js";

const bashSchema = Type.Object({
	command: Type.String({ description: "Bash command to execute" }),
	timeout: Type.Optional(Type.Number({ description: "Timeout in seconds (optional, no default timeout)" })),
	allowDestructiveGit: Type.Optional(
		Type.Boolean({
			description:
				"Skip the dirty-tree guard for destructive git discard commands. Only set when discarding uncommitted work is intentional.",
		}),
	),
});

export type BashToolInput = Static<typeof bashSchema>;

export interface BashToolDetails {
	truncation?: TruncationResult;
	fullOutputPath?: string;
}

/**
 * Pluggable operations for the bash tool.
 * Override these to delegate command execution to remote systems (for example SSH).
 */
export interface BashOperations {
	/**
	 * Execute a command and stream output.
	 * @param command The command to execute
	 * @param cwd Working directory
	 * @param options Execution options
	 * @returns Promise resolving to exit code (null if killed)
	 */
	exec: (
		command: string,
		cwd: string,
		options: {
			onData: (data: Buffer) => void;
			signal?: AbortSignal;
			timeout?: number;
			env?: NodeJS.ProcessEnv;
		},
	) => Promise<{ exitCode: number | null }>;
}

/**
 * Create bash operations using pi's built-in local shell execution backend.
 *
 * This is useful for extensions that intercept user_bash and still want pi's
 * standard local shell behavior while wrapping or rewriting commands.
 */
export function createLocalBashOperations(options?: { shellPath?: string }): BashOperations {
	return {
		exec: (command, cwd, { onData, signal, timeout, env }) => {
			return new Promise((resolve, reject) => {
				const { shell, args } = getShellConfig(options?.shellPath);
				if (!existsSync(cwd)) {
					reject(new Error(`Working directory does not exist: ${cwd}\nCannot execute bash commands.`));
					return;
				}
				const child = spawnHidden(shell, [...args, command], {
					cwd,
					detached: process.platform !== "win32",
					env: env ?? getShellEnv(),
					stdio: ["ignore", "pipe", "pipe"],
				});
				if (child.pid) trackDetachedChildPid(child.pid);
				let timedOut = false;
				let timeoutHandle: NodeJS.Timeout | undefined;
				if (timeout !== undefined && timeout > 0) {
					timeoutHandle = setTimeout(() => {
						timedOut = true;
						if (child.pid) killProcessTree(child.pid);
					}, timeout * 1000);
				}
				child.stdout?.on("data", onData);
				child.stderr?.on("data", onData);
				const onAbort = () => {
					if (child.pid) killProcessTree(child.pid);
				};
				if (signal) {
					if (signal.aborted) onAbort();
					else signal.addEventListener("abort", onAbort, { once: true });
				}
				// Handle shell spawn errors and wait for the process to terminate without hanging
				// on inherited stdio handles held by detached descendants.
				waitForChildProcess(child)
					.then((code) => {
						if (child.pid) untrackDetachedChildPid(child.pid);
						if (timeoutHandle) clearTimeout(timeoutHandle);
						if (signal) signal.removeEventListener("abort", onAbort);
						if (signal?.aborted) {
							reject(new Error("aborted"));
							return;
						}
						if (timedOut) {
							reject(new Error(`timeout:${timeout}`));
							return;
						}
						resolve({ exitCode: code });
					})
					.catch((err) => {
						if (child.pid) untrackDetachedChildPid(child.pid);
						if (timeoutHandle) clearTimeout(timeoutHandle);
						if (signal) signal.removeEventListener("abort", onAbort);
						reject(err);
					});
			});
		},
	};
}

export interface BashSpawnContext {
	command: string;
	cwd: string;
	env: NodeJS.ProcessEnv;
}

export type BashSpawnHook = (context: BashSpawnContext) => BashSpawnContext;

function resolveSpawnContext(command: string, cwd: string, spawnHook?: BashSpawnHook): BashSpawnContext {
	const baseContext: BashSpawnContext = { command, cwd, env: { ...getShellEnv() } };
	return spawnHook ? spawnHook(baseContext) : baseContext;
}

export interface BashToolOptions {
	/** Custom operations for command execution. Default: local shell */
	operations?: BashOperations;
	/** Command prefix prepended to every command (for example shell setup commands) */
	commandPrefix?: string;
	/** Optional explicit shell path from settings */
	shellPath?: string;
	/** Hook to adjust command, cwd, or env before execution */
	spawnHook?: BashSpawnHook;
}

/** Bypass env var for the destructive-git dirty-tree guard. */
export const BASH_DESTRUCTIVE_GIT_BYPASS_ENV = "PI_BASH_ALLOW_DESTRUCTIVE_GIT";

const GIT_STATUS_PORCELAIN_COMMAND = "git status --porcelain";

/** How many dirty paths the refusal lists before eliding the rest. */
const MAX_DIRTY_PATHS_LISTED = 10;

/**
 * Detect git commands that discard uncommitted working-tree changes: the
 * reflexive "clean the worktree" idiom (`git checkout -- .`, `git clean -fd`,
 * `git reset --hard`) that has repeatedly destroyed in-progress agent work.
 *
 * Intentionally conservative: a false positive costs one `git status` probe
 * and an explicit-bypass retry, while a false negative silently loses work.
 * Matching is best-effort shell-text heuristics, not a parse.
 */
export function isDestructiveGitDiscardCommand(command: string): boolean {
	// git checkout -- . | git checkout . | git checkout HEAD -- . | git restore .
	if (/\bgit\s+checkout\s+(?:(?:--\s+)?\.|HEAD\s+--\s+\.)(?=\s|$|[;&|)])/.test(command)) return true;
	if (/\bgit\s+restore\s+(?:(?:--source|--worktree)(?:=\S+)?\s+|-s\s+\S+\s+|-W\s+)?\.(?=\s|$|[;&|)])/.test(command)) {
		return true;
	}
	// git reset --hard [ref]
	if (/\bgit\s+reset\s+--hard\b/.test(command)) return true;
	// git clean with a force flag, unless it is also a dry run
	for (const match of command.matchAll(/\bgit\s+clean\s+([^;&|]*)/g)) {
		const args = match[1].split(/\s+/).filter(Boolean);
		const forces = args.filter((arg) =>
			arg.startsWith("--") ? arg.startsWith("--force") : arg.startsWith("-") && arg.includes("f"),
		);
		if (forces.length === 0) continue;
		const dryRun = args.some(
			(arg) => arg === "--dry-run" || (arg.startsWith("-") && !arg.startsWith("--") && arg.includes("n")),
		);
		if (!dryRun) return true;
	}
	return false;
}

function isTruthyEnvValue(value: string | undefined): boolean {
	return value !== undefined && value !== "" && value !== "0";
}

/**
 * Probe for uncommitted changes via `git status --porcelain` in the command's cwd.
 * Returns null when dirtiness cannot be determined (not a repo, git missing,
 * probe failure) so the guard fails open instead of blocking on a guess.
 */
async function probeUncommittedChanges(
	ops: BashOperations,
	probeCommand: string,
	cwd: string,
	env: NodeJS.ProcessEnv,
	signal: AbortSignal | undefined,
): Promise<string[] | null> {
	let output = "";
	try {
		const result = await ops.exec(probeCommand, cwd, {
			onData: (data) => {
				output += data.toString("utf8");
			},
			signal,
			env,
		});
		if (result.exitCode !== 0) return null;
	} catch (err) {
		if (err instanceof Error && err.message === "aborted") throw err;
		return null;
	}
	return output
		.split("\n")
		.filter((line) => line.trim().length > 0)
		.map((line) => line.replace(/\r$/, ""));
}

function formatDirtyTreeRefusal(dirtyPaths: string[]): string {
	const listed = dirtyPaths.slice(0, MAX_DIRTY_PATHS_LISTED);
	const elided = dirtyPaths.length - listed.length;
	const lines = [
		`Refusing to run this destructive git command: the working tree has ${dirtyPaths.length} uncommitted change(s).`,
		...listed.map((line) => `  ${line}`),
	];
	if (elided > 0) lines.push(`  ... and ${elided} more`);
	lines.push("");
	lines.push("Commit, stash, or stage your work first.");
	lines.push(
		`To discard these changes intentionally, retry with allowDestructiveGit: true, or set ${BASH_DESTRUCTIVE_GIT_BYPASS_ENV}=1.`,
	);
	return lines.join("\n");
}

const BASH_PREVIEW_LINES = 5;
const BASH_UPDATE_THROTTLE_MS = 100;

type BashRenderState = {
	startedAt: number | undefined;
	endedAt: number | undefined;
	interval: NodeJS.Timeout | undefined;
};

type BashResultRenderState = {
	cachedWidth: number | undefined;
	cachedLines: string[] | undefined;
	cachedSkipped: number | undefined;
};

class BashResultRenderComponent extends Container {
	state: BashResultRenderState = {
		cachedWidth: undefined,
		cachedLines: undefined,
		cachedSkipped: undefined,
	};
}

function formatDuration(ms: number): string {
	return `${(ms / 1000).toFixed(1)}s`;
}

function formatBashCall(args: { command?: string; timeout?: number } | undefined): string {
	const command = str(args?.command);
	const timeout = args?.timeout as number | undefined;
	const timeoutSuffix = timeout ? theme.fg("dim", ` (timeout ${timeout}s)`) : "";
	let commandDisplay: string;
	if (command === null) {
		commandDisplay = invalidArgText(theme);
	} else if (command) {
		const preview = previewBashCommand(command);
		const label = preview.language === "bash" ? "" : `${preview.language}: `;
		commandDisplay = preview.text ? `${label}${preview.text}` : command;
	} else {
		commandDisplay = theme.fg("toolOutput", "...");
	}
	return theme.fg("dim", `$ ${commandDisplay}`) + timeoutSuffix;
}

function rebuildBashResultRenderComponent(
	component: BashResultRenderComponent,
	result: {
		content: Array<{ type: string; text?: string; data?: string; mimeType?: string }>;
		details?: BashToolDetails;
	},
	options: ToolRenderResultOptions,
	showImages: boolean,
	includeImageDimensions: boolean,
	showExpandHint: boolean,
	startedAt: number | undefined,
	endedAt: number | undefined,
): void {
	const state = component.state;
	component.clear();

	const output = getTextOutput(result as any, showImages, { includeImageDimensions }).trim();

	if (output) {
		const styledOutput = output
			.split("\n")
			.map((line) => theme.fg("toolOutput", line))
			.join("\n");

		if (options.expanded) {
			component.addChild(new Text(`\n${styledOutput}`, 0, 0));
		} else {
			component.addChild({
				render: (width: number) => {
					if (state.cachedLines === undefined || state.cachedWidth !== width) {
						const preview = truncateToVisualLines(styledOutput, BASH_PREVIEW_LINES, width);
						state.cachedLines = preview.visualLines;
						state.cachedSkipped = preview.skippedCount;
						state.cachedWidth = width;
					}
					if (state.cachedSkipped && state.cachedSkipped > 0) {
						const hint = showExpandHint
							? `${theme.fg("dim", `... ${state.cachedSkipped} earlier lines`)} ${expandCollapseHint("app.tools.expand", false)}`
							: theme.fg("dim", `... (${state.cachedSkipped} earlier lines)`);
						return ["", truncateToWidth(hint, width, "..."), ...(state.cachedLines ?? [])];
					}
					return ["", ...(state.cachedLines ?? [])];
				},
				invalidate: () => {
					state.cachedWidth = undefined;
					state.cachedLines = undefined;
					state.cachedSkipped = undefined;
				},
			});
		}
	}

	const truncation = result.details?.truncation;
	const fullOutputPath = result.details?.fullOutputPath;
	if (truncation?.truncated || fullOutputPath) {
		const warnings: string[] = [];
		if (fullOutputPath) {
			warnings.push(`Full output: ${fullOutputPath}`);
		}
		if (truncation?.truncated) {
			if (truncation.truncatedBy === "lines") {
				warnings.push(`Truncated: showing ${truncation.outputLines} of ${truncation.totalLines} lines`);
			} else {
				warnings.push(
					`Truncated: ${truncation.outputLines} lines shown (${formatSize(truncation.maxBytes ?? DEFAULT_MAX_BYTES)} limit)`,
				);
			}
		}
		component.addChild(new Text(`\n${theme.fg("warning", `[${warnings.join(". ")}]`)}`, 0, 0));
	}

	if (startedAt !== undefined) {
		const label = options.isPartial ? "Elapsed" : "Took";
		const endTime = endedAt ?? Date.now();
		component.addChild(new Text(`\n${theme.fg("dim", `${label} ${formatDuration(endTime - startedAt)}`)}`, 0, 0));
	}
}

export function createBashToolDefinition(
	cwd: string,
	options?: BashToolOptions,
): ToolDefinition<typeof bashSchema, BashToolDetails | undefined, BashRenderState> {
	const ops = options?.operations ?? createLocalBashOperations({ shellPath: options?.shellPath });
	const commandPrefix = options?.commandPrefix;
	const spawnHook = options?.spawnHook;
	const definition: ToolDefinition<typeof bashSchema, BashToolDetails | undefined, BashRenderState> = {
		name: "bash",
		label: "bash",
		description: `Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last ${DEFAULT_MAX_LINES} lines or ${DEFAULT_MAX_BYTES / 1024}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds. Destructive git discard commands (git checkout -- ., git checkout ., git clean -f..., git reset --hard, git restore .) are refused while uncommitted changes exist; retry with allowDestructiveGit: true only when the discard is intentional.`,
		promptSnippet: "Execute bash commands (ls, grep, find, etc.)",
		parameters: bashSchema,
		async execute(
			_toolCallId,
			{
				command,
				timeout,
				allowDestructiveGit,
			}: { command: string; timeout?: number; allowDestructiveGit?: boolean },
			signal?: AbortSignal,
			onUpdate?,
			_ctx?,
		) {
			const resolvedCommand = commandPrefix ? `${commandPrefix}\n${command}` : command;
			const spawnContext = resolveSpawnContext(resolvedCommand, cwd, spawnHook);
			// Dirty-tree guard: destructive git discard commands have repeatedly
			// wiped uncommitted work, so refuse them while the tree is dirty
			// (make it impossible, not discouraged). Zero cost otherwise: the
			// pattern check is string-only and the probe only runs on a match.
			if (
				allowDestructiveGit !== true &&
				!isTruthyEnvValue(spawnContext.env[BASH_DESTRUCTIVE_GIT_BYPASS_ENV]) &&
				isDestructiveGitDiscardCommand(spawnContext.command)
			) {
				const probeCommand = commandPrefix
					? `${commandPrefix}\n${GIT_STATUS_PORCELAIN_COMMAND}`
					: GIT_STATUS_PORCELAIN_COMMAND;
				const dirtyPaths = await probeUncommittedChanges(
					ops,
					probeCommand,
					spawnContext.cwd,
					spawnContext.env,
					signal,
				);
				if (dirtyPaths && dirtyPaths.length > 0) {
					throw new Error(formatDirtyTreeRefusal(dirtyPaths));
				}
			}
			const output = new OutputAccumulator({ tempFilePrefix: "pi-bash" });
			let updateTimer: NodeJS.Timeout | undefined;
			let updateDirty = false;
			let lastUpdateAt = 0;

			const emitOutputUpdate = () => {
				if (!onUpdate || !updateDirty) return;
				updateDirty = false;
				lastUpdateAt = Date.now();
				const snapshot = output.snapshot();
				onUpdate({
					content: [{ type: "text", text: snapshot.content || "" }],
					details: {
						truncation: snapshot.truncation.truncated ? snapshot.truncation : undefined,
						fullOutputPath: snapshot.fullOutputPath,
					},
				});
			};

			const clearUpdateTimer = () => {
				if (updateTimer) {
					clearTimeout(updateTimer);
					updateTimer = undefined;
				}
			};

			const scheduleOutputUpdate = () => {
				if (!onUpdate) return;
				updateDirty = true;
				const delay = BASH_UPDATE_THROTTLE_MS - (Date.now() - lastUpdateAt);
				if (delay <= 0) {
					clearUpdateTimer();
					emitOutputUpdate();
					return;
				}
				updateTimer ??= setTimeout(() => {
					updateTimer = undefined;
					emitOutputUpdate();
				}, delay);
			};

			if (onUpdate) {
				onUpdate({ content: [], details: undefined });
			}

			const handleData = (data: Buffer) => {
				output.append(data);
				scheduleOutputUpdate();
			};

			const finishOutput = async () => {
				output.finish();
				clearUpdateTimer();
				emitOutputUpdate();
				// Snapshot only after the spill settled: the advertised path is terminal.
				await output.closeTempFile();
				return output.snapshot();
			};

			const formatOutput = (snapshot: Awaited<ReturnType<typeof finishOutput>>, emptyText = "(no output)") => {
				const truncation = snapshot.truncation;
				let text = snapshot.content || emptyText;
				let details: BashToolDetails | undefined;
				if (truncation.truncated) {
					details = { truncation, fullOutputPath: snapshot.fullOutputPath };
					const startLine = truncation.totalLines - truncation.outputLines + 1;
					const endLine = truncation.totalLines;
					// A degraded spill has no path; never advertise "Full output: undefined".
					const location = snapshot.fullOutputPath ? `. Full output: ${snapshot.fullOutputPath}` : "";
					if (truncation.lastLinePartial) {
						// The partial line is the first SHOWN line; trailing blanks can follow it.
						const lastLineBytes = output.getLastLineBytes();
						const lineSize = lastLineBytes > 0 ? ` (line is ${formatSize(lastLineBytes)})` : "";
						text += `\n\n[Showing last ${formatSize(truncation.outputBytes)} of line ${startLine}${lineSize}${location}]`;
					} else if (truncation.truncatedBy === "lines") {
						text += `\n\n[Showing lines ${startLine}-${endLine} of ${truncation.totalLines}${location}]`;
					} else {
						text += `\n\n[Showing lines ${startLine}-${endLine} of ${truncation.totalLines} (${formatSize(DEFAULT_MAX_BYTES)} limit)${location}]`;
					}
				}
				return { text, details };
			};

			const appendStatus = (text: string, status: string) => `${text ? `${text}\n\n` : ""}${status}`;

			try {
				let exitCode: number | null;
				try {
					const result = await ops.exec(spawnContext.command, spawnContext.cwd, {
						onData: handleData,
						signal,
						timeout,
						env: spawnContext.env,
					});
					exitCode = result.exitCode;
				} catch (err) {
					const snapshot = await finishOutput();
					const { text } = formatOutput(snapshot, "");
					if (err instanceof Error && err.message === "aborted") {
						throw new Error(appendStatus(text, "Command aborted"));
					}
					if (err instanceof Error && err.message.startsWith("timeout:")) {
						const timeoutSecs = err.message.split(":")[1];
						throw new Error(appendStatus(text, `Command timed out after ${timeoutSecs} seconds`));
					}
					throw err;
				}

				const snapshot = await finishOutput();
				const { text: outputText, details } = formatOutput(snapshot);
				if (exitCode !== 0 && exitCode !== null) {
					throw new Error(appendStatus(outputText, `Command exited with code ${exitCode}`));
				}
				return { content: [{ type: "text", text: outputText }], details };
			} finally {
				clearUpdateTimer();
			}
		},
		renderCall(args, _theme, context) {
			const state = context.state;
			if (context.executionStarted && state.startedAt === undefined) {
				state.startedAt = Date.now();
				state.endedAt = undefined;
			}
			const text = (context.lastComponent as Text | undefined) ?? new Text("", 0, 0);
			text.setText(formatBashCall(args));
			return text;
		},
		renderResult(result, options, _theme, context) {
			const state = context.state;
			if (state.startedAt !== undefined && options.isPartial && !state.interval) {
				state.interval = setInterval(() => context.invalidate(), 1000);
			}
			if (!options.isPartial || context.isError) {
				state.endedAt ??= Date.now();
				if (state.interval) {
					clearInterval(state.interval);
					state.interval = undefined;
				}
			}
			const component =
				(context.lastComponent as BashResultRenderComponent | undefined) ?? new BashResultRenderComponent();
			rebuildBashResultRenderComponent(
				component,
				result as any,
				options,
				context.showImages,
				context.includeImageDimensions,
				context.showExpandHint !== false,
				state.startedAt,
				state.endedAt,
			);
			component.invalidate();
			return component;
		},
	};
	return Object.assign(definition, { replayBuiltInToolName: "bash" as const });
}

export function createBashTool(cwd: string, options?: BashToolOptions): AgentTool<typeof bashSchema> {
	return wrapToolDefinition(createBashToolDefinition(cwd, options));
}
