//! Agent Client Protocol (ACP) v1 adapter.
//!
//! Raven speaks ACP over stdio (`raven acp`) so editors can attach. The
//! adapter is a thin JSON-RPC layer over [`crate::agent::Agent`] — no MCP,
//! no client-owned filesystem or terminal. See [`protocol`] for the wire
//! subset and [`server`] for the session loop.

pub mod protocol;
pub mod server;

#[cfg(test)]
mod tests;

pub use server::{run_stdio, AcpServer, FrameWrite};
