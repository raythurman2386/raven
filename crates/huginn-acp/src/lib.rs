//! Agent Client Protocol (ACP) v1 adapter.
//!
//! Huginn speaks ACP over stdio (`huginn --acp`) so editors can attach. The
//! adapter is a thin JSON-RPC layer over [`huginn_agent::Agent`]. Stdio MCP
//! servers from `session/new` (and native config) are connected per session;
//! client-owned filesystem / terminal methods are unused. See [`protocol`]
//! for the wire subset and [`server`] for the session loop.

pub mod protocol;
pub mod server;

#[cfg(test)]
mod tests;

pub use server::{run_stdio, AcpServer, FrameWrite};
