//! The `prime-agent update` driver: the invoking CLI's phases (spec §4
//! `Acquire`..`Staged`) plus the spawn of the detached coordinator and the
//! status tail. The terminal status drives the printed report and the exit
//! code.

use std::path::PathBuf;

use anyhow::{Context, Result};
use pa_types::daemon::update_flow::{UpdateState, UpdateStatus, UpdateTimeoutBudget};

use super::intent::{acquire, hand_over, release, status_path_for, AcquireOutcome};
use super::plan::{plan, UpdatePlan};
use super::report::UpdateReport;
use super::status::UPDATE_TELEMETRY_STATE_NAMES;
use super::status::{read_status, StatusWriter};

/// The tail loop budgets (TS `launchDaemonUpdateRestartCoordinator`:
/// 30 minutes of progress, 3 minutes of heartbeat liveness).
const TAIL_PROGRESS_TIMEOUT_MS: u64 = 30 * 60_000;
const TAIL_LIVENESS_TIMEOUT_MS: u64 = 180_000;
const TAIL_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// The update command's fixed inputs (parsed by the public command layer).
pub struct UpdateCommandOptions {
    pub force: bool,
    pub rollback: bool,
    pub channel: Option<pa_core::update::version::UpdateChannel>,
}

/// `prime-agent update`: plan, stage, spawn the coordinator, and relay its
/// terminal status. Returns the process exit code.
pub async fn run_update_command(options: &UpdateCommandOptions) -> Result<i32> {
    let agent_dir = crate::config::get_agent_dir();
    let socket_path = pa_daemon::socket::default_daemon_socket_path();
    let install_root = super::activation_root().map_err(|_| {
        anyhow::anyhow!(
            "This compiled application is not owned by the Prime Agent installer. Update it using its original installer."
        )
    })?;
    let update_id = pa_types::daemon::update_flow::UpdateId::from(uuid::Uuid::now_v7().to_string());
    let status_path = status_path_for(&agent_dir, &socket_path.to_string_lossy());

    match acquire(
        &agent_dir,
        &socket_path.to_string_lossy(),
        &update_id,
        &status_path,
    )? {
        AcquireOutcome::Join { status_path } => {
            // Relay the running update's terminal status (TS
            // `waitForActiveDaemonUpdateRestartCoordinator`). The running
            // coordinator's invoker owns the telemetry emission.
            let status = tail_status(&status_path).await;
            print_terminal(&status);
            return Ok(if status.state == UpdateState::Complete {
                0
            } else {
                1
            });
        }
        AcquireOutcome::Acquired => {}
    }
    let mut writer = StatusWriter::new(&status_path, &update_id, &socket_path.to_string_lossy());
    writer.save()?;
    writer.set_state(UpdateState::Planning)?;
    let budget = UpdateTimeoutBudget::from_env();
    let download_base = std::env::var("PRIME_AGENT_DOWNLOAD_BASE_URL").ok();
    let planned = plan(
        &install_root,
        options.force,
        options.rollback,
        options.channel,
        download_base.as_deref(),
    )
    .await;
    let (coordinator_exe, candidate_dir) = match planned {
        Ok(UpdatePlan::Update {
            version,
            archive_url,
            archive_sha256,
            base_url,
        }) => {
            // `Downloading`: stream + digest the archive (one wall-clock
            // budget across all attempts, spec §9).
            writer.set_state(UpdateState::Downloading)?;
            let archive = agent_dir.join(format!("update-{update_id}.tar.gz"));
            pa_core::update::download::download_archive(
                &archive_url,
                &archive_sha256,
                &archive,
                pa_core::update::download::DownloadBudget {
                    total_ms: budget.download_ms,
                    attempts: budget.download_attempts,
                },
                &pa_core::update::release::update_user_agent(version.as_str()),
            )
            .await
            .with_context(|| "the release download failed; the installed version was kept")?;
            // `Staged`: extract, validate, and probe the candidate.
            writer.set_state(UpdateState::Staged)?;
            let release_dir = pa_core::update::download::stage_archive(
                &archive,
                &archive_sha256,
                &install_root,
                &version,
                base_url.as_str(),
            )?;
            let _ = std::fs::remove_file(&archive);
            super::swap::validate_candidate(&release_dir.join("prime-agent"), &version).await?;
            (release_dir.join("prime-agent"), release_dir)
        }
        Ok(UpdatePlan::Rollback {
            coordinator_exe,
            candidate_dir,
            ..
        }) => {
            // A rollback stages nothing; the previous release is already
            // validated on disk. The coordinator IS the previous binary.
            writer.set_state(UpdateState::Staged)?;
            (coordinator_exe, candidate_dir)
        }
        Ok(UpdatePlan::Skipped { reason }) => {
            writer.set_state(UpdateState::Skipped)?;
            writer.set_message(Some(reason.clone()))?;
            release(&agent_dir, &socket_path.to_string_lossy())?;
            track_update_completed(writer.current()).await;
            println!("{reason}");
            return Ok(0);
        }
        Err(error) => {
            // A planning failure aborts before any state that could wedge:
            // the daemon never stopped (spec §4 `Aborted`).
            writer.set_state(UpdateState::Aborted)?;
            writer.set_message(Some(error.to_string()))?;
            release(&agent_dir, &socket_path.to_string_lossy())?;
            track_update_completed(writer.current()).await;
            anyhow::bail!("{error:#}");
        }
    };

    // Spawn the detached coordinator (the new - or previous - binary) and
    // hand the lock over (spec §3: the user-facing process exits after
    // spawning it; only the status file couples them).
    let mut command = std::process::Command::new(&coordinator_exe);
    command
        .args([
            "update",
            crate::public_command::DAEMON_UPDATE_RESTART_COORDINATOR_FLAG,
            "--daemon-socket",
        ])
        .arg(&socket_path)
        .arg(crate::public_command::DAEMON_UPDATE_RESTART_STATUS_FLAG)
        .arg(&status_path)
        .env(super::coordinator::UPDATE_CANDIDATE_DIR_ENV, &candidate_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // The coordinator inherits this CLI's environment minus the worker
        // role markers (the TS launcher deletes the same set).
        .env_remove(pa_daemon::worker::WORKER_ROLE_ENV)
        .env_remove(pa_daemon::worker::WORKER_TOKEN_ENV)
        .env_remove(pa_daemon::worker::WORKER_ACTIVE_SESSION_ID_ENV)
        .env_remove(pa_daemon::worker::WORKER_RECOVERY_JOURNAL_ENV)
        .env_remove(pa_daemon::worker::WORKER_SUPERVISOR_SOCKET_ENV)
        .env_remove(pa_daemon::worker::WORKER_SOCKET_ENV)
        .env_remove(pa_daemon::worker::WORKER_INSTANCE_ID_ENV)
        .env_remove(pa_daemon::worker::WORKER_SCRIPT_ENV);
    // Detached: own process group, reaped by init, survives this CLI.
    pa_core::platform::process::set_new_process_group(&mut command);
    let child = command.spawn().with_context(|| {
        format!(
            "spawn the update coordinator at {}",
            coordinator_exe.display()
        )
    })?;
    hand_over(
        &agent_dir,
        &socket_path.to_string_lossy(),
        &update_id,
        child.id() as u64,
        &status_path,
    )?;
    drop(child);

    let status = tail_status(&status_path).await;
    track_update_completed(&status).await;
    print_terminal(&status);
    Ok(if status.state == UpdateState::Complete {
        0
    } else {
        1
    })
}

/// Tail the coordinator's status file to a terminal state (TS
/// `launchDaemonUpdateRestartCoordinator`'s wait loop): progress, liveness,
/// and the holder's process lifetime all bound the wait.
async fn tail_status(status_path: &std::path::Path) -> UpdateStatus {
    let started = std::time::Instant::now();
    let mut last_liveness = std::time::Instant::now();
    let mut last_epoch: Option<u64> = None;
    loop {
        if let Some(status) = read_status(status_path) {
            if Some(status.epoch) != last_epoch {
                last_epoch = Some(status.epoch);
                last_liveness = std::time::Instant::now();
            }
            if status.state.is_terminal() {
                return status;
            }
        }
        if last_liveness.elapsed().as_millis() as u64 >= TAIL_LIVENESS_TIMEOUT_MS {
            return unreported(
                status_path,
                "the update coordinator stopped reporting liveness",
            );
        }
        if started.elapsed().as_millis() as u64 >= TAIL_PROGRESS_TIMEOUT_MS {
            return unreported(status_path, "timed out waiting for update progress");
        }
        tokio::time::sleep(TAIL_POLL).await;
    }
}

fn unreported(status_path: &std::path::Path, message: &str) -> UpdateStatus {
    read_status(status_path).unwrap_or_else(|| UpdateStatus {
        version: 1,
        update_id: pa_types::daemon::update_flow::UpdateId::from(String::new()),
        socket_path: String::new(),
        state: UpdateState::Failed,
        epoch: 0,
        coordinator: None,
        predecessor: None,
        successor: None,
        counts: Default::default(),
        failures: Vec::new(),
        message: Some(message.to_string()),
        started_at: String::new(),
        updated_at: String::new(),
        heartbeat_at: None,
        rest: Default::default(),
    })
}

fn print_terminal(status: &UpdateStatus) {
    UpdateReport::build(status).print();
    if let Some(message) = &status.message {
        match status.state {
            UpdateState::Complete | UpdateState::Skipped => println!("{message}"),
            _ => eprintln!("{message}"),
        }
    }
}

/// `update completed`: the update flow's adoption event, one per invocation
/// at the terminal status (primitives only; the privacy contract keeps
/// paths, messages, and ids out of telemetry).
async fn track_update_completed(status: &UpdateStatus) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let agent_dir = crate::config::get_agent_dir();
    let settings = pa_core::settings::SettingsManager::create(&cwd, &agent_dir);
    if crate::mode::telemetry_disabled(&settings) {
        return;
    }
    let client = pa_core::session_engine::telemetry::build_client(&settings, &agent_dir);
    let mut properties = pa_telemetry::base_properties("cli");
    properties.set(
        "outcome",
        serde_json::Value::from(
            UPDATE_TELEMETRY_STATE_NAMES
                .iter()
                .find(|(state, _)| *state == status.state)
                .map(|(_, name)| *name)
                .unwrap_or("unknown"),
        ),
    );
    properties.set(
        "sessions_total",
        serde_json::Value::from(status.counts.total),
    );
    properties.set(
        "sessions_restored",
        serde_json::Value::from(status.counts.restored),
    );
    properties.set(
        "sessions_failed",
        serde_json::Value::from(status.counts.failed),
    );
    client.track("update completed", properties);
    let _ = client.shutdown().await;
}

/// The coordinator mode entry (`update --internal-update-restart-coordinator
/// --daemon-socket <path> --internal-update-restart-status <path>`): the
/// detached process that adopts the staged status and drives the FSM to a
/// terminal state. Returns the process exit code.
pub async fn run_coordinator_mode(socket_path: PathBuf, status_path: PathBuf) -> Result<i32> {
    let agent_dir = crate::config::get_agent_dir();
    // TS parity: the status file belongs under the agent dir's
    // `update-restarts/` - the coordinator never writes status elsewhere.
    let restarts_dir = pa_types::daemon::update_flow::update_restarts_dir(&agent_dir);
    if !status_path.starts_with(&restarts_dir) {
        eprintln!("Invalid daemon update restart coordinator invocation.");
        return Ok(1);
    }
    let options = super::coordinator::CoordinatorOptions {
        agent_dir,
        socket_path,
        status_path,
        budget: UpdateTimeoutBudget::from_env(),
    };
    let status = super::coordinator::run(&options).await?;
    print_terminal(&status);
    Ok(if status.state == UpdateState::Complete {
        0
    } else {
        1
    })
}
