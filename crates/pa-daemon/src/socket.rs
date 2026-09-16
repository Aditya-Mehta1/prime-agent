//! Daemon socket paths and lifecycle (port of daemon-socket.ts).

use anyhow::{anyhow, Result};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;

pub const DAEMON_SOCKET_MODE: u32 = 0o600;

pub fn socket_dir() -> PathBuf {
    let uid = nix_uid().unwrap_or_else(|| "user".to_string());
    let tmp = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    tmp.join(format!("prime-agent-{uid}"))
}

fn nix_uid() -> Option<String> {
    // Read the uid without libc: /proc/self/status on Linux, fallback to
    // HOME-derived uniqueness elsewhere.
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

pub fn default_daemon_socket_path() -> PathBuf {
    socket_dir().join("daemon.sock")
}

pub fn worker_socket_path(supervisor_socket_path: &Path, worker_id: &str) -> PathBuf {
    let key = crate::paths::hash_key(&supervisor_socket_path.to_string_lossy(), 12);
    socket_dir().join(format!(
        "worker-{key}-{}.sock",
        &worker_id[..12.min(worker_id.len())]
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketIdentity {
    pub dev: u64,
    pub ino: u64,
}

pub fn socket_identity(path: &Path) -> Option<SocketIdentity> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    Some(SocketIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

/// Try to connect to a socket within `timeout`; true when a peer accepts.
pub async fn can_connect(path: &Path, timeout: Duration) -> bool {
    let connect = UnixStream::connect(path);
    match tokio::time::timeout(timeout, connect).await {
        Ok(Ok(stream)) => {
            drop(stream);
            true
        }
        _ => false,
    }
}

/// Remove a stale socket file after verifying nothing is listening.
pub async fn prepare_socket_path(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        crate::paths::ensure_dir(parent)?;
    }
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !metadata.file_type().is_socket() {
        return Err(anyhow!(
            "Daemon socket path exists and is not a socket: {}",
            path.display()
        ));
    }
    let stale_identity = SocketIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    };
    if can_connect(path, Duration::from_millis(250)).await {
        return Err(anyhow!("Daemon socket already in use: {}", path.display()));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1000);
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
        if !path.exists() {
            return Ok(());
        }
        match socket_identity(path) {
            None => return Ok(()),
            Some(current) if current == stale_identity => {}
            Some(_) => {
                return Err(anyhow!(
                    "Daemon socket changed ownership while waiting for cleanup: {}",
                    path.display()
                ))
            }
        }
        if can_connect(path, Duration::from_millis(250)).await {
            return Err(anyhow!("Daemon socket already in use: {}", path.display()));
        }
    }
    std::fs::remove_file(path)?;
    Ok(())
}

/// Remove the socket file when it still belongs to this supervisor incarnation.
pub fn cleanup_socket_path(path: &Path, expected_identity: Option<SocketIdentity>) {
    if !path.exists() {
        return;
    }
    if let Some(expected) = expected_identity {
        match socket_identity(path) {
            Some(current) if current == expected => {}
            _ => return,
        }
    }
    let _ = std::fs::remove_file(path);
}

pub fn restrict_socket_path(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(DAEMON_SOCKET_MODE));
    }
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
        assert_eq!(a, worker_socket_path(supervisor, "0123456789abffff"));
        assert!(a.starts_with(socket_dir()));
    }
}
