//! Low-level network clients: HTTP(S), WebSocket, DNS resolution,
//! public IP lookup and latency probes.

pub mod dns;
pub mod http;
pub mod iplookup;
pub mod probe;
pub mod websocket;
