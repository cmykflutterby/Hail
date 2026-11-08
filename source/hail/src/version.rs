//! HAIL product version.
//!
//! The authoritative release number is the workspace `version` in Cargo.toml.
//! Keep this prominent Rust constant as the application-facing version API.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
