//! Cross-crate platform contracts: transport and process identity.
//!
//! pa-types is the only crate every platform consumer can depend on
//! (pa-tui depends on pa-types alone; pa-daemon, pa-cli, pa-core all sit
//! above it), so the shared platform traits live here. Every implementation
//! is cfg-gated per platform: Unix sockets and `/proc` today, named pipes and
//! native process queries on Windows later. Adding a platform means adding an
//! implementation - call sites never branch on `cfg` themselves.

pub mod identity;
pub mod process;
pub mod transport;

pub use identity::socket_identity;
pub use process::{is_process_alive, process_start_id};
pub use transport::{
    bind_transport, connect_blocking, connect_transport, BlockingTransportStream,
    TransportListener, TransportStream,
};
