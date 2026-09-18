//! Session supervisor, worker processes, and wire protocol for Prime Agent.
//!
//! Ported from the TypeScript daemon: `modes/daemon/*`, `modes/session-worker/*`,
//! `core/session-manager.ts`, and `core/session-lease.ts`. The supervisor hosts
//! no sessions: it spawns one worker process per active session, supervises
//! restarts with backoff, and routes clients. Sessions persist as append-only
//! JSONL under `<agent-dir>/sessions/` using the same layout as the TS product.

pub mod acp;
pub mod agent_engine;
pub(crate) mod agent_messaging;
pub(crate) mod agent_roster;
pub mod compaction;
pub mod descriptor;
pub mod engine;
pub mod framing;
pub mod journal;
pub mod lease;
pub mod mcp_login;
pub(crate) mod messaging;
pub mod paths;
pub(crate) mod peer;
pub(crate) mod peer_client;
pub(crate) mod peer_tickets;
pub mod platform;
pub mod protocol;
pub mod registration;
pub(crate) mod registry;
pub mod rlm_child_model;
pub mod rlm_children;
pub(crate) mod session_commands;
pub mod session_stats;
pub mod session_store;
pub mod side_question;
pub mod snapshot_stream;
pub mod socket;
pub mod status_line;
pub(crate) mod streaming;
pub mod supervisor;
pub mod supervisor_link;
pub(crate) mod supervisor_roster;
pub mod types;
pub mod util;
pub mod worker;
