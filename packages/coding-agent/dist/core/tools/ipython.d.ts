import type { AgentTool } from "@earendil-works/pi-agent-core";
import { type Static, Type } from "typebox";
import type { ToolDefinition } from "../extensions/types.js";
declare const ipythonSchema: Type.TObject<{
    code: Type.TString;
}>;
export type IpythonToolInput = Static<typeof ipythonSchema>;
export interface IpythonToolDetails {
    durationMs?: number;
    status?: "ok" | "error" | "aborted";
    errorEname?: string;
}
export interface IpythonToolOptions {
    /**
     * Path to the Python interpreter that runs the kernel. Must have `ipykernel`
     * installed. Defaults to {@link resolveKernelPython}, which checks
     * `PRIME_AGENT_KERNEL_PYTHON`, then `~/.prime-agent/kernel-venv`, then
     * `<repo>/.kernel-venv` for development.
     */
    python?: string;
}
export declare function createIpythonToolDefinition(cwd: string, options?: IpythonToolOptions): ToolDefinition<typeof ipythonSchema, IpythonToolDetails>;
export declare function createIpythonTool(cwd: string, options?: IpythonToolOptions): AgentTool<typeof ipythonSchema>;
export {};
//# sourceMappingURL=ipython.d.ts.map