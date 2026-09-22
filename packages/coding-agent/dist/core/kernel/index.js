/**
 * Persistent IPython kernel manager.
 *
 * Spawns an IPython kernel as a child process and drives it via Jupyter's
 * ZMQ messaging protocol. Variables, imports, and loaded data persist
 * across `execute()` calls — this is the substrate the `ipython` tool
 * runs against. RLM-1 was trained to exploit kernel persistence (load
 * data once, slice across turns), so a stateful kernel is required, not
 * a per-call subprocess.
 */
import { spawn } from "node:child_process";
import { createHmac, randomBytes } from "node:crypto";
import { existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { v4 as uuid } from "uuid";
import { Dealer, Subscriber } from "zeromq";
const DELIM = Buffer.from("<IDS|MSG>");
const PROTOCOL_VERSION = "5.3";
const STARTUP_DELAY_MS = 500;
// ---- wire format ---------------------------------------------------------
function buildMessage(msgType, content, session, username) {
    return {
        header: {
            msg_id: uuid(),
            session,
            username,
            date: new Date().toISOString(),
            msg_type: msgType,
            version: PROTOCOL_VERSION,
        },
        parent_header: {},
        metadata: {},
        content,
    };
}
function sign(parts, key) {
    const hmac = createHmac("sha256", key);
    for (const p of parts)
        hmac.update(p);
    return Buffer.from(hmac.digest("hex"));
}
function encode(msg, key) {
    const parts = [
        Buffer.from(JSON.stringify(msg.header)),
        Buffer.from(JSON.stringify(msg.parent_header)),
        Buffer.from(JSON.stringify(msg.metadata)),
        Buffer.from(JSON.stringify(msg.content)),
    ];
    return [DELIM, sign(parts, key), ...parts];
}
function decode(frames) {
    let i = 0;
    while (i < frames.length && !frames[i].equals(DELIM))
        i++;
    if (i + 5 >= frames.length)
        return null;
    try {
        return {
            header: JSON.parse(frames[i + 2].toString()),
            parent_header: JSON.parse(frames[i + 3].toString()),
            metadata: JSON.parse(frames[i + 4].toString()),
            content: JSON.parse(frames[i + 5].toString()),
        };
    }
    catch {
        return null;
    }
}
// ---- connection setup ----------------------------------------------------
function pickPorts(n) {
    // Bind-to-0 + zeromq across all five sockets is fiddly. Random high
    // ports are sufficient given kernel sessions are local and short-lived.
    // Collisions surface on socket connect.
    const start = 50000 + Math.floor(Math.random() * 5000);
    return Array.from({ length: n }, (_, i) => start + i);
}
function makeConnection() {
    const [shell_port, iopub_port, stdin_port, control_port, hb_port] = pickPorts(5);
    const info = {
        ip: "127.0.0.1",
        transport: "tcp",
        shell_port,
        iopub_port,
        stdin_port,
        control_port,
        hb_port,
        signature_scheme: "hmac-sha256",
        key: randomBytes(16).toString("hex"),
        kernel_name: "python3",
    };
    const tempDir = mkdtempSync(join(tmpdir(), "prime-agent-kernel-"));
    const path = join(tempDir, "connection.json");
    writeFileSync(path, JSON.stringify(info, null, 2));
    return { info, path, tempDir };
}
// ---- kernel manager ------------------------------------------------------
export class KernelManager {
    options;
    session = uuid();
    kernel;
    shell;
    iopub;
    control;
    connection;
    tempDir;
    /** Serializes execute() calls — Jupyter shell channel is request/reply. */
    executionQueue = Promise.resolve();
    state = "idle";
    constructor(options) {
        if (!existsSync(options.python)) {
            throw new Error(`Python interpreter not found: ${options.python}`);
        }
        this.options = {
            python: options.python,
            cwd: options.cwd,
            username: options.username ?? "prime-agent",
        };
    }
    async start() {
        if (this.state !== "idle")
            return;
        this.state = "starting";
        const { info, path: connectionPath, tempDir } = makeConnection();
        this.connection = info;
        this.tempDir = tempDir;
        this.kernel = spawn(this.options.python, ["-m", "ipykernel_launcher", "-f", connectionPath], {
            cwd: this.options.cwd,
            stdio: ["ignore", "pipe", "pipe"],
        });
        // Surface kernel stderr — useful for diagnosing startup failures.
        this.kernel.stderr?.on("data", (buf) => {
            process.stderr.write(`[kernel] ${buf.toString()}`);
        });
        this.kernel.on("exit", (code, signal) => {
            if (this.state !== "shutdown") {
                console.error(`[kernel] unexpected exit code=${code} signal=${signal}`);
            }
            this.state = "shutdown";
        });
        this.shell = new Dealer();
        this.iopub = new Subscriber();
        this.control = new Dealer();
        this.shell.connect(`${info.transport}://${info.ip}:${info.shell_port}`);
        this.iopub.connect(`${info.transport}://${info.ip}:${info.iopub_port}`);
        this.control.connect(`${info.transport}://${info.ip}:${info.control_port}`);
        this.iopub.subscribe("");
        // ZMQ slow-joiner: give the kernel time to bind ports before publishing.
        await sleep(STARTUP_DELAY_MS);
        this.state = "running";
    }
    async execute(code, opts = {}) {
        if (this.state === "idle")
            await this.start();
        if (this.state === "shutdown") {
            throw new Error("Kernel has been shut down");
        }
        const prev = this.executionQueue;
        let resolveNext = () => { };
        this.executionQueue = new Promise((r) => {
            resolveNext = r;
        });
        await prev;
        const started = Date.now();
        try {
            return await this.executeInner(code, opts, started);
        }
        finally {
            resolveNext();
        }
    }
    async executeInner(code, opts, started) {
        const conn = this.connection;
        const shell = this.shell;
        const iopub = this.iopub;
        const msg = buildMessage("execute_request", {
            code,
            silent: false,
            store_history: true,
            user_expressions: {},
            allow_stdin: false,
            stop_on_error: true,
        }, this.session, this.options.username);
        const requestMsgId = msg.header.msg_id;
        await shell.send(encode(msg, conn.key));
        let stdout = "";
        let stderr = "";
        let result;
        let error;
        let status = "ok";
        const onAbort = () => {
            this.interrupt().catch(() => { });
        };
        opts.signal?.addEventListener("abort", onAbort);
        try {
            for await (const frames of iopub) {
                const incoming = decode(frames);
                if (!incoming)
                    continue;
                if (incoming.parent_header.msg_id !== requestMsgId)
                    continue;
                const t = incoming.header.msg_type;
                if (t === "stream") {
                    const c = incoming.content;
                    if (c.name === "stdout")
                        stdout += c.text;
                    else
                        stderr += c.text;
                    opts.onStream?.(c.text, c.name);
                }
                else if (t === "execute_result") {
                    const c = incoming.content;
                    if (c.data["text/plain"])
                        result = c.data["text/plain"];
                }
                else if (t === "error") {
                    const c = incoming.content;
                    error = c;
                    status = "error";
                }
                else if (t === "status") {
                    const c = incoming.content;
                    if (c.execution_state === "idle")
                        break;
                }
            }
        }
        finally {
            opts.signal?.removeEventListener("abort", onAbort);
        }
        if (opts.signal?.aborted)
            status = "aborted";
        return { stdout, stderr, result, error, status, durationMs: Date.now() - started };
    }
    async interrupt() {
        if (!this.control || !this.connection)
            return;
        const msg = buildMessage("interrupt_request", {}, this.session, this.options.username);
        await this.control.send(encode(msg, this.connection.key));
    }
    async shutdown() {
        if (this.state === "shutdown")
            return;
        this.state = "shutdown";
        try {
            if (this.control && this.connection) {
                const msg = buildMessage("shutdown_request", { restart: false }, this.session, this.options.username);
                await this.control.send(encode(msg, this.connection.key));
                await sleep(200);
            }
        }
        catch {
            // fall through to SIGTERM
        }
        this.shell?.close();
        this.iopub?.close();
        this.control?.close();
        try {
            this.kernel?.kill("SIGTERM");
        }
        catch {
            // already dead
        }
        if (this.tempDir)
            rmSync(this.tempDir, { recursive: true, force: true });
    }
    get isRunning() {
        return this.state === "running";
    }
}
// ---- Python interpreter resolution ---------------------------------------
/**
 * Resolve the Python interpreter to use for the kernel. Searched in order:
 *   1. PRIME_AGENT_KERNEL_PYTHON env var
 *   2. ~/.prime-agent/kernel-venv/bin/python (canonical user-install location)
 *   3. <repo>/.kernel-venv/bin/python (development; created by scripts/setup-kernel-venv.sh)
 */
export function resolveKernelPython() {
    const envOverride = process.env.PRIME_AGENT_KERNEL_PYTHON;
    if (envOverride && existsSync(envOverride))
        return envOverride;
    const home = process.env.HOME;
    if (home) {
        const canonical = join(home, ".prime-agent", "kernel-venv", "bin", "python");
        if (existsSync(canonical))
            return canonical;
    }
    // Walk up from cwd looking for a repo-root .kernel-venv.
    for (let dir = process.cwd(); dir !== "/"; dir = join(dir, "..")) {
        const candidate = join(dir, ".kernel-venv", "bin", "python");
        if (existsSync(candidate))
            return candidate;
    }
    return null;
}
//# sourceMappingURL=index.js.map