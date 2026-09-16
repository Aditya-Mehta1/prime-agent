//! Settings storage: global (agentDir/settings.json) + project
//! (cwd/<config-dir>/settings.json) files with lock-retry and atomic writes.
//! Port of `FileSettingsStorage` / `InMemorySettingsStorage`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, Result};

/// The TS `CONFIG_DIR_NAME` (pkg.piConfig.configDir fallback).
pub const CONFIG_DIR_NAME: &str = ".prime/agent";

/// Scope of a settings document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsScope {
    Global,
    Project,
}

/// Read/modify/write under a per-file advisory lock. `update` returns the next
/// document or `None` to leave the file unchanged (TS `withLock`).
pub trait SettingsStorage: Send + Sync {
    fn with_lock(
        &self,
        scope: SettingsScope,
        update: &mut dyn FnMut(Option<String>) -> Option<String>,
    ) -> Result<()>;
}

/// File-backed storage with `.lock` sidecar files (flock), retrying briefly on
/// contention like the TS `acquireLockSyncWithRetry` (10 x 20ms).
pub struct FileSettingsStorage {
    global_path: PathBuf,
    project_path: PathBuf,
}

struct LockGuard {
    file: fs::File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl FileSettingsStorage {
    pub fn new(cwd: impl Into<PathBuf>, agent_dir: impl Into<PathBuf>) -> Self {
        let agent_dir: PathBuf = agent_dir.into();
        FileSettingsStorage {
            global_path: agent_dir.join("settings.json"),
            project_path: cwd.into().join(CONFIG_DIR_NAME).join("settings.json"),
        }
    }

    fn path(&self, scope: SettingsScope) -> &Path {
        match scope {
            SettingsScope::Global => &self.global_path,
            SettingsScope::Project => &self.project_path,
        }
    }

    fn acquire_lock(&self, path: &Path) -> Result<LockGuard> {
        use std::os::unix::io::AsRawFd;
        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        let max_attempts = 10;
        let mut last_error: Option<std::io::Error> = None;
        for _ in 1..=max_attempts {
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)?;
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                return Ok(LockGuard { file });
            }
            if last_error.is_none() {
                last_error = Some(
                    file.metadata()
                        .err()
                        .unwrap_or_else(|| std::io::Error::other("lock busy")),
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Err(anyhow!(
            "Failed to acquire settings lock: {}",
            last_error
                .map(|e| e.to_string())
                .unwrap_or_else(|| "busy".into())
        ))
    }
}

impl SettingsStorage for FileSettingsStorage {
    fn with_lock(
        &self,
        scope: SettingsScope,
        update: &mut dyn FnMut(Option<String>) -> Option<String>,
    ) -> Result<()> {
        let path = self.path(scope);
        let file_exists = path.exists();
        let mut held: Option<LockGuard> = None;
        if file_exists {
            held = Some(self.acquire_lock(path)?);
        }
        let current = if file_exists {
            Some(fs::read_to_string(path)?)
        } else {
            None
        };
        let mut next = update(current);
        if next.is_some() {
            if let Some(dir) = path.parent() {
                if !dir.exists() {
                    fs::create_dir_all(dir)?;
                }
            }
            if held.is_none() {
                held = Some(self.acquire_lock(path)?);
                // A racing first writer may have landed since the unlocked read.
                if path.exists() {
                    next = update(Some(fs::read_to_string(path)?));
                }
            }
            if let Some(content) = next {
                atomic_write(path, &content)?;
            }
        }
        drop(held);
        Ok(())
    }
}

/// Atomic write: temp file + rename, 0o600 like `writeFileAtomicSync`.
pub fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let temp = PathBuf::from(format!("{}.tmp{}", path.display(), std::process::id()));
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)?;
    Ok(())
}

/// In-memory storage (tests, embedded hosts).
#[derive(Default)]
pub struct InMemorySettingsStorage {
    global: Mutex<Option<String>>,
    project: Mutex<Option<String>>,
}

impl SettingsStorage for InMemorySettingsStorage {
    fn with_lock(
        &self,
        scope: SettingsScope,
        update: &mut dyn FnMut(Option<String>) -> Option<String>,
    ) -> Result<()> {
        let slot = match scope {
            SettingsScope::Global => &self.global,
            SettingsScope::Project => &self.project,
        };
        let mut guard = slot.lock().unwrap();
        if let Some(next) = update(guard.clone()) {
            *guard = Some(next);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_round_trip() {
        let storage = InMemorySettingsStorage::default();
        storage
            .with_lock(SettingsScope::Global, &mut |current| {
                assert_eq!(current, None);
                Some(r#"{ "theme": "prime" }"#.to_string())
            })
            .unwrap();
        storage
            .with_lock(SettingsScope::Global, &mut |current| {
                assert!(current.unwrap().contains("prime"));
                None
            })
            .unwrap();
    }

    #[test]
    fn file_storage_read_modify_write() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileSettingsStorage::new(dir.path().join("cwd"), dir.path().join("agent"));
        storage
            .with_lock(SettingsScope::Global, &mut |current| {
                assert_eq!(current, None);
                Some(r#"{ "defaultProvider": "prime-inference" }"#.to_string())
            })
            .unwrap();
        let path = dir.path().join("agent").join("settings.json");
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("prime-inference"));
        // 0o600.
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
