//! Process control: signals, process groups, detached spawns.
//!
//! Unix implementations today (libc `kill`, `process_group(0)`); the Windows
//! lane replaces them with taskkill/Job-object semantics behind the same
//! signatures. Signatures that report outcomes return `bool` where callers
//! treat "unproven" conservatively (a kill that could not be proven reports
//! false, matching the TS `killOrphanProcess` contract).

use std::process::Command;

/// Termination signal for [`kill_pid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Graceful stop (SIGTERM).
    Term,
    /// Forcible stop (SIGKILL).
    Kill,
}

/// Put the spawned child into its own process group so later group-scoped
/// kills reach all of its descendants (TS: `detached: true` on POSIX).
#[cfg(unix)]
pub fn set_new_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
pub fn set_new_process_group(_command: &mut Command) {
    // Windows: children join a Job object instead; tracked in
    // docs/windows-readiness.md.
}

/// Signal a single pid. Returns true only when the signal was delivered,
/// proving the pid was alive at signal time.
#[cfg(unix)]
pub fn kill_pid(pid: i32, signal: Signal) -> bool {
    if pid <= 0 {
        return false;
    }
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    unsafe { libc::kill(pid, sig) == 0 }
}

#[cfg(not(unix))]
pub fn kill_pid(_pid: i32, _signal: Signal) -> bool {
    // Windows: TerminateProcess belongs in the Windows implementation;
    // false reports the kill as unproven, the conservative answer.
    false
}

/// Kill a process and all its children: the process group first (bash()
/// children run detached in a new group), then the bare pid as fallback.
/// Returns true when either signal was delivered (TS `killProcessTree`).
#[cfg(unix)]
pub fn kill_process_group_or_pid(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe {
        if libc::kill(-pid, libc::SIGKILL) == 0 {
            return true;
        }
    }
    unsafe { libc::kill(pid, libc::SIGKILL) == 0 }
}

#[cfg(not(unix))]
pub fn kill_process_group_or_pid(_pid: i32) -> bool {
    // Windows: `taskkill /F /T /PID` (absolute System32 path) is the TS
    // implementation; false reports the kill as unproven.
    false
}

/// Cheap `kill(pid, 0)` existence probe; counts zombies as existing.
#[cfg(unix)]
pub fn pid_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
pub fn pid_exists(_pid: u32) -> bool {
    // Windows: process-handle query is the planned implementation.
    false
}

/// The signal number that terminated a child, when it was signaled
/// (`ExitStatus::signal` on Unix; None elsewhere).
#[cfg(unix)]
pub fn termination_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
pub fn termination_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}
