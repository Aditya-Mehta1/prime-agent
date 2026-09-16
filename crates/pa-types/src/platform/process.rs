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

#[cfg(not(unix))]
pub fn process_start_id(_pid: u32) -> Option<String> {
    // Windows: native creation-time query (OpenProcess + GetProcessTimes) is
    // the planned implementation; see docs/windows-readiness.md.
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

#[cfg(not(unix))]
pub fn is_process_alive(_pid: u32) -> anyhow::Result<bool> {
    anyhow::bail!("Windows process liveness is not yet implemented")
}
