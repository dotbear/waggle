//! The portable engine behind Waggle.
//!
//! Everything here builds and runs on Linux and Windows. Nothing here draws a
//! window or calls a platform tool directly: platform behaviour sits behind
//! traits, and each front end supplies its own backend.

pub mod alert;
pub mod app;
pub mod bridges;
pub mod config;
pub mod directorbot;
pub mod esi;
pub mod files;
pub mod industry;
pub mod intel;
pub mod logs;
pub mod mining;
pub mod notify;
pub mod secrets;
pub mod skills;
pub mod store;
pub mod universe;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
