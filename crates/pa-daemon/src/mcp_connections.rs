//! The worker arm behind the `get_mcp_connections` command: the roster
//! the interactive client's `/mcp` connections view renders — every
//! configured connection (built-in catalog plus user-declared servers)
//! with its connected state, plus the per-server tool listing the session
//! kernel reports for the generic servers (the runtime `mcp_status`
//! request, bounded per server).

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::worker::Worker;

/// Per-server budget for the kernel tool listing: a server that cannot
/// finish its handshake inside it reports its error entry instead of
/// stalling the view (the listing opens each not-yet-connected server).
pub(crate) const MCP_TOOL_LISTING_TIMEOUT_MS: u64 = 8_000;

/// How long the listing waits for the built session: the create-time
/// build races the first demand seam (the kernel prewarm starts at
/// create, the session slot fills when the build commits), and a running
/// turn holds the session mutex across its admission — the view answers
/// the roster alone (with the tools marked unavailable) instead of
/// queuing past this bound.
const SESSION_BUILD_WAIT: std::time::Duration = std::time::Duration::from_secs(12);

/// The poll interval while the create-time session build is in flight.
const SESSION_BUILD_POLL: std::time::Duration = std::time::Duration::from_millis(200);

impl Worker {
    /// `get_mcp_connections`: the roster from the session's MCP manager
    /// (auth gating over settings plus the built-in catalog), overlaid
    /// with the kernel's tool listing for every connected generic server.
    /// The listing waits out the create-time session build (bounded); a
    /// running turn holds the session past the bound, and the view then
    /// answers the roster alone with the tools marked unavailable.
    pub(crate) async fn handle_get_mcp_connections(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("get_mcp_connections") {
            return response;
        }
        let Some(manager) = self.engine.acp_mcp_manager() else {
            return response_failure(
                None,
                "get_mcp_connections",
                "MCP connections are not available in this session",
                None,
            );
        };
        // The roster read gates through the auth store, whose snapshot
        // takes a blocking lock (the pa-daemon test harnesses use the same
        // spawn_blocking shape for `list_status`) — never on the runtime.
        let roster = match tokio::task::spawn_blocking(move || {
            let manager = manager.lock().unwrap();
            manager.connection_roster()
        })
        .await
        {
            Ok(roster) => roster,
            Err(error) => {
                return response_failure(
                    None,
                    "get_mcp_connections",
                    &format!("MCP roster read failed: {error}"),
                    None,
                );
            }
        };
        // The servers whose tools the kernel can list: connected generic
        // servers (the built-in catalog surfaces through integration
        // skills, not the generic `mcp.list_tools` API).
        let listable: Vec<String> = roster
            .iter()
            .filter(|entry| entry.connected && entry.generic)
            .map(|entry| entry.server.clone())
            .collect();
        let mut listing: HashMap<String, Value> = HashMap::new();
        if !listable.is_empty() {
            if let Some(agent_engine) = self.agent_engine.as_ref() {
                // The session builds eagerly at create but commits to its
                // slot when the build finishes (the kernel prewarm races
                // it); a running turn holds the slot's mutex across its
                // admission. Wait out the build window — bounded, so the
                // view never queues behind a long turn — then list
                // through the built session's kernel.
                let deadline = std::time::Instant::now() + SESSION_BUILD_WAIT;
                let session = loop {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        break None;
                    }
                    match tokio::time::timeout(remaining, agent_engine.session.lock()).await {
                        // The mutex is held past the bound (a running
                        // turn): the roster answers alone.
                        Err(_) => break None,
                        Ok(session) if session.is_some() => break Some(session),
                        Ok(session) => {
                            drop(session);
                            tokio::time::sleep(SESSION_BUILD_POLL).await;
                        }
                    }
                };
                if let Some(session) = session {
                    if let Some(session) = session.as_ref() {
                        if let Some(connections) = session
                            .mcp_tool_listing(&listable, MCP_TOOL_LISTING_TIMEOUT_MS)
                            .await
                        {
                            for connection in connections {
                                if let Some(server) =
                                    connection.get("server").and_then(Value::as_str)
                                {
                                    listing.insert(server.to_string(), connection);
                                }
                            }
                        }
                    }
                }
            }
        }
        let connections: Vec<Value> = roster
            .into_iter()
            .map(|entry| {
                let mut value = serde_json::to_value(&entry).unwrap_or(Value::Null);
                let (tools, error) = match listing.get(&entry.server) {
                    Some(connection) => (
                        connection.get("tools").cloned().unwrap_or(Value::Null),
                        connection.get("error").cloned().unwrap_or(Value::Null),
                    ),
                    // No listing entry: either the server is not listable
                    // (skills-based built-in, or disconnected) or the
                    // listing was unavailable (no kernel yet, session busy).
                    None => (Value::Null, Value::Null),
                };
                value["tools"] = tools;
                value["error"] = error;
                value
            })
            .collect();
        response_success(
            None,
            "get_mcp_connections",
            Some(json!({ "connections": connections })),
        )
    }
}
