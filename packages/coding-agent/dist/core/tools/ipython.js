import { Type } from "typebox";
import { KernelManager, resolveKernelPython } from "../kernel/index.js";
import { wrapToolDefinition } from "./tool-definition-wrapper.js";
const ipythonSchema = Type.Object({
    code: Type.String({
        description: "Python code to execute. State (variables, imports, loaded data) persists across calls. " +
            "Shell commands available via `!cmd` (single-line) or `%%bash` (multi-line cell).",
    }),
});
export function createIpythonToolDefinition(cwd, options) {
    let manager;
    async function getManager() {
        if (manager)
            return manager;
        const python = options?.python ?? resolveKernelPython();
        if (!python) {
            throw new Error("No Python interpreter with `ipykernel` was found. Run `./scripts/setup-kernel-venv.sh` from the repo root, or set PRIME_AGENT_KERNEL_PYTHON to point at a python that has ipykernel installed.");
        }
        manager = new KernelManager({ python, cwd });
        await manager.start();
        return manager;
    }
    // Best-effort cleanup on process exit.
    process.once("exit", () => {
        manager?.shutdown().catch(() => { });
    });
    return {
        name: "ipython",
        label: "ipython",
        description: "Execute Python code in a persistent IPython kernel. Variables, imports, and loaded data " +
            "persist across calls. Shell commands available inside Python via `!cmd` (single-line) " +
            "or `%%bash` (multi-line cells).",
        promptSnippet: "ipython - execute Python in a persistent kernel; state survives across calls",
        parameters: ipythonSchema,
        execute: async (_toolCallId, params, signal, onUpdate) => {
            const m = await getManager();
            const r = await m.execute(params.code, {
                signal,
                onStream: (chunk) => {
                    onUpdate?.({
                        content: [{ type: "text", text: chunk }],
                        details: { status: "ok" },
                    });
                },
            });
            let text = r.stdout;
            if (r.stderr)
                text += (text ? "\n" : "") + r.stderr;
            if (r.result)
                text += (text ? "\n" : "") + r.result;
            if (r.status === "error" && r.error) {
                text += (text ? "\n" : "") + r.error.traceback.join("\n");
            }
            return {
                content: [{ type: "text", text: text || "" }],
                details: {
                    durationMs: r.durationMs,
                    status: r.status,
                    errorEname: r.error?.ename,
                },
                isError: r.status === "error",
            };
        },
    };
}
export function createIpythonTool(cwd, options) {
    return wrapToolDefinition(createIpythonToolDefinition(cwd, options));
}
//# sourceMappingURL=ipython.js.map