#!/usr/bin/env python3
"""FrontierHarness benchmark: Prime Agent + Kimi K3 on Prime Sandboxes."""
import json, os, re, subprocess, sys, time
from pathlib import Path
import tomllib

# Read API key from prime config (the daemon also uses this)
config = json.loads(Path.home().joinpath(".prime/config.json").read_text())
API_KEY = config.get("api_key", "")
TEAM_ID = config.get("team_id", "")
os.environ["PRIME_API_KEY"] = API_KEY
os.environ["PRIME_TEAM_ID"] = TEAM_ID

TASKS_DIR = Path("/Users/milkkarten/Research/frontier-harness-eval/tasks")
BUNDLE = "/tmp/prime-agent-full-final.tar.gz"
OUT_DIR = Path("/tmp/fh-benchmark-results")
OUT_DIR.mkdir(exist_ok=True, parents=True)
LOG = OUT_DIR / "benchmark.log"

def log(msg):
    line = f"[{time.strftime('%H:%M:%S')}] {msg}"
    print(line, flush=True)
    with open(LOG, "a") as f:
        f.write(line + "\n")

def sb_run(sid, cmd, timeout=300):
    try:
        r = subprocess.run(
            ["prime", "sandbox", "run", sid, "--", "sh", "-c", cmd, "--plain"],
            capture_output=True, text=True, timeout=timeout
        )
        return r.stdout + r.stderr
    except subprocess.TimeoutExpired:
        return "TIMEOUT"

def sb_upload(sid, src, dst):
    r = subprocess.run(
        ["prime", "sandbox", "upload", sid, str(src), dst, "--plain"],
        capture_output=True, text=True, timeout=120
    )
    return "success" in r.stdout.lower()

def load_tasks():
    tasks = []
    for td in sorted(TASKS_DIR.iterdir()):
        if not td.is_dir(): continue
        tp, ip = td / "task.toml", td / "instruction.md"
        if not (tp.exists() and ip.exists()): continue
        with open(tp, "rb") as f:
            meta = tomllib.load(f)
        tasks.append({
            "name": td.name,
            "instruction": ip.read_text(),
            "image": meta.get("environment", {}).get("docker_image", ""),
            "cpus": meta.get("environment", {}).get("cpus", 1),
            "mem_mb": meta.get("environment", {}).get("memory_mb", 2048),
            "timeout": int(meta.get("agent", {}).get("timeout_sec", 900)),
            "type": "tb" if "terminal-bench" in meta.get("task", {}).get("name", "") else "deepswe",
        })
    return tasks

SETUP_CMD = (
    "export DEBIAN_FRONTEND=noninteractive; "
    "apt-get update -qq 2>/dev/null; "
    "apt-get install -y -qq curl ca-certificates 2>/dev/null; "
    "curl -fsSL https://deb.nodesource.com/setup_22.x | sh - 2>/dev/null; "
    "apt-get install -y -qq nodejs 2>/dev/null; "
    "curl -LsSf https://astral.sh/uv/install.sh | sh 2>/dev/null; "
    "mkdir -p /tmp/bundle; tar xzf /tmp/bundle.tar.gz -C /tmp/bundle; "
    "cd /tmp/bundle; npm install undici --silent 2>/dev/null; "
    "node --version && echo SETUP_OK"
)

def run_task(task):
    name = task["name"]
    result_file = OUT_DIR / f"{name}.json"
    if result_file.exists():
        log(f"  {name}: already done")
        return json.loads(result_file.read_text())

    log(f"  {name}: creating sandbox (image={task['image'][:40]}...)")
    mem_gb = task["mem_mb"] / 1024
    r = subprocess.run(
        ["prime", "sandbox", "create", task["image"],
         "--name", f"fh-{name[:20]}", "--cpu-cores", str(task["cpus"]),
         "--memory-gb", str(mem_gb), "--timeout-minutes", "45",
         "--yes", "--plain"],
        capture_output=True, text=True, timeout=120
    )
    m = re.search(r"Successfully created sandbox ([a-z0-9]+)", r.stdout)
    if not m:
        log(f"  {name}: SANDBOX_CREATE_FAILED")
        return {"task": name, "resolved": False, "error": "create_failed"}
    sid = m.group(1)

    try:
        # Upload
        instr_file = Path(f"/tmp/fh-instr-{name}.txt")
        instr_file.write_text(task["instruction"])
        if not sb_upload(sid, BUNDLE, "/tmp/bundle.tar.gz"):
            log(f"  {name}: BUNDLE_UPLOAD_FAILED, retrying...")
            time.sleep(5)
            if not sb_upload(sid, BUNDLE, "/tmp/bundle.tar.gz"):
                return {"task": name, "resolved": False, "error": "upload_failed"}
        sb_upload(sid, instr_file, "/tmp/instruction.txt")

        # Setup
        log(f"  {name}: setting up...")
        setup_out = sb_run(sid, SETUP_CMD, timeout=300)
        if "SETUP_OK" not in setup_out:
            log(f"  {name}: SETUP_FAILED: {setup_out[-200:]}")
            return {"task": name, "resolved": False, "error": "setup_failed"}

        # Run script
        run_script = (
            "#!/bin/bash\n"
            'export PATH="$HOME/.local/bin:$PATH"\n'
            f"export PRIME_API_KEY={API_KEY}\n"
            f"export PRIME_TEAM_ID={TEAM_ID}\n"
            "export PRIME_AGENT_INSTALL_UV=1\n"
            "cd /app 2>/dev/null || cd /work 2>/dev/null || cd /\n"
            "INSTR=$(cat /tmp/instruction.txt)\n"
            f"exec timeout {min(task['timeout'], 480)} node /tmp/bundle/cli.js "
            "-p --provider prime-inference --model moonshotai/kimi-k3 \"$INSTR\" 2>&1\n"
        )
        run_file = Path(f"/tmp/fh-run-{name}.sh")
        run_file.write_text(run_script)
        sb_upload(sid, run_file, "/tmp/run-task.sh")

        # Run agent
        agent_timeout = min(task["timeout"], 480) + 60
        log(f"  {name}: running agent (timeout {min(task['timeout'], 480)}s)...")
        t0 = time.time()
        agent_out = sb_run(sid, "bash /tmp/run-task.sh", timeout=agent_timeout)
        elapsed = time.time() - t0

        # Check output files (task-specific)
        # For TB tasks: check if expected output exists
        output_check = sb_run(sid, "ls /app/ 2>/dev/null | head -20", timeout=30)
        
        result = {
            "task": name,
            "type": task["type"],
            "duration_seconds": elapsed,
            "agent_output": agent_out[:3000],
            "sandbox_files": output_check[:500],
            "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ"),
        }

        result_file.write_text(json.dumps(result, indent=2))
        log(f"  {name}: completed in {elapsed:.0f}s")
        return result

    finally:
        subprocess.run(
            ["prime", "sandbox", "delete", sid, "--yes", "--plain"],
            capture_output=True, text=True, timeout=30
        )

def main():
    tasks = load_tasks()
    log(f"Starting FrontierHarness benchmark: {len(tasks)} tasks")
    log(f"API key: {'OK' if API_KEY else 'MISSING!'}")

    # Determine which tasks to run
    if len(sys.argv) > 1 and sys.argv[1] == "--all":
        to_run = tasks
    else:
        # Run in batches, starting with the easiest
        to_run = tasks

    results = []
    for i, task in enumerate(to_run):
        log(f"\n[{i+1}/{len(to_run)}] {task['name']} ({task['type']})")
        try:
            result = run_task(task)
            results.append(result)
        except Exception as e:
            log(f"  {task['name']}: ERROR {e}")
            results.append({"task": task["name"], "resolved": False, "error": str(e)})

    # Summary
    log(f"\n{'='*60}")
    log(f"BENCHMARK COMPLETE: {len(results)} tasks")
    log(f"{'='*60}")

    summary = {
        "completed_at": time.strftime("%Y-%m-%dT%H:%M:%SZ"),
        "total_tasks": len(results),
        "results": results,
    }
    (OUT_DIR / "summary.json").write_text(json.dumps(summary, indent=2))
    log(f"Summary written to {OUT_DIR / 'summary.json'}")

if __name__ == "__main__":
    main()
