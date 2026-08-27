//! Library crate for Raven — exposes the core modules so benchmarks and
//! integration tests can import them. The binary (`main.rs`) re-exports the
//! same modules via `use raven::...`.

pub mod acp;
pub mod agent;
pub mod commands;
pub mod config;
pub mod context;
pub mod error;
pub mod memory;
pub mod plan;
pub mod plugins;
pub mod repomap;
pub mod runner;
pub mod session;
pub mod skills;
pub mod state;
pub mod tokenizer;
pub mod tools;
pub mod tui;
pub mod update;
pub mod web;
