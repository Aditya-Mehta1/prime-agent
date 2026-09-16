//! Cross-process file locking.
//!
//! Unix: `flock(2)` on a lock sidecar file (exclusive, non-blocking; callers
//! own retry policy). Windows: `LockFileEx` is the planned implementation -
//! acquiring on Windows fails with a clear error until that lane lands, never
//! by silently proceeding unlocked.

#[cfg(unix)]
use std::fs;
use std::path::Path;

/// An exclusive advisory lock on an open file, released on drop.
pub struct FileLock {
    #[cfg(unix)]
    file: fs::File,
}

impl FileLock {
    /// Open (creating if needed) the lock file and take an exclusive
    /// non-blocking lock. Errors when the file exists but is locked by
    /// another process (callers retry on contention).
    #[cfg(unix)]
    pub fn acquire_exclusive_non_blocking(path: &Path) -> std::io::Result<Self> {
        use std::os::unix::io::AsRawFd;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(FileLock { file })
    }

    #[cfg(not(unix))]
    pub fn acquire_exclusive_non_blocking(_path: &Path) -> std::io::Result<Self> {
        Err(std::io::Error::other(
            "Windows file locking (LockFileEx) is not yet implemented",
        ))
    }
}

#[cfg(unix)]
impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}
