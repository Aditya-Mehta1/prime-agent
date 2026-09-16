//! Differential CLI tests: run a corpus of invocations against both the
//! installed TypeScript `prime-agent` binary (ground truth for parity) and the
//! Rust `prime-agent` binary built by this crate, and assert matching exit
//! codes, stdout, and stderr.
//!
//! Corpus policy: only invocations whose behavior is fully determined by the
//! CLI surface itself (help/version output, argument validation errors, public
//! command routing errors, MCP settings reads/writes). Invocations that reach
//! the daemon, the model runtime, or the package network are excluded because
//! their output depends on machine state or on crates that are not merged yet.
//!
//! The TS binary is located via `PA_TS_BINARY` or `prime-agent` on PATH; the
//! test is skipped (not failed) when it is not installed. Each binary runs in
//! its own sandbox HOME + cwd so stateful cases (mcp add) behave identically.
//! Version numbers are normalized before comparison.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Each corpus case: (argv, needs per-binary sandbox HOME).
const CORPUS: &[&[&str]] = &[
    // Help and version.
    &["--version"],
    &["-v"],
    &["--help"],
    &["-h"],
    &["help"],
    &["help", "help"],
    &["help", "agents"],
    &["help", "list"],
    &["help", "attach"],
    &["help", "stop"],
    &["help", "rename"],
    &["help", "send"],
    &["help", "schedule"],
    &["help", "schedule", "list"],
    &["help", "schedule", "add"],
    &["help", "schedule", "cancel"],
    &["help", "status"],
    &["help", "doctor"],
    &["help", "shutdown"],
    &["help", "mcp"],
    &["help", "mcp", "add"],
    &["help", "mcp", "list"],
    &["help", "mcp", "get"],
    &["help", "mcp", "remove"],
    &["help", "package"],
    &["help", "package", "install"],
    &["help", "package", "remove"],
    &["help", "package", "list"],
    &["help", "package", "update"],
    &["help", "update"],
    &["help", "model"],
    &["help", "model", "list"],
    &["help", "session"],
    &["help", "session", "export"],
    &["help", "config"],
    // Typos produce suggestions.
    &["help", "schedul"],
    &["help", "schedule", "bogus"],
    &["help", "help", "help"],
    // Argument parser diagnostics.
    &["-x"],
    &["-x", "--thinking", "bogus"],
    &["--mode"],
    &["--mode", "bogus"],
    &["--thinking", "bogus"],
    &["--thinking", "off", "--thinking", "bogus"],
    &["--provider"],
    &["--model"],
    &["--api-key"],
    &["--cwd"],
    &["--system-prompt"],
    &["--fork"],
    &["--session-dir"],
    &["--models"],
    &["--tools", "read,write"],
    &["--tools", "read"],
    &["--goal-token-budget", "5"],
    &["--goal", ""],
    &["--goal", "  "],
    &["--autonomous-max-turns", "0"],
    &["--autonomous-max-turns", "abc"],
    &["--autonomous-max-tokens", "-1"],
    &["--export", "foo"],
    &["--export=foo"],
    &["--list-models"],
    &["--list-models", "gpt"],
    &["--list-models=gpt"],
    // Daemon client connect failure against a socket that never exists: the
    // full error text (socket + daemon log path) is deterministic.
    &[
        "--daemon-socket",
        "/nonexistent-pa-daemon-differential.sock",
        "list",
    ],
    &[
        "--daemon-socket",
        "/nonexistent-pa-daemon-differential.sock",
        "list",
        "--json",
    ],
    &[
        "--daemon-socket",
        "/nonexistent-pa-daemon-differential.sock",
        "stop",
        "some-agent",
    ],
    // Flag interdependency validation.
    &["--fork", "x", "--continue"],
    &["--fork", "x", "--resume", "y"],
    &["--fork", "x", "--no-session"],
    &["--resume"],
    &["--resume="],
    &["--mode", "rpc", "@file.txt"],
    &["--mode", "daemon", "@file.txt"],
    &[
        "--cwd",
        "/nonexistent-differential-test-dir",
        "--mode",
        "json",
    ],
    // Removed commands.
    &["daemon"],
    &["daemon", "foo"],
    &["app"],
    &["app", "update"],
    &["install"],
    &["remove"],
    &["uninstall"],
    &["manage"],
    &["manage", "update"],
    // Public command routing and validation.
    &["--offline", "status"],
    &["--verbose", "status", "--json"],
    &["--help", "status"],
    &["status", "--bogus"],
    &["status", "--offline"],
    &["doctor", "--bogus"],
    &["shutdown", "--bogus"],
    &["stop"],
    &["stop", "a", "b"],
    &["rename", "a"],
    &["send", "--help"],
    &["schedule"],
    &["schedule", "bogus"],
    &["schedule", "list", "--bogus"],
    &["schedule", "list", "a", "b"],
    &["schedule", "cancel"],
    &["schedule", "list", "--help"],
    &["attach"],
    &["attach", "--resume"],
    &["attach", "a", "b"],
    &["attach", "a", "--resume"],
    &["attach", "a", "-r"],
    &["attach", "a", "--resume=x"],
    &["attach", "a", "--continue"],
    &["attach", "a", "--fork"],
    // model/session command rewrites.
    &["model"],
    &["model", "bogus"],
    &["model", "list", "x", "y"],
    &["session"],
    &["session", "bogus"],
    &["session", "export"],
    &["session", "export", "a", "b", "c"],
    &["session", "export", "--x"],
    // update command validation.
    &["update", "--self"],
    &["update", "package"],
    &["update", "--extensions"],
    &["update", "--self", "package"],
    &["update", "self", "pkg"],
    &["update", "--bogus"],
    &["update", "--force", "--self"],
    // package command validation.
    &["package"],
    &["package", "bogus"],
    &["package", "uninstall"],
    &["package", "list", "x"],
    &["package", "update", "--self"],
    &["package", "update", "a", "b"],
    &["package", "update", "self"],
    &["package", "update", "prime-agent"],
    &["package", "update", "pi"],
    &["package", "update", "--force"],
    &["package", "install"],
    &["package", "remove"],
    &["package", "install", "--bogus"],
    &["package", "install", "-l"],
    &["package", "install", "-h"],
    &["package", "remove", "-h"],
    &["package", "list", "-h"],
    &["package", "update", "-h"],
    &["package", "update", "--nightly", "--stable"],
    &["package", "update", "--extension", "x", "--self"],
    &["package", "update", "--extension", "x", "y"],
    &["package", "update", "--extensions", "--rollback"],
    &["package", "update", "--stable", "some-source"],
    // MCP management (stateful cases share one sandbox HOME per binary).
    &["mcp"],
    &["mcp", "bogus"],
    &["mcp", "list", "x"],
    &["mcp", "list"],
    &["mcp", "get"],
    &["mcp", "get", "x"],
    &["mcp", "get", "bad name!"],
    &["mcp", "remove", "x"],
    &["mcp", "add"],
    &["mcp", "add", "bad name!"],
    &["mcp", "add", "linear"],
    &["mcp", "add", "x"],
    &["mcp", "add", "x", "--url", "notaurl"],
    &["mcp", "add", "x", "--url", "ftp://foo"],
    &["mcp", "add", "x", "--url", "http://u:p@host"],
    &["mcp", "add", "x", "--url", "http://"],
    &["mcp", "add", "x", "--url"],
    &["mcp", "add", "x", "--bogus", "v"],
    &[
        "mcp",
        "add",
        "x",
        "--url",
        "https://e.com",
        "--url",
        "https://f.com",
    ],
    &[
        "mcp",
        "add",
        "x",
        "--url",
        "https://e.com",
        "--oauth",
        "--bearer-token-env-var",
        "V",
    ],
    &["mcp", "add", "x", "--env", "CHILD"],
    &["mcp", "add", "x", "--env", "CHILD=bad-name"],
    &["mcp", "add", "x", "--env", "=SOURCE"],
    &["mcp", "add", "x", "--cwd", "/tmp", "--url", "https://e.com"],
    &[
        "mcp",
        "add",
        "x",
        "--url",
        "https://e.com",
        "--",
        "run",
        "arg",
    ],
    &["mcp", "add", "y", "--url", "https://example.com/mcp"],
    &["mcp", "list"],
    &["mcp", "get", "y"],
    &["mcp", "add", "y", "--url", "https://example.com/mcp"],
    &[
        "mcp",
        "add",
        "y",
        "--force",
        "--url",
        "https://other.com/mcp",
    ],
    &["mcp", "remove", "y"],
    &["mcp", "list"],
    &["mcp", "remove", "y"],
];

fn ts_binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PA_TS_BINARY") {
        return Some(PathBuf::from(path));
    }
    let found = Command::new("which")
        .arg("prime-agent")
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    let path = String::from_utf8_lossy(&found.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

struct InvocationOutput {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(binary: &Path, args: &[&str], sandbox: &Path) -> InvocationOutput {
    let output = Command::new("timeout")
        .arg("20")
        .arg(binary)
        .args(args)
        .env("HOME", sandbox.join("home"))
        // Isolate the agent state dir: the TS binary resolves the real home
        // through passwd rather than $HOME, so the env override is the only
        // reliable isolation for both binaries.
        .env("PRIME_AGENT_CODING_AGENT_DIR", sandbox.join("agent"))
        .env("PI_OFFLINE", "1")
        .current_dir(sandbox.join("cwd"))
        .current_dir(sandbox.join("cwd"))
        .output()
        .expect("failed to spawn binary under test");
    InvocationOutput {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Normalize version numbers so the TS product version (0.9.x) and the Rust
/// workspace version can be compared structurally.
fn normalize(text: &str, sandbox_roots: &[&Path]) -> String {
    let mut text = text.to_string();
    // Sandbox paths leak into cwd-related error messages; normalize them so
    // the per-binary sandbox roots compare equal.
    for root in sandbox_roots {
        text = text.replace(&root.display().to_string(), "<SANDBOX>");
    }
    normalize_versions(&text)
}

fn normalize_versions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let token = &text[start..i];
            let trailing = token.ends_with('.');
            let version_candidate = token.trim_end_matches('.');
            let parts: Vec<&str> = version_candidate.split('.').collect();
            if parts.len() >= 2
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
            {
                out.push_str(&format!("X{}.X.X", if trailing { "." } else { "" }));
            } else {
                // Non-version digit runs are kept verbatim: trimming here
                // would corrupt adjacent separators (e.g. git fetch ranges).
                out.push_str(token);
            }
        } else {
            let ch = text[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn sandbox(prefix: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "pa-cli-differential-{prefix}-{}",
        std::process::id()
    ));
    for dir in ["home", "cwd", "agent"] {
        std::fs::create_dir_all(base.join(dir)).expect("create sandbox directory");
    }
    base
}

#[test]
fn differential_corpus_matches_ts_binary() {
    let Some(ts) = ts_binary() else {
        eprintln!("SKIPPED: TS prime-agent binary not found (set PA_TS_BINARY)");
        return;
    };
    let rust = PathBuf::from(env!("CARGO_BIN_EXE_prime-agent"));
    let ts_sandbox = sandbox("ts");
    let rs_sandbox = sandbox("rs");
    let sandbox_roots: Vec<&Path> = vec![&ts_sandbox, &rs_sandbox];

    let mut failures: Vec<String> = Vec::new();
    for case in CORPUS {
        let ts_out = run(&ts, case, &ts_sandbox);
        let rs_out = run(&rust, case, &rs_sandbox);
        let ok = ts_out.exit_code == rs_out.exit_code
            && normalize(&ts_out.stdout, &sandbox_roots)
                == normalize(&rs_out.stdout, &sandbox_roots)
            && normalize(&ts_out.stderr, &sandbox_roots)
                == normalize(&rs_out.stderr, &sandbox_roots);
        if !ok {
            failures.push(format!(
                "argv: {:?}\n  ts exit {:?} rs exit {:?}\n  ts stdout: {:?}\n  rs stdout: {:?}\n  ts stderr: {:?}\n  rs stderr: {:?}",
                case,
                ts_out.exit_code,
                rs_out.exit_code,
                ts_out.stdout,
                rs_out.stdout,
                ts_out.stderr,
                rs_out.stderr
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "differential corpus mismatches ({} of {}):\n{}",
        failures.len(),
        CORPUS.len(),
        failures.join("\n---\n")
    );
}

fn run_with_env(
    binary: &Path,
    args: &[&str],
    sandbox: &Path,
    envs: &[(&str, &str)],
) -> InvocationOutput {
    let mut command = Command::new("timeout");
    command
        .arg("20")
        .arg(binary)
        .args(args)
        .env("PRIME_AGENT_CODING_AGENT_DIR", sandbox.join("agent"))
        .env("PI_OFFLINE", "1")
        .current_dir(sandbox.join("cwd"));
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.output().expect("failed to spawn binary under test");
    InvocationOutput {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[test]
fn differential_env_flag_cases_match_ts_binary() {
    let Some(ts) = ts_binary() else {
        eprintln!("SKIPPED: TS prime-agent binary not found (set PA_TS_BINARY)");
        return;
    };
    let rust = PathBuf::from(env!("CARGO_BIN_EXE_prime-agent"));
    let ts_sandbox = sandbox("ts-env");
    let rs_sandbox = sandbox("rs-env");
    let sandbox_roots: Vec<&Path> = vec![&ts_sandbox, &rs_sandbox];

    // PI_STARTUP_BENCHMARK is rejected outside interactive mode.
    for case in [&["-p", "hi"][..], &["--print"][..], &["--mode", "json"][..]] {
        let ts_out = run_with_env(&ts, case, &ts_sandbox, &[("PI_STARTUP_BENCHMARK", "1")]);
        let rs_out = run_with_env(&rust, case, &rs_sandbox, &[("PI_STARTUP_BENCHMARK", "1")]);
        assert_eq!(
            ts_out.exit_code, rs_out.exit_code,
            "case {case:?} exit code"
        );
        assert_eq!(
            normalize(&ts_out.stderr, &sandbox_roots),
            normalize(&rs_out.stderr, &sandbox_roots),
            "case {case:?} stderr"
        );
    }
}
