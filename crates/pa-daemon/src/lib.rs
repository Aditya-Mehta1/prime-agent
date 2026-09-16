//! Session supervisor, worker processes, and wire protocol for Prime Agent.
//!
//! Ported from the TypeScript daemon: `modes/daemon/*`, `modes/session-worker/*`,
//! `core/session-manager.ts`, and `core/session-lease.ts`. The supervisor hosts
//! no sessions: it spawns one worker process per active session, supervises
//! restarts with backoff, and routes clients. Sessions persist as append-only
//! JSONL under `<agent-dir>/sessions/` using the same layout as the TS product.

pub mod agent_engine;
pub mod descriptor;
pub mod engine;
pub mod framing;
pub mod journal;
pub mod lease;
pub mod paths;
pub mod protocol;
pub mod session_stats;
pub mod session_store;
pub mod socket;
pub mod supervisor;
pub mod types;
pub mod util;
pub mod worker;
