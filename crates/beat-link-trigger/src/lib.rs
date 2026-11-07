//! # Beat Link Trigger (Rust rewrite)
//!
//! Trigger events and automate shows in response to CDJ activity. This crate holds
//! the application logic; the protocol lives in the `beat-link` crate.

pub mod app;
pub mod carabiner;
pub mod config;
pub mod expressions;
#[cfg(feature = "gui")]
pub mod gui;
pub mod legacy;
pub mod logs;
pub mod outputs;
pub mod overlay;
pub mod playlist;
pub mod report;
pub mod shows;
pub mod simulator;
pub mod triggers;

/// The version of this program.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
