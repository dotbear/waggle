//! Waggle, the app: its window, tray, notifications and the engine that feeds
//! them. The binary in `main.rs` wires these together; the library form lets
//! tools such as `examples/screenshot.rs` draw the real window offscreen.

slint::include_modules!();

pub mod config;
pub mod engine;
pub mod paths;
pub mod setup;
pub mod toast;
pub mod tray;
pub mod view;
