//! Serverless Monitor Agent (`sma`).
//!
//! Module layout:
//! - [`agent`]: runtime orchestration (event loop, collection, reporting)
//! - [`transport`]: WebSocket report session with the Worker
//! - [`net`]: low-level HTTP / WebSocket / DNS / probe clients
//! - [`storage`]: config file, offline cache, traffic accounting
//! - [`metrics`]: per-platform system metric collection
//! - [`platform`]: per-platform service management and paths
//! - [`cli`]: command line entry, install / uninstall
//! - [`update`]: self-update from GitHub releases
//! - [`types`]: shared data types and constants
//! - [`util`]: logging, clock, command helpers

pub mod agent;
pub mod cli;
pub mod metrics;
pub mod net;
pub mod platform;
pub mod storage;
pub mod transport;
pub mod types;
pub mod update;
pub mod util;

#[cfg(test)]
#[path = "../tests/support/websocket.rs"]
pub(crate) mod test_websocket;

pub use agent::AgentError;
pub use cli::execute;
