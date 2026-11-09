//! Session transcripts, session-scoped goals and todos, and memory files.

pub mod memory;
pub mod session;
pub mod state;

// The shared helpers also serve crates that do not call every function.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../../src/testutil.rs"]
mod testutil;
