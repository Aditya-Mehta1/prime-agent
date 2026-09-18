//! Process identity, shared by pa-core (kernel/orphan journal) and pa-daemon
//! (session leases, wire auth).
//!
//! `process_start_id` is the pid-reuse identity the TS product records as
//! `proc:<starttime>` (TS `getProcessStartId`, `core/session-lease.ts`): the
//! kernel-reported process start time from `/proc/<pid>/stat` field 22. A
//! recycled pid has a different start time, so a recorded identity that still
//! matches proves the pid still names the same process. `None` means the
//! platform exposes no identity - owners then trust liveness checks alone,
//! exactly like TS records with `processStartId: undefined`.

/// `/proc/<pid>/stat` field 22 (starttime), formatted `proc:<starttime>`.
/// `None` when the platform has no procfs identity.
#[cfg(unix)]
pub fn process_start_id(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let command_end = stat.rfind(')')?;
    let start_time = stat[command_end + 2..].split(' ').nth(19)?;
    (!start_time.is_empty()).then(|| format!("proc:{start_time}"))
}

/// Windows: the process creation time in 100ns ticks since 1601-01-01 UTC
/// (`GetProcessTimes`), formatted `win:<ticks>` - the same value the TS
/// product records via PowerShell `StartTime.ToUniversalTime().Ticks`
/// (TS `getWindowsProcessStartId`); a recycled pid has a different
/// creation time, so the identity check is exact.
#[cfg(windows)]
pub fn process_start_id(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let handle = winapi::open_process(winapi::PROCESS_QUERY_LIMITED_INFORMATION, pid)?;
    let ticks = winapi::process_creation_ticks(handle);
    winapi::close_handle(handle);
    ticks.map(|ticks| format!("win:{ticks}"))
}

#[cfg(not(any(unix, windows)))]
pub fn process_start_id(_pid: u32) -> Option<String> {
    // No identity available on this platform; owners trust liveness alone.
    None
}

/// True only for a process that is actually running: zombies do not count
/// (TS `isProcessAlive`). Errors when the platform cannot answer.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> anyhow::Result<bool> {
    if pid == 0 {
        return Ok(false);
    }
    if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
        return Ok(false);
    }
    // A zombie still owns /proc; treat it as dead for lease purposes.
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        if let Some(state) = status.lines().find_map(|l| l.strip_prefix("State:")) {
            return Ok(!state.trim_start().starts_with('Z'));
        }
    }
    Ok(true)
}

/// Windows: a handle-existence probe with the STILL_ACTIVE exit-code check
/// (TS `isProcessAlive` = `processIdExists` && !zombie; win32 has no zombie
/// state, and Node's `kill(pid, 0)` is the same exit-code probe). A pid the
/// caller may not query exists (TS counts EPERM as existing) and reads
/// alive: lease owners must not treat an access-denied probe as a dead
/// owner.
#[cfg(windows)]
pub fn is_process_alive(pid: u32) -> anyhow::Result<bool> {
    if pid == 0 {
        return Ok(false);
    }
    Ok(winapi::is_still_active(pid))
}

#[cfg(not(any(unix, windows)))]
pub fn is_process_alive(_pid: u32) -> anyhow::Result<bool> {
    anyhow::bail!("process liveness is not implemented on this platform")
}

/// The kernel32 surface the Windows identity/liveness queries need, as a
/// hand-declared extern wall (repo policy: pinned constants and externs,
/// no windows-sys dependency - same policy as the named-pipe transport).
#[cfg(windows)]
mod winapi {
    #![allow(non_snake_case)]

    use std::ffi::c_void;

    /// `winnt.h`: query the process without operating on it.
    pub(crate) const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    /// `winerror.h` `STILL_ACTIVE`: the exit code a running process reports.
    pub(crate) const STILL_ACTIVE: u32 = 259;
    /// `winerror.h` `ERROR_ACCESS_DENIED`: the pid exists but is not ours to
    /// query.
    const ERROR_ACCESS_DENIED: u32 = 5;

    /// A Win32 `FILETIME`: 100ns ticks since 1601-01-01 UTC, split 32/32.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FileTime {
        dwLowDateTime: u32,
        dwHighDateTime: u32,
    }

    impl FileTime {
        fn ticks(self) -> u64 {
            (self.dwHighDateTime as u64) << 32 | self.dwLowDateTime as u64
        }
    }

    type Handle = *mut c_void;

    extern "system" {
        fn OpenProcess(access: u32, inherit_handle: i32, process_id: usize) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
        fn GetExitCodeProcess(handle: Handle, exit_code: *mut u32) -> i32;
        fn GetProcessTimes(
            handle: Handle,
            creation_time: *mut FileTime,
            exit_time: *mut FileTime,
            kernel_time: *mut FileTime,
            user_time: *mut FileTime,
        ) -> i32;
        fn GetLastError() -> u32;
    }

    /// Open a query handle, `None` when the pid does not resolve.
    pub(crate) fn open_process(access: u32, pid: u32) -> Option<Handle> {
        let handle = unsafe { OpenProcess(access, 0, pid as usize) };
        (!handle.is_null()).then_some(handle)
    }

    pub(crate) fn close_handle(handle: Handle) {
        unsafe { CloseHandle(handle) };
    }

    /// The creation-time ticks of the process, `None` when the query fails.
    pub(crate) fn process_creation_ticks(handle: Handle) -> Option<u64> {
        let mut creation = FileTime::default();
        let mut exit = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        let ok =
            unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
        (ok != 0).then(|| creation.ticks())
    }

    /// True when the pid names a running process. A queryable pid is alive
    /// while its exit code is `STILL_ACTIVE`; an access-denied probe means
    /// the process exists but is not ours to inspect (the EPERM case TS
    /// counts as existing), and reads alive: a lease owner must never look
    /// dead just because the probe was denied.
    pub(crate) fn is_still_active(pid: u32) -> bool {
        let Some(handle) = open_process(PROCESS_QUERY_LIMITED_INFORMATION, pid) else {
            return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
        };
        let mut exit_code = 0;
        let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
        close_handle(handle);
        ok != 0 && exit_code == STILL_ACTIVE
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// Round-trip identity + liveness on this very process; the identity
    /// must be stable while the process runs (it is the creation time).
    #[test]
    fn self_process_identity_and_liveness() {
        let id = process_start_id(std::process::id());
        let Some(id) = id else {
            panic!("a live process must expose its creation-time identity");
        };
        assert!(id.starts_with("win:") && id[4..].chars().all(|c| c.is_ascii_digit()));
        assert_eq!(process_start_id(std::process::id()), Some(id));
        assert!(is_process_alive(std::process::id()).unwrap_or(false));
    }

    /// A pid that cannot exist is dead and carries no identity.
    #[test]
    fn invalid_pid_is_dead() {
        assert_eq!(process_start_id(0), None);
        assert_eq!(process_start_id(u32::MAX), None);
        assert!(!is_process_alive(0).unwrap_or(true));
        assert!(!is_process_alive(u32::MAX).unwrap_or(true));
    }
}
