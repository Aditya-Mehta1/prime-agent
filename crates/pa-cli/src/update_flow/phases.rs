//! The coordinator's daemon-facing phase bodies: the prepare poll, the
//! commit, the marker freshness gate, and the adoption-based restore
//! report. Split from the driver so the FSM stays readable; the driver owns
//! the state writes, these own the wire and filesystem facts.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use pa_types::daemon::update_flow::{
    prepared_marker_expiry, update_marker_path, PreparedMarkerExpiry, UpdateId, UpdateRoster,
    UpdateStatusCounts, UpdateStatusFailure, UpdateTimeoutBudget,
};

/// The prepare-poll and restore-poll interval.
const PHASE_POLL: Duration = Duration::from_millis(500);

/// Poll `prepare_update_restart` (idempotent on `update_id`) until the old
/// supervisor reports `prepared` (spec §4 `Preparing`), bounded by the
/// prepare budget. A typed refusal is the spec's `Join` case: another
/// update owns the daemon's transaction - the update aborts for a later
/// retry (this process holds the coordinator lock, so there is nothing to
/// join).
pub(super) async fn prepare_to_prepared(
    client: &pa_tui::daemon_client::DaemonClient,
    update_id: &UpdateId,
    budget: &UpdateTimeoutBudget,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(budget.prepare_ms.max(1));
    loop {
        let response = client
            .request_with_timeout(
                pa_types::daemon::DaemonCommand::PrepareUpdateRestart {
                    id: None,
                    update_id: Some(update_id.to_string()),
                    rest: Default::default(),
                },
                budget.prepare_rpc_ms.max(1),
            )
            .await?;
        if response.success {
            let state = response
                .data
                .as_ref()
                .and_then(|data| data.get("state"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if state == "prepared" {
                return Ok(());
            }
        } else if matches!(
            response.error_info,
            Some(pa_types::daemon::DaemonErrorInfo::UpdatePrepareRefused { .. })
        ) {
            anyhow::bail!(
                "another update is preparing on the daemon ({}); retry later",
                response.error.unwrap_or_default()
            );
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("the old supervisor did not reach prepared within its budget");
        }
        tokio::time::sleep(PHASE_POLL).await;
    }
}

/// Consume the prepared artifact: `commit_update_restart` (spec §5
/// `Prepared -> Stopping`, the slice-3 dispatch). The RPC budget spans the
/// graceful stops (a stop that exceeds its budget refuses and the
/// supervisor abandons - either way this call returns).
pub(super) async fn commit_update(
    client: &pa_tui::daemon_client::DaemonClient,
    update_id: &UpdateId,
    budget: &UpdateTimeoutBudget,
) -> Result<()> {
    let response = client
        .request_with_timeout(
            pa_types::daemon::DaemonCommand::CommitUpdateRestart {
                id: None,
                update_id: Some(update_id.to_string()),
                rest: Default::default(),
            },
            budget.prepare_rpc_ms + budget.worker_stop_ms + budget.worker_stop_extension_ms,
        )
        .await?;
    if !response.success {
        anyhow::bail!(
            "commit_update_restart was refused: {}",
            response
                .error
                .unwrap_or_else(|| "unknown error".to_string())
        );
    }
    Ok(())
}

/// An expired marker is a refusal, never a restore of stale snapshots (spec
/// §7).
pub(super) fn check_marker_fresh(prepared_dir: &Path) -> Result<()> {
    let marker_path = update_marker_path(prepared_dir);
    let content = std::fs::read_to_string(&marker_path)
        .with_context(|| format!("read the prepared marker at {}", marker_path.display()))?;
    let marker: pa_types::daemon::update_flow::UpdatePreparedMarker =
        serde_json::from_str(&content)?;
    match prepared_marker_expiry(&marker.expires_at, &crate::util_time::now_iso8601()) {
        PreparedMarkerExpiry::Active => Ok(()),
        PreparedMarkerExpiry::Expired => anyhow::bail!(
            "the prepared marker expired at {}; the update is abandoned",
            marker.expires_at
        ),
        PreparedMarkerExpiry::Malformed => anyhow::bail!(
            "the prepared marker at {} is malformed",
            marker_path.display()
        ),
    }
}

pub(super) fn read_roster(path: &Path) -> Result<UpdateRoster> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read the roster at {}", path.display()))?;
    Ok(serde_json::from_str(&content)?)
}

/// The adoption-based restore report (slice 4): poll the successor's
/// session list until every roster session is adopted or the overall
/// restore budget expires. Missing sessions are recorded as failures -
/// restore never fails the boot (spec §9 `Restoring`).
pub(super) async fn adoption_report(
    roster: Option<&UpdateRoster>,
    socket_path: &Path,
    budget: &UpdateTimeoutBudget,
) -> (UpdateStatusCounts, Vec<UpdateStatusFailure>) {
    let Some(roster) = roster else {
        return (UpdateStatusCounts::default(), Vec::new());
    };
    if roster.sessions.is_empty() {
        return (UpdateStatusCounts::default(), Vec::new());
    }
    let deadline =
        tokio::time::Instant::now() + Duration::from_millis(budget.restore_overall_ms.max(1));
    let mut adopted: Vec<bool> = vec![false; roster.sessions.len()];
    loop {
        if let Ok((client, _events)) =
            pa_tui::daemon_client::DaemonClient::connect(socket_path).await
        {
            if let Ok(list) = client
                .request_ok(pa_types::daemon::DaemonCommand::List {
                    id: None,
                    all: None,
                    cwd: None,
                    session_dir: None,
                    include_client_owned: None,
                    rest: Default::default(),
                })
                .await
            {
                let ids = list
                    .get("sessions")
                    .and_then(serde_json::Value::as_array)
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|row| {
                                row.get("sessionId").and_then(serde_json::Value::as_str)
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                for (index, session) in roster.sessions.iter().enumerate() {
                    adopted[index] = ids.iter().any(|id| *id == session.session_id);
                }
            }
            client.close();
        }
        if adopted.iter().all(|adopted| *adopted) || tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(PHASE_POLL).await;
    }
    let mut counts = UpdateStatusCounts::default();
    let mut failures = Vec::new();
    for (index, session) in roster.sessions.iter().enumerate() {
        counts.total += 1;
        if adopted[index] {
            counts.restored += 1;
            if session.should_resume {
                counts.resumed += 1;
            }
        } else {
            counts.failed += 1;
            failures.push(UpdateStatusFailure {
                session_file: session.session_file.clone(),
                message: "the successor supervisor did not adopt the session".to_string(),
            });
        }
    }
    (counts, failures)
}
