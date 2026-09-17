//! Extension host: the Node sidecar that runs user-authored TS/JS extensions
//! (design doc `docs/extensions-runner-design.md`).
//!
//! One sidecar process per session, NDJSON JSON over stdio (protocol
//! [`pa_types::extension_rpc`]), with the host script and vendored jiti
//! materialized under `<agentDir>/extension-host/`. This module owns the
//! process lifecycle, the RPC protocol, and the host script runtime that
//! loads extension modules.
//!
//! Stage status (design doc §4, as landed): stage 1 = sidecar process
//! management + RPC framing/protocol + the bundled protocol-peer script
//! (hello handshake, ping, event no-op, orderly shutdown). Stage 2 (module
//! loading with vendored jiti, registration landing, tool execution over
//! RPC) is next. Emission at the session-engine seams, the Rust-side
//! registry, and ctx-action binding are later stages; nothing in the
//! session engine spawns this host yet.
//!
//! TS ground truth: `packages/coding-agent/src/core/extensions/`
//! (loader.ts for the loading surface; runner.ts emit semantics arrive with
//! the event-surface stage).

mod client;
mod framing;
mod host;
mod script;

pub use client::{CtxCall, CtxCallHandler, RpcClient, SidecarNotification};
pub use framing::{encode_line, LineDecoder, LineLimits, DEFAULT_MAX_LINE_BYTES};
pub use host::{ExtensionHost, ExtensionHostSpec, HostScript, HostTimeouts};
