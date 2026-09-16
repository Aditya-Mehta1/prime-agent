//! Shell selection for the bash tool and the kernel's `bash()`.
//!
//! Unix resolution today (explicit path, /bin/bash, `which bash`, `sh`).
//! Windows: the TS product resolves Git Bash from canonical install paths,
//! never PATH (a repo-controlled PATH must not pick the kernel shell); that
//! candidate order is the Windows implementation, tracked in
//! docs/windows-readiness.md.

#[cfg(unix)]
use std::path::Path;

/// Shell program plus the fixed argument list used to run a command string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellConfig {
    pub shell: String,
    pub args: Vec<String>,
}

/// Resolve the shell to run commands with, honoring an explicit custom path.
#[cfg(unix)]
pub fn get_shell_config(custom_shell_path: Option<&str>) -> anyhow::Result<ShellConfig> {
    if let Some(path) = custom_shell_path {
        if Path::new(path).exists() {
            return Ok(ShellConfig {
                shell: path.to_string(),
                args: vec!["-c".to_string()],
            });
        }
        return Err(anyhow::anyhow!("Custom shell path not found: {path}"));
    }

    if Path::new("/bin/bash").exists() {
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

#[cfg(not(unix))]
pub fn get_shell_config(_custom_shell_path: Option<&str>) -> anyhow::Result<ShellConfig> {
    anyhow::bail!("Windows shell selection (Git Bash resolution) is not yet implemented")
}

#[cfg(unix)]
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
    (!first.is_empty()).then(|| first.to_string())
}

/// Absolute default shell for the kernel's `bash()`: explicit path wins;
/// POSIX uses `/bin/bash` else `/bin/sh`. `None` when no shell resolves
/// (kernel startup must not fail; `bash()` raises its teaching error).
#[cfg(unix)]
pub fn resolve_kernel_bash_shell(custom_shell_path: Option<&str>) -> Option<String> {
    if let Some(explicit) = custom_shell_path.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(explicit.to_string());
    }
    if Path::new("/bin/bash").exists() {
        Some("/bin/bash".to_string())
    } else {
        Some("/bin/sh".to_string())
    }
}

#[cfg(not(unix))]
pub fn resolve_kernel_bash_shell(custom_shell_path: Option<&str>) -> Option<String> {
    // Windows: canonical Git Bash install paths only (never PATH);
    // implemented with the Windows shell-selection lane.
    custom_shell_path
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}
