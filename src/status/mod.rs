//! Status detection: per-pane "what is this agent doing right now?" inference
//! from rendered screen content.

pub mod matchers;
pub mod poller;
pub mod version_probe;

pub use matchers::{Status, match_status};
