//! Per-OS daemon endpoint naming (TS: `daemon-socket.ts`
//! `defaultDaemonSocketPath` / `daemon-supervisor.ts` `workerSocketPath`).
//!
//! Unix: socket files under `<tmpdir>/prime-agent-<uid>/`. Windows: named
//! pipes in the `\\.\pipe\` namespace (fixed daemon pipe name, hashed worker
//! pipe names) - the TS product's exact split.

use std::path::{Path, PathBuf};

use crate::paths::hash_key;

/// Default directory holding daemon socket files (Unix).
#[cfg(unix)]
pub fn socket_dir() -> PathBuf {
    let uid = current_uid().unwrap_or_else(|| "user".to_string());
    let tmp = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    tmp.join(format!("prime-agent-{uid}"))
}

/// Read the effective uid without libc: `/proc/self/status` on Linux,
/// HOME-derived uniqueness elsewhere (best-effort, same as today).
#[cfg(unix)]
fn current_uid() -> Option<String> {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Uid:") {
                if let Some(first) = rest.split_whitespace().next() {
                    return Some(first.to_string());
                }
            }
        }
    }
    None
}

/// Default supervisor endpoint: `daemon.sock` in the socket dir (Unix) or
/// the fixed daemon pipe name (Windows).
#[cfg(unix)]
pub fn default_daemon_socket_path() -> PathBuf {
    socket_dir().join("daemon.sock")
}

#[cfg(not(unix))]
pub fn default_daemon_socket_path() -> PathBuf {
    PathBuf::from(r"\\.\pipe\prime-agent-daemon")
}

/// Worker endpoint next to the supervisor's: hashed supervisor key plus the
/// worker id prefix (TS `workerSocketPath`).
#[cfg(unix)]
pub fn worker_socket_path(supervisor_socket_path: &Path, worker_id: &str) -> PathBuf {
    let key = hash_key(&supervisor_socket_path.to_string_lossy(), 12);
    socket_dir().join(format!(
        "worker-{key}-{}.sock",
        &worker_id[..12.min(worker_id.len())]
    ))
}

#[cfg(not(unix))]
pub fn worker_socket_path(supervisor_socket_path: &Path, worker_id: &str) -> PathBuf {
    let key = hash_key(&supervisor_socket_path.to_string_lossy(), 12);
    PathBuf::from(format!(
        r"\\.\pipe\prime-agent-worker-{key}-{}",
        &worker_id[..12.min(worker_id.len())]
    ))
}

/// Filesystem identity of a bound socket, guarding stale-file cleanup against
/// a socket re-created by another supervisor incarnation. `None` on Windows:
/// named pipes leave no file to clean up (TS treats identity as undefined
/// there and skips the ownership check).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketIdentity {
    pub dev: u64,
    pub ino: u64,
}

#[cfg(unix)]
pub fn socket_identity(path: &Path) -> Option<SocketIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path).ok()?;
    Some(SocketIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

#[cfg(not(unix))]
pub fn socket_identity(_path: &Path) -> Option<SocketIdentity> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_socket_names_are_deterministic() {
        let supervisor = Path::new("/tmp/prime-agent-1/daemon.sock");
        let a = worker_socket_path(supervisor, "0123456789abcdef");
        let b = worker_socket_path(supervisor, "fedcba9876543210");
        assert_ne!(a, b);
        // Only the first 12 id characters key the name.
        assert_eq!(a, worker_socket_path(supervisor, "0123456789abffff"));
        #[cfg(unix)]
        assert!(a.starts_with(socket_dir()));
    }
}
