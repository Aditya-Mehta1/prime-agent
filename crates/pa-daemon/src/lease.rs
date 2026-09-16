//! Session leases (port of core/session-lease.ts).
//!
//! One process may host one runtime per canonical session file. A lease is a
//! directory `<agent-dir>/session-leases/<sha256(path)>.lock` containing
//! `owner.json`; acquisition is an atomic rename of a candidate directory, and
//! stale owners (dead pid, or a recycled pid whose start identity changed) are
//! reclaimed. A separate guard lock serializes lease updates.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const SESSION_LEASES_ENABLED_ENV: &str = "PRIME_AGENT_INTERNAL_SESSION_LEASES";
pub const SESSION_LEASE_OWNER_ID_ENV: &str = "PRIME_AGENT_INTERNAL_SESSION_LEASE_OWNER_ID";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeaseOwner {
    version: u32,
    token: String,
    pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_start_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_session_id: Option<String>,
    session_path: String,
    created_at: String,
}

/// Error matching the TS wire shape (`session_already_active`).
#[derive(Debug, thiserror::Error)]
#[error("Session is already active in {owner}: {session_path}")]
pub struct SessionAlreadyActiveError {
    pub session_path: String,
    pub active_session_id: Option<String>,
    pub owner: String,
}

impl SessionAlreadyActiveError {
    fn for_owner(session_path: &str, owner: Option<&LeaseOwner>) -> Self {
        SessionAlreadyActiveError {
            session_path: session_path.to_string(),
            active_session_id: owner
                .and_then(|o| o.active_session_id.clone())
                .filter(|id| !id.is_empty()),
            owner: owner
                .and_then(|o| o.active_session_id.clone())
                .unwrap_or_else(|| "another process".to_string()),
        }
    }
}

pub fn canonical_session_path(path: &Path) -> PathBuf {
    match path.canonicalize() {
        Ok(canonical) => canonical,
        Err(_) => match path.parent().map(|parent| parent.canonicalize()) {
            Some(Ok(parent)) => parent.join(path.file_name().unwrap_or_default()),
            _ => path.to_path_buf(),
        },
    }
}

/// `proc:<starttime>` start identity (TS `getProcessStartId`); shared with
/// pa-core through `pa_types::platform`.
pub fn get_process_start_id(pid: u32) -> Option<String> {
    pa_types::platform::process::process_start_id(pid)
}

/// True only for a process that is actually running: zombies do not count.
/// Errors when the platform cannot answer (the caller treats an unverifiable
/// owner as alive rather than reclaiming its lease).
pub fn is_process_alive(pid: u32) -> anyhow::Result<bool> {
    pa_types::platform::process::is_process_alive(pid)
}

fn lease_directory(agent_dir: &Path, session_path: &Path) -> PathBuf {
    let canonical = canonical_session_path(session_path);
    let key = Sha256::digest(canonical.to_string_lossy().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    agent_dir.join("session-leases").join(format!("{key}.lock"))
}

fn leases_enabled() -> bool {
    matches!(
        std::env::var(SESSION_LEASES_ENABLED_ENV).as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn read_owner(directory: &Path) -> Result<Option<LeaseOwner>> {
    let owner_path = directory.join("owner.json");
    let content = match fs::read_to_string(&owner_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let owner: LeaseOwner = serde_json::from_str(&content).map_err(|e| {
        anyhow!(
            "Corrupt session lease owner file: {} - {e}",
            owner_path.display()
        )
    })?;
    Ok(Some(owner))
}

fn owner_alive(owner: &LeaseOwner) -> bool {
    match is_process_alive(owner.pid) {
        Ok(true) => {}
        // A provably-dead owner is stale; an unverifiable one counts as
        // alive, like the TS lease (reclaiming a live owner is worse).
        Ok(false) => return false,
        Err(_) => return true,
    }
    match owner.process_start_id.as_deref() {
        None => true,
        Some(expected) => match get_process_start_id(owner.pid) {
            Some(current) => current == expected,
            // Unobservable identity counts as alive, like the TS lease.
            None => true,
        },
    }
}

fn reclaim_stale(directory: &Path) -> bool {
    let stale = directory.with_extension(format!(
        "lock.stale-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    match fs::rename(directory, &stale) {
        Ok(()) => {
            let _ = fs::remove_dir_all(&stale);
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

/// Serialize lease-directory mutations with a guard directory lock.
fn with_lease_guard<T>(directory: &Path, action: impl FnOnce() -> Result<T>) -> Result<T> {
    let guard = PathBuf::from(format!("{}.guard", directory.display()));
    let mut acquired = false;
    for attempt in 0..100u32 {
        match fs::create_dir(&guard) {
            Ok(()) => {
                acquired = true;
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // Stale guard: a holder that died without cleanup.
                if crate::paths::mtime_age(&guard).is_some_and(|age| age > Duration::from_secs(5)) {
                    let _ = fs::remove_dir_all(&guard);
                    continue;
                }
                std::thread::sleep(Duration::from_millis(10 + (attempt % 5) as u64));
            }
            Err(error) => return Err(error.into()),
        }
    }
    if !acquired {
        return Err(anyhow!(
            "Could not coordinate session lease: {}",
            directory.display()
        ));
    }
    let result = action();
    let _ = fs::remove_dir_all(&guard);
    result
}

/// A held session lease; release removes the directory when still owned.
#[derive(Debug)]
pub struct SessionLease {
    pub session_path: PathBuf,
    directory: PathBuf,
    token: String,
    released: std::sync::atomic::AtomicBool,
}

impl SessionLease {
    pub fn release(&self) {
        if self
            .released
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let _ = with_lease_guard(&self.directory, || {
            if let Ok(Some(owner)) = read_owner(&self.directory) {
                if owner.token == self.token {
                    reclaim_stale(&self.directory);
                }
            }
            Ok(())
        });
    }
}

/// Acquire the lease for one session file. Returns `None` when leases are
/// disabled (default) or `session_path` is empty.
pub fn acquire_session_lease(
    session_path: Option<&Path>,
    agent_dir: &Path,
) -> Result<Option<SessionLease>> {
    let Some(session_path) = session_path.filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(None);
    };
    if !leases_enabled() {
        return Ok(None);
    }
    let canonical = canonical_session_path(session_path);
    let root = agent_dir.join("session-leases");
    fs::create_dir_all(&root)?;
    let directory = lease_directory(agent_dir, &canonical);

    with_lease_guard(&directory, || {
        for _ in 0..3 {
            let token = uuid::Uuid::new_v4().to_string();
            let candidate = directory.with_extension(format!(
                "lock.candidate-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&candidate)?;
            let owner = LeaseOwner {
                version: 1,
                token: token.clone(),
                pid: std::process::id(),
                process_start_id: get_process_start_id(std::process::id()),
                active_session_id: std::env::var(SESSION_LEASE_OWNER_ID_ENV).ok(),
                session_path: canonical.to_string_lossy().to_string(),
                created_at: crate::util::now_iso(),
            };
            let owner_path = candidate.join("owner.json");
            fs::write(&owner_path, serde_json::to_string_pretty(&owner)? + "\n")?;
            match fs::rename(&candidate, &directory) {
                Ok(()) => {
                    return Ok(Some(SessionLease {
                        session_path: canonical.clone(),
                        directory: directory.clone(),
                        token,
                        released: std::sync::atomic::AtomicBool::new(false),
                    }));
                }
                Err(error) => {
                    let _ = fs::remove_dir_all(&candidate);
                    if error.kind() == std::io::ErrorKind::NotFound {
                        continue;
                    }
                    if error.kind() == std::io::ErrorKind::AlreadyExists
                        || error.kind() == std::io::ErrorKind::DirectoryNotEmpty
                    {
                        match read_owner(&directory)? {
                            Some(existing) if owner_alive(&existing) => {
                                return Err(SessionAlreadyActiveError::for_owner(
                                    &canonical.to_string_lossy(),
                                    Some(&existing),
                                )
                                .into());
                            }
                            _ => {
                                reclaim_stale(&directory);
                                continue;
                            }
                        }
                    }
                    return Err(error.into());
                }
            }
        }
        match read_owner(&directory)? {
            Some(owner) if owner_alive(&owner) => Err(SessionAlreadyActiveError::for_owner(
                &canonical.to_string_lossy(),
                Some(&owner),
            )
            .into()),
            _ => Err(anyhow!(
                "Could not acquire session lease: {}",
                canonical.display()
            )),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_start_id_reflects_proc() {
        let start = get_process_start_id(std::process::id());
        assert!(start.is_some());
        assert!(start.unwrap().starts_with("proc:"));
        assert!(get_process_start_id(0).is_none());
    }

    #[test]
    fn lease_conflicts_and_releases() {
        let dir = std::env::temp_dir().join(format!("pa-lease-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var(SESSION_LEASES_ENABLED_ENV, "1");
        let session = dir.join("s.jsonl");
        std::fs::write(&session, "{}").unwrap();
        let lease = acquire_session_lease(Some(&session), &dir)
            .unwrap()
            .unwrap();
        // Second holder conflicts.
        let err = acquire_session_lease(Some(&session), &dir).unwrap_err();
        assert!(err.to_string().contains("already active"));
        lease.release();
        // Released lease can be acquired again.
        let second = acquire_session_lease(Some(&session), &dir)
            .unwrap()
            .unwrap();
        second.release();
        std::env::remove_var(SESSION_LEASES_ENABLED_ENV);
        let _ = fs::remove_dir_all(&dir);
    }
}
