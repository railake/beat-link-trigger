//! # Beat Link Trigger (Rust rewrite)
//!
//! Trigger events and automate shows in response to CDJ activity. This crate holds
//! the application logic; the protocol lives in the `beat-link` crate.

pub mod carabiner;
pub mod config;
pub mod expressions;
pub mod legacy;
pub mod outputs;
pub mod triggers;

/// The version of this program.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
