import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { SELF_UPDATE_INTERACTIVE_CHILD_ENV } from "../src/config.js";

const mocks = vi.hoisted(() => ({
	daemonCommands: [] as string[][],
	packageCommands: [] as string[][],
	psCalls: [] as boolean[],
	reapCalls: [] as Array<[boolean, boolean]>,
	shutdownCalls: [] as Array<[boolean, boolean]>,
	mcpCommands: [] as string[][],
}));

vi.mock("../src/cli/daemon-command.js", () => ({
	handleDaemonCommand: async (args: string[]) => {
		mocks.daemonCommands.push(args);
		return true;
	},
}));

vi.mock("../src/package-manager-cli.js", () => ({
	handlePackageCommand: async (args: string[]) => {
		mocks.packageCommands.push(args);
		return true;
	},
	isSelfUpdateSource: (source: string) => source === "self" || source === "pi" || source === "prime-agent",
}));

vi.mock("../src/core/mcp/mcp-command.js", () => ({
	runMcpManagementCommand: async (args: string[]) => {
		mocks.mcpCommands.push(args);
		return { action: args[0], message: "managed", changed: false };
	},
}));

vi.mock("../src/core/settings-manager.js", () => ({
	SettingsManager: {
		create: () => ({ flush: async () => {}, drainErrors: () => [], getGlobalMcpServers: () => undefined }),
	},
}));

vi.mock("../src/cli/daemon-ps.js", () => ({
	runPs: async (json: boolean) => {
		mocks.psCalls.push(json);
	},
	runReap: async (json: boolean, force: boolean) => {
		mocks.reapCalls.push([json, force]);
	},
	runShutdownAll: async (json: boolean, force: boolean) => {
		mocks.shutdownCalls.push([json, force]);
	},
}));

import { INTERNAL_RUNTIME_COMMAND_MARKER } from "../src/cli/args.js";
import { DAEMON_UPDATE_RESTART_COORDINATOR_FLAG } from "../src/cli/daemon-update-restart.js";
import { handlePublicCommand } from "../src/cli/public-command.js";

describe("public command routing", () => {
	beforeEach(() => {
		mocks.daemonCommands.length = 0;
		mocks.packageCommands.length = 0;
		mocks.psCalls.length = 0;
		mocks.reapCalls.length = 0;
		mocks.shutdownCalls.length = 0;
		mocks.mcpCommands.length = 0;
		process.exitCode = undefined;
		vi.spyOn(console, "log").mockImplementation(() => {});
		vi.spyOn(console, "error").mockImplementation(() => {});
	});

	afterEach(() => {
		process.exitCode = undefined;
		vi.restoreAllMocks();
	});

	it.each<[string[], Record<string, unknown>]>([
		[
			["attach", "worker"],
			{ handled: false, args: ["--resume", "worker"], explicitAgentsView: false, attachAgent: "worker" },
		],
		[
			["attach", "worker", "--verbose", "--provider", "anthropic"],
			{
				handled: false,
				args: ["--resume", "worker", "--verbose", "--provider", "anthropic"],
				explicitAgentsView: false,
				attachAgent: "worker",
			},
		],
		[
			["agents", "--verbose", "--provider", "anthropic"],
			{ handled: false, args: ["--verbose", "--provider", "anthropic"], explicitAgentsView: true },
		],
		[
			["model", "list", "sonnet", "--offline"],
			{
				handled: false,
				args: [INTERNAL_RUNTIME_COMMAND_MARKER, "--list-models", "sonnet", "--offline"],
				explicitAgentsView: false,
			},
		],
		[
			["session", "export", "session.jsonl", "session.html", "--verbose"],
			{
				handled: false,
				args: [INTERNAL_RUNTIME_COMMAND_MARKER, "--export", "session.jsonl", "session.html", "--verbose"],
				explicitAgentsView: false,
			},
		],
		[
			["help", "me", "fix", "this"],
			{ handled: false, args: ["help", "me", "fix", "this"], explicitAgentsView: false },
		],
		[["--help"], { handled: false, args: ["--help"], explicitAgentsView: false }],
		[["-h"], { handled: false, args: ["-h"], explicitAgentsView: false }],
	])("passes %j back to the interactive startup path", async (argv, expected) => {
		await expect(handlePublicCommand(argv)).resolves.toEqual(expected);
	});

	it.each<[string[], string[]]>([
		[
			["list", "--all", "--json"],
			["daemon", "list", "--all", "--json"],
		],
		[
			["stop", "worker", "--daemon-socket", "/tmp/custom-daemon.sock"],
			["daemon", "kill", "worker", "--daemon-socket", "/tmp/custom-daemon.sock"],
		],
		[
			["rename", "worker", "reviewer", "--daemon-socket", "/tmp/custom-daemon.sock"],
			["daemon", "rename", "worker", "reviewer", "--daemon-socket", "/tmp/custom-daemon.sock"],
		],
		// Help-like message text after the separator stays literal payload.
		[
			["send", "worker", "--", "--help"],
			["daemon", "send", "worker", "--", "--help"],
		],
	])("routes %j through the internal protocol adapter", async (argv, forwarded) => {
		await expect(handlePublicCommand(argv)).resolves.toMatchObject({ handled: true });
		expect(mocks.daemonCommands).toEqual([forwarded]);
	});

	it.each<[string[]]>([
		[["attach", "worker", "extra"]],
		[["attach", "worker", "--resume", "other"]],
		[["attach", "worker", "-r", "other"]],
		[["attach", "worker", "--continue"]],
		[["attach", "worker", "--fork", "session.jsonl"]],
	])("rejects %j instead of starting a session", async (argv) => {
		await expect(handlePublicCommand(argv)).resolves.toMatchObject({ handled: true });
		expect(process.exitCode).toBe(1);
		expect(mocks.daemonCommands).toEqual([]);
	});

	it("routes MCP management without entering agent startup", async () => {
		await expect(
			handlePublicCommand(["mcp", "add", "local", "--", "node", "server file.js", "--stdio"]),
		).resolves.toMatchObject({ handled: true });
		expect(mocks.mcpCommands).toEqual([["add", "local", "--", "node", "server file.js", "--stdio"]]);
	});

	it("separates Prime Agent updates from package updates", async () => {
		await handlePublicCommand(["update", "--force"]);
		await handlePublicCommand(["package", "update"]);
		await handlePublicCommand(["package", "update", "npm:@example/tools"]);

		expect(mocks.packageCommands).toEqual([
			["update", "--self", "--force"],
			["update", "--extensions"],
			["update", "npm:@example/tools"],
		]);
	});

	it("forwards hidden update restart coordinator invocations", async () => {
		const args = ["update", DAEMON_UPDATE_RESTART_COORDINATOR_FLAG, "--daemon-socket", "custom-daemon.sock"];

		await handlePublicCommand(args);

		expect(mocks.packageCommands).toEqual([args]);
	});

	it("preserves the internal interactive self-update command", async () => {
		const previousValue = process.env[SELF_UPDATE_INTERACTIVE_CHILD_ENV];
		const args = ["update", "--self", "--force", "--daemon-socket", "custom-daemon.sock"];
		process.env[SELF_UPDATE_INTERACTIVE_CHILD_ENV] = "1";

		try {
			await handlePublicCommand(args);
		} finally {
			if (previousValue === undefined) {
				delete process.env[SELF_UPDATE_INTERACTIVE_CHILD_ENV];
			} else {
				process.env[SELF_UPDATE_INTERACTIVE_CHILD_ENV] = previousValue;
			}
		}

		expect(mocks.packageCommands).toEqual([args]);
	});

	it.each<[string[]]>([
		[["update", "self"]],
		[["update", "--self"]],
		[["update", "prime-agent"]],
		[["update", "npm:@example/tools"]],
		[["update", "--extensions"]],
		[["update", "--self", "--extensions"]],
		[["package", "update", "self"]],
		[["package", "uninstall", "npm:@example/tools"]],
		[["package", "list", "ignored-source"]],
		[["daemon", "list"]],
		[["schedule", "cancell", "job-1"]],
	])("does not execute the retired form %j", async (argv) => {
		await handlePublicCommand(argv);

		expect(mocks.packageCommands).toEqual([]);
		expect(mocks.daemonCommands).toEqual([]);
	});

	it("uses force only when explicitly requested for full shutdown", async () => {
		await handlePublicCommand(["shutdown", "--json"]);
		await handlePublicCommand(["shutdown", "--force"]);

		expect(mocks.shutdownCalls).toEqual([
			[true, false],
			[false, true],
		]);
	});

	it("routes doctor fixes through the safe cleanup path", async () => {
		await handlePublicCommand(["doctor", "--fix", "--json"]);

		expect(mocks.reapCalls).toEqual([[true, false]]);
	});

	it("resolves command help when options precede the help flag", async () => {
		await handlePublicCommand(["list", "--all", "--help"]);
		await handlePublicCommand(["package", "install", "--local", "--help"]);

		expect(console.log).toHaveBeenCalledTimes(2);
		expect(console.error).not.toHaveBeenCalled();
		expect(mocks.daemonCommands).toEqual([]);
		expect(mocks.packageCommands).toEqual([]);
	});

	it("rejects invalid paths below a known help command", async () => {
		await handlePublicCommand(["help", "schedule", "nonsense"]);

		expect(process.exitCode).toBe(1);
		expect(mocks.daemonCommands).toEqual([]);
	});
});
