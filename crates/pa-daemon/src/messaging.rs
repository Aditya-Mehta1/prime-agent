//! Supervisor `send_message` arm: agent-to-agent message routing.
//!
//! Port of the `send_message` block in `modes/daemon/daemon-supervisor.ts`:
//! resolve the source and target workers, refuse self-targeting, then route
//! `worker_deliver_message` to the target with sender info from the source
//! session (agent origin) or the sending client (CLI origin). The TS
//! supervisor can also wake a saved session from its catalog when the
//! target is not resident; that wake-up path is deferred (see
//! docs/parity-checklist.md), so an unknown target answers with the TS
//! unknown-session error. The TS family-reach assertion needs the session
//! family catalog, which the thin supervisor does not keep yet; it is
//! deferred with it.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use pa_types::daemon::{DaemonCommand, DaemonWorkerCommand};
use serde_json::{json, Value};

use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::registry::ResidentWorker;
use crate::supervisor::Supervisor;

/// Worker round-trip budget for the sender-summary read and the delivery
/// route (TS `WORKER_REQUEST_TIMEOUT_MS`).
const WORKER_REQUEST_TIMEOUT_MS: u64 = 30_000;

impl Supervisor {
    /// `send_message`: route to the target worker as `worker_deliver_message`.
    pub(crate) async fn handle_send_message(
        self: &Arc<Self>,
        command_id: &str,
        client_id: &str,
        command: &DaemonCommand,
    ) -> DaemonResponse {
        let DaemonCommand::SendMessage {
            target_active_session_id,
            message,
            from_active_session_id,
            delivery_mode,
            ..
        } = command
        else {
            return response_failure(Some(command_id), "send_message", "invalid command", None);
        };
        let fail = |error: String| response_failure(Some(command_id), "send_message", &error, None);
        // Source first, like the TS supervisor: an unknown source answers
        // with the same unknown-session error as an unknown target.
        let source = match from_active_session_id {
            Some(source) => match self.registry.resolve(source).await {
                Ok(resident) => Some(resident),
                Err(_) => return fail(format!("Unknown active session: {source}")),
            },
            None => None,
        };
        let Ok(target) = self.registry.resolve(target_active_session_id).await else {
            return fail(format!(
                "Unknown active session: {target_active_session_id}"
            ));
        };
        if source
            .as_ref()
            .is_some_and(|source| Arc::ptr_eq(source, &target))
        {
            return fail("Agent messaging cannot target the sending session".to_string());
        }
        let sender = match &source {
            Some(source) => match self.sender_endpoint(source, client_id).await {
                Ok(sender) => sender,
                Err(error) => return fail(format!("{error:#}")),
            },
            // CLI origin: the TS worker attributes client-sent messages to
            // the client id (`createAgentSessionMessageSender`).
            None => json!({ "clientId": client_id }),
        };
        let delivery = DaemonWorkerCommand::WorkerDeliverMessage {
            id: None,
            target_active_session_id: target_active_session_id.clone(),
            message: message.clone(),
            sender,
            delivery_mode: delivery_mode.clone(),
            rest: Default::default(),
        };
        let payload = match serde_json::to_value(&delivery) {
            Ok(payload) => payload,
            Err(error) => return fail(format!("invalid delivery command: {error}")),
        };
        let response = self
            .route_command(
                &target,
                "worker_deliver_message",
                payload,
                WORKER_REQUEST_TIMEOUT_MS,
            )
            .await;
        match response {
            Ok(response) if response.success => {
                response_success(Some(command_id), "send_message", response.data)
            }
            Ok(response) => fail(
                response
                    .error
                    .unwrap_or_else(|| "delivery failed".to_string()),
            ),
            Err(error) => fail(format!("{error:#}")),
        }
    }

    /// Sender endpoint for an agent-origin message: the source session's
    /// live summary (the TS supervisor reads the same fields from its
    /// roster entry).
    async fn sender_endpoint(
        self: &Arc<Self>,
        source: &Arc<ResidentWorker>,
        client_id: &str,
    ) -> Result<Value> {
        let state = self
            .route_command(source, "get_state", json!({}), WORKER_REQUEST_TIMEOUT_MS)
            .await?;
        if !state.success {
            return Err(anyhow!(
                "{}",
                state
                    .error
                    .unwrap_or_else(|| "source state unavailable".to_string())
            ));
        }
        let summary = state
            .data
            .ok_or_else(|| anyhow!("source session state unavailable"))?;
        let mut sender = json!({
            "activeSessionId": summary
                .get("activeSessionId")
                .or_else(|| summary.get("id"))
                .cloned()
                .unwrap_or(Value::Null),
            "sessionId": summary.get("sessionId").cloned().unwrap_or(Value::Null),
            "runtimeKind": summary
                .get("runtimeKind")
                .cloned()
                .unwrap_or(json!("top-level")),
            "clientId": client_id,
        });
        if let Some(name) = summary.get("sessionName").and_then(Value::as_str) {
            if !name.is_empty() {
                sender["sessionName"] = json!(name);
            }
        }
        Ok(sender)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ResidentWorker;
    use crate::supervisor::SupervisorOptions;
    use pa_types::daemon::{
        DaemonWorkerDescriptor, DaemonWorkerLifecycle, DurableDaemonCreateCommand,
    };

    fn resident(worker_id: &str) -> Arc<ResidentWorker> {
        ResidentWorker::new(
            worker_id.to_string(),
            DaemonWorkerDescriptor {
                version: 2,
                worker_id: worker_id.to_string(),
                pid: 1,
                process_start_id: None,
                socket_path: "/w.sock".to_string(),
                recovery_journal_path: "/w.jsonl".to_string(),
                orphan_process_journal_path: None,
                supervisor_socket_path: "/s.sock".to_string(),
                authentication_token: "t".to_string(),
                worker_instance_id: None,
                root_active_session_id: worker_id.to_string(),
                owner_client_id: None,
                root_session_id: None,
                session_file: Some("/sessions/some-session.jsonl".to_string()),
                session_dir: None,
                telemetry_disabled: None,
                created_at: "t".to_string(),
                updated_at: "t".to_string(),
                lifecycle: DaemonWorkerLifecycle::Ready,
                create_command: DurableDaemonCreateCommand {
                    session_path: None,
                    no_session: None,
                    rest: Default::default(),
                },
                consecutive_failures: 0,
                stop_requested_at: None,
                archive_on_stop: None,
                last_failure_at: None,
                last_error: None,
                rest: Default::default(),
            },
            std::path::PathBuf::from("/d.json"),
        )
    }

    fn supervisor() -> Arc<Supervisor> {
        let dir = tempfile::TempDir::new().unwrap();
        Arc::new(
            Supervisor::new(SupervisorOptions {
                socket_path: dir.path().join("s.sock"),
                agent_dir: dir.path().join("agent"),
            })
            .unwrap(),
        )
    }

    fn send_command(target: &str, from: Option<&str>) -> DaemonCommand {
        DaemonCommand::SendMessage {
            id: Some("m1".to_string()),
            target_active_session_id: target.to_string(),
            message: "hello".to_string(),
            from_active_session_id: from.map(str::to_string),
            agent_origin: None,
            delivery_mode: None,
            rest: Default::default(),
        }
    }

    /// An unknown target answers with the TS unknown-session error, with
    /// the request id and command echoed on the failure response.
    #[tokio::test]
    async fn unknown_target_answers_with_the_ts_error() {
        let supervisor = supervisor();
        let response = supervisor
            .handle_send_message("m1", "cli-1", &send_command("no-such-session", None))
            .await;
        assert!(!response.success, "{response:?}");
        assert_eq!(response.id.as_deref(), Some("m1"));
        assert_eq!(response.command, "send_message");
        assert_eq!(
            response.error.as_deref(),
            Some("Unknown active session: no-such-session")
        );
    }

    /// The source resolves before the target, so an unknown source fails
    /// even when the target is resident.
    #[tokio::test]
    async fn unknown_source_fails_like_the_ts_source_lookup() {
        let supervisor = supervisor();
        supervisor.registry.insert(resident("target-1")).await;
        let response = supervisor
            .handle_send_message("m1", "cli-1", &send_command("target-1", Some("ghost")))
            .await;
        assert!(!response.success, "{response:?}");
        assert_eq!(
            response.error.as_deref(),
            Some("Unknown active session: ghost")
        );
    }

    /// A session cannot message itself (TS self-target guard).
    #[tokio::test]
    async fn self_target_is_refused() {
        let supervisor = supervisor();
        supervisor.registry.insert(resident("solo-1")).await;
        let response = supervisor
            .handle_send_message("m1", "cli-1", &send_command("solo-1", Some("solo-1")))
            .await;
        assert!(!response.success, "{response:?}");
        assert_eq!(
            response.error.as_deref(),
            Some("Agent messaging cannot target the sending session")
        );
    }

    /// Delivery routes `worker_deliver_message` to the resolved target with
    /// a CLI-origin sender; the resident has no live worker connection, so
    /// the route fails with the not-connected error (the arm reached the
    /// routing stage with the right command).
    #[tokio::test]
    async fn delivery_routes_worker_deliver_message_to_the_target() {
        let supervisor = supervisor();
        supervisor.registry.insert(resident("target-1")).await;
        let response = supervisor
            .handle_send_message("m1", "cli-1", &send_command("target-1", None))
            .await;
        assert!(!response.success, "{response:?}");
        assert_eq!(
            response.error.as_deref(),
            Some("Session worker is not connected")
        );
    }
}
