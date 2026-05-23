//! VT escape-sequence filtering for both directions.
//!
//! The pane is hostile (it runs arbitrary user-installed agents that execute
//! model-generated commands), so the filter is an **allowlist** of accepted
//! escape sequences, not a denylist of blocked ones.
//!
//! See `outbound.rs` (tmux → browser) and `inbound.rs` (browser → tmux) for
//! the per-direction allowlists, plus tests/vt_filter.rs for the security
//! fixtures.

pub mod inbound;
pub mod outbound;

pub use inbound::InboundFilter;
pub use outbound::OutboundFilter;
