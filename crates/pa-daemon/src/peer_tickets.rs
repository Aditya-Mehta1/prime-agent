//! Supervisor-issued direct-transport tickets (TS `daemon-supervisor.ts`
//! `get_direct_worker_transport` / `issuePeerTransport`).
//!
//! A client that wants to attach to a session asks the supervisor for a
//! ticket: the worker's socket path and filesystem identity, plus a
//! single-use grant (10s TTL). The grant is pushed into the worker's memory
//! (`worker_register_peer_transport`) before the ticket is returned, so by
//! the time the client presents it the worker can validate and burn it. The
//! supervisor is then out of the streaming path: the client attaches to the
//! session socket directly.
//!
//! TS refuses tickets for client-owned workers (`ownerClientId`); the Rust
//! supervisor has no client-owned worker lifecycle (every worker it spawns or
//! adopts is a resident session), so there is no such class to refuse.

use std::sync::atomic::Ordering;

use anyhow::{anyhow, Result};
use pa_types::daemon::{
    DaemonPeerTransportTicket, DaemonWorkerCommand, DaemonWorkerLifecycle, DaemonWorkerPeerGrant,
};

use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::registry::ResidentWorker;
use crate::supervisor::Supervisor;
use crate::util;

/// TS `PEER_TRANSPORT_GRANT_TTL_MS`: how long a minted grant stays valid.
pub(crate) const PEER_TRANSPORT_GRANT_TTL_MS: u64 = 10_000;
/// TS `issuePeerTransport` worker round-trip budget for the grant push.
const GRANT_REGISTRATION_TIMEOUT_MS: u64 = 3_000;

impl Supervisor {
    /// `get_direct_worker_transport`: resolve the session, mint a grant,
    /// register it with the worker, and return the ticket.
    pub(crate) async fn handle_get_direct_worker_transport(
        self: &std::sync::Arc<Self>,
        command_id: &str,
        type_name: &str,
        selector: &str,
    ) -> DaemonResponse {
        match self.issue_direct_transport(selector).await {
            Ok(ticket) => response_success(
                Some(command_id),
                type_name,
                serde_json::to_value(&ticket).ok(),
            ),
            Err(error) => response_failure(Some(command_id), type_name, &error.to_string(), None),
        }
    }

    /// Port of `issuePeerTransport`. Errors carry the TS worker-state
    /// strings so clients see the same diagnosis.
    async fn issue_direct_transport(
        self: &std::sync::Arc<Self>,
        selector: &str,
    ) -> Result<DaemonPeerTransportTicket> {
        let resident = self.registry.resolve(selector).await?;
        if self.is_stopping(&resident) {
            return Err(anyhow!("Supervisor is shutting down"));
        }
        if !resident.peer_transport_capable.load(Ordering::SeqCst) {
            return Err(anyhow!(
                "Session worker does not support direct peer transport"
            ));
        }
        self.require_available_worker_client(&resident).await?;
        let (worker_instance_id, socket_path, pid) = {
            let descriptor = resident.descriptor.lock().await;
            let worker_instance_id = descriptor
                .worker_instance_id
                .clone()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    anyhow!("Direct transport requires an exact worker process identity")
                })?;
            (
                worker_instance_id,
                descriptor.socket_path.clone(),
                descriptor.pid,
            )
        };
        // The TS supervisor additionally pins the worker's process-start
        // id; this supervisor never populated it, so the live-pid check is
        // the process-identity guarantee here.
        if !matches!(crate::lease::is_process_alive(pid as u32), Ok(true)) {
            return Err(anyhow!(
                "Direct transport worker process identity is not current"
            ));
        }
        let socket_identity = pa_types::platform::socket_identity(std::path::Path::new(
            &socket_path,
        ))
        .ok_or_else(|| anyhow!("Direct transport requires an exact worker socket identity"))?;
        let grant = mint_grant(&worker_instance_id, &resident.worker_id);
        let registration = DaemonWorkerCommand::WorkerRegisterPeerTransport {
            id: None,
            grant: grant.clone(),
            rest: Default::default(),
        };
        let payload = serde_json::to_value(&registration)?;
        let response = self
            .route_command(
                &resident,
                "worker_register_peer_transport",
                payload,
                GRANT_REGISTRATION_TIMEOUT_MS,
            )
            .await?;
        if !response.success {
            return Err(anyhow!(
                "{}",
                response
                    .error
                    .unwrap_or_else(|| "Peer transport grant is invalid".to_string())
            ));
        }
        Ok(DaemonPeerTransportTicket {
            purpose: grant.purpose.clone(),
            socket_path,
            socket_identity,
            worker_instance_id: grant.worker_instance_id.clone(),
            active_session_id: grant.active_session_id.clone(),
            grant_id: grant.grant_id.clone(),
            token: grant.token.clone(),
            expires_at: grant.expires_at.clone(),
        })
    }

    /// TS `requireAvailableWorkerClient`: the worker must be connected and
    /// ready, and not stopping.
    async fn require_available_worker_client(
        &self,
        resident: &std::sync::Arc<ResidentWorker>,
    ) -> Result<()> {
        let connected = resident.cmd_tx.lock().await.is_some();
        let lifecycle = resident.descriptor.lock().await.lifecycle;
        let available =
            connected && lifecycle == DaemonWorkerLifecycle::Ready && !self.is_stopping(resident);
        if !available {
            return Err(anyhow!(
                "Session worker is {}",
                effective_worker_state(connected, &lifecycle, self.is_stopping(resident))
            ));
        }
        Ok(())
    }
}

/// TS `effectiveWorkerState`.
fn effective_worker_state(
    connected: bool,
    lifecycle: &DaemonWorkerLifecycle,
    stopping: bool,
) -> &'static str {
    if stopping {
        "stopping"
    } else if lifecycle == &DaemonWorkerLifecycle::Failed {
        "failed"
    } else if lifecycle == &DaemonWorkerLifecycle::Ready && !connected {
        "recovering"
    } else {
        match lifecycle {
            DaemonWorkerLifecycle::Starting => "starting",
            DaemonWorkerLifecycle::Ready => "ready",
            DaemonWorkerLifecycle::Recovering => "recovering",
            DaemonWorkerLifecycle::Stopping => "stopping",
            DaemonWorkerLifecycle::Failed => "failed",
        }
    }
}

/// Mint one single-use grant for a worker instance and session, valid for
/// [`PEER_TRANSPORT_GRANT_TTL_MS`].
fn mint_grant(worker_instance_id: &str, active_session_id: &str) -> DaemonWorkerPeerGrant {
    DaemonWorkerPeerGrant {
        grant_id: uuid::Uuid::new_v4().to_string(),
        token: uuid::Uuid::new_v4().simple().to_string(),
        expires_at: util::iso_from_unix_ms(util::now_ms() + PEER_TRANSPORT_GRANT_TTL_MS),
        purpose: "session_client".to_string(),
        worker_instance_id: worker_instance_id.to_string(),
        active_session_id: active_session_id.to_string(),
        issuer_generation: format!("sup:{}", std::process::id()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_grants_expire_in_ten_seconds_and_are_unique() {
        let first = mint_grant("inst-1", "abc123");
        let second = mint_grant("inst-1", "abc123");
        assert_ne!(first.grant_id, second.grant_id);
        assert_ne!(first.token, second.token);
        assert_eq!(first.purpose, "session_client");
        assert_eq!(first.worker_instance_id, "inst-1");
        assert_eq!(first.active_session_id, "abc123");
        let expires = crate::util::iso_to_unix_ms(&first.expires_at).expect("iso expiry");
        let now = crate::util::now_ms();
        assert!(
            expires > now && expires <= now + PEER_TRANSPORT_GRANT_TTL_MS,
            "expiry inside the 10s TTL window: {expires} vs {now}"
        );
    }

    #[test]
    fn worker_states_match_ts_names() {
        use DaemonWorkerLifecycle as L;
        assert_eq!(effective_worker_state(true, &L::Ready, true), "stopping");
        assert_eq!(effective_worker_state(true, &L::Failed, false), "failed");
        assert_eq!(
            effective_worker_state(false, &L::Ready, false),
            "recovering"
        );
        assert_eq!(effective_worker_state(true, &L::Ready, false), "ready");
        assert_eq!(
            effective_worker_state(true, &L::Starting, false),
            "starting"
        );
        assert_eq!(
            effective_worker_state(false, &L::Recovering, false),
            "recovering"
        );
    }
}
