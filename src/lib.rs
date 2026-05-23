//! tmons — Web UI for tmux sessions.
//!
//! This crate exposes a small public surface so the binary and the integration
//! tests can share infrastructure (auth, router, state). Internal modules are
//! kept private to keep the API tight.

pub mod assets;
pub mod auth;
pub mod cli;
pub mod observability;
pub mod server;
pub mod templates;
pub mod tmux;
pub mod vt_filter;
pub mod ws_pane;

pub use auth::{COOKIE_NAME, Token};
pub use server::{AppState, BuildInfo, router};
