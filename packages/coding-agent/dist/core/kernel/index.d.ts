export interface KernelManagerOptions {
    /** Python interpreter that has `ipykernel` available. */
    python: string;
    /** Working directory for the kernel. */
    cwd?: string;
    /** Username to send in message headers. Default: "prime-agent". */
    username?: string;
}
export interface ExecuteOptions {
    /** Abort the execution mid-flight. The kernel is interrupted via the control channel. */
    signal?: AbortSignal;
    /** Called as stdout/stderr arrives, before the call resolves. */
    onStream?: (chunk: string, name: "stdout" | "stderr") => void;
}
export interface ExecuteResult {
    stdout: string;
    stderr: string;
    /** Last `execute_result` payload (text/plain), if the cell produced one. */
    result?: string;
    status: "ok" | "error" | "aborted";
    error?: {
        ename: string;
        evalue: string;
        traceback: string[];
    };
    /** Wall-clock duration in ms. */
    durationMs: number;
}
export declare class KernelManager {
    private readonly options;
    private readonly session;
    private kernel?;
    private shell?;
    private iopub?;
    private control?;
    private connection?;
    private tempDir?;
    /** Serializes execute() calls — Jupyter shell channel is request/reply. */
    private executionQueue;
    private state;
    constructor(options: KernelManagerOptions);
    start(): Promise<void>;
    execute(code: string, opts?: ExecuteOptions): Promise<ExecuteResult>;
    private executeInner;
    private interrupt;
    shutdown(): Promise<void>;
    get isRunning(): boolean;
}
/**
 * Resolve the Python interpreter to use for the kernel. Searched in order:
 *   1. PRIME_AGENT_KERNEL_PYTHON env var
 *   2. ~/.prime-agent/kernel-venv/bin/python (canonical user-install location)
 *   3. <repo>/.kernel-venv/bin/python (development; created by scripts/setup-kernel-venv.sh)
 */
export declare function resolveKernelPython(): string | null;
//# sourceMappingURL=index.d.ts.map