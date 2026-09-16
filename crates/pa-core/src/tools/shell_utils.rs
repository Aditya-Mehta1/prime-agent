//! Shell configuration and process helpers.
//!
//! Port of `packages/coding-agent/src/utils/shell.ts` (POSIX behavior) plus the
//! `waitForChildProcess` semantics of `utils/child-process.ts` that the bash
//! tool relies on.

/// Shell program plus the fixed argument list used to run a command string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellConfig {
    pub shell: String,
    pub args: Vec<String>,
}

/// Resolve the shell to run commands with.
///
/// Order: explicit `custom_shell_path` (must exist), `/bin/bash`, bash on PATH,
/// then `sh`.
pub fn get_shell_config(custom_shell_path: Option<&str>) -> anyhow::Result<ShellConfig> {
    if let Some(path) = custom_shell_path {
        if std::path::Path::new(path).exists() {
            return Ok(ShellConfig {
                shell: path.to_string(),
                args: vec!["-c".to_string()],
            });
        }
        return Err(anyhow::anyhow!("Custom shell path not found: {path}"));
    }

    if std::path::Path::new("/bin/bash").exists() {
        return Ok(ShellConfig {
            shell: "/bin/bash".to_string(),
            args: vec!["-c".to_string()],
        });
    }

    if let Some(bash) = find_bash_on_path() {
        return Ok(ShellConfig {
            shell: bash,
            args: vec!["-c".to_string()],
        });
    }

    Ok(ShellConfig {
        shell: "sh".to_string(),
        args: vec!["-c".to_string()],
    })
}

fn find_bash_on_path() -> Option<String> {
    let out = std::process::Command::new("which")
        .arg("bash")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.trim().lines().next()?;
    if first.is_empty() {
        None
    } else {
        Some(first.to_string())
    }
}

/// Absolute default shell for the kernel's `bash()`: explicit path wins;
/// POSIX uses `/bin/bash` else `/bin/sh`.
pub fn resolve_kernel_bash_shell(custom_shell_path: Option<&str>) -> Option<String> {
    if let Some(explicit) = custom_shell_path.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(explicit.to_string());
    }
    if std::path::Path::new("/bin/bash").exists() {
        Some("/bin/bash".to_string())
    } else {
        Some("/bin/sh".to_string())
    }
}

/// The agent config directory (`~/.prime/agent` unless overridden).
pub fn get_agent_dir() -> String {
    if let Ok(dir) = std::env::var("PI_CODING_AGENT_DIR") {
        if dir.starts_with('~') {
            if let Some(rest) = dir.strip_prefix("~/") {
                return format!("{}/{}", home_dir(), rest);
            }
        }
        return dir;
    }
    format!("{}/.prime/agent", home_dir())
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// Directory containing the agent's bundled binaries, prepended to PATH.
pub fn get_bin_dir() -> String {
    format!("{}/bin", get_agent_dir())
}

/// Environment for agent-spawned shells.
///
/// Prepends the agent bin dir to PATH and forces non-interactive settings so
/// prompts, pagers, and editors fail fast instead of hanging on dead stdin.
pub fn get_shell_env() -> std::collections::HashMap<String, String> {
    let mut env: std::collections::HashMap<String, String> = std::env::vars()
        .filter(|(_, v)| !v.contains('\0'))
        .collect();
    let bin_dir = get_bin_dir();
    let path_key = env
        .keys()
        .find(|k| k.eq_ignore_ascii_case("path"))
        .cloned()
        .unwrap_or_else(|| "PATH".to_string());
    let current_path = env.get(&path_key).cloned().unwrap_or_default();
    if !current_path
        .split(':')
        .filter(|p| !p.is_empty())
        .any(|p| p == bin_dir)
    {
        let updated = if current_path.is_empty() {
            bin_dir
        } else {
            format!("{bin_dir}:{current_path}")
        };
        env.insert(path_key, updated);
    }
    env.insert("GIT_EDITOR".into(), "true".into());
    env.insert("GIT_SEQUENCE_EDITOR".into(), "true".into());
    env.insert("GIT_TERMINAL_PROMPTS".into(), "0".into());
    env.insert("GIT_ASKPASS".into(), "true".into());
    env.insert("SSH_ASKPASS_REQUIRE".into(), "never".into());
    env.insert("EDITOR".into(), "true".into());
    env.insert("VISUAL".into(), "true".into());
    env.insert("PAGER".into(), "cat".into());
    env.insert("GIT_PAGER".into(), "cat".into());
    env.insert("DEBIAN_FRONTEND".into(), "noninteractive".into());
    env
}

/// Sanitize binary output for display/storage.
///
/// Removes control characters (except tab, newline, carriage return) and
/// Unicode format characters; lone surrogates cannot occur in Rust strings.
pub fn sanitize_binary_output(s: &str) -> String {
    s.chars()
        .filter(|&ch| {
            let code = ch as u32;
            // Allow tab, newline, carriage return.
            if code == 0x09 || code == 0x0a || code == 0x0d {
                return true;
            }
            // Control characters.
            if code <= 0x1f {
                return false;
            }
            // Unicode format characters that crash string-width.
            if (0xfff9..=0xfffb).contains(&code) {
                return false;
            }
            true
        })
        .collect()
}

/// Kill a process and all its children.
///
/// POSIX: SIGKILL the process group (children run detached in a new group),
/// falling back to the direct pid.
pub fn kill_process_tree(pid: i32) {
    unsafe {
        if libc::kill(-pid, libc::SIGKILL) != 0 {
            // Fallback to killing just the child if the group kill fails.
            let _ = libc::kill(pid, libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_config_prefers_bin_bash() {
        let cfg = get_shell_config(None).unwrap();
        assert_eq!(cfg.shell, "/bin/bash");
        assert_eq!(cfg.args, vec!["-c".to_string()]);
    }

    #[test]
    fn shell_env_disables_prompts() {
        let env = get_shell_env();
        assert_eq!(env.get("GIT_TERMINAL_PROMPTS").unwrap(), "0");
        assert_eq!(env.get("PAGER").unwrap(), "cat");
        assert!(env.get("PATH").unwrap().contains("/bin"));
    }

    #[test]
    fn sanitize_removes_control_chars() {
        assert_eq!(sanitize_binary_output("a\u{0}b\u{7}c"), "abc");
        assert_eq!(sanitize_binary_output("a\tb\nc\rd"), "a\tb\nc\rd");
        assert_eq!(sanitize_binary_output("x\u{FFF9}y"), "xy");
    }
}
