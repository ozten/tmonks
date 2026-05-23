//! tmux control-mode driver.
//!
//! `control_mode::connect(...)` spawns a `tmux -CC attach -t <session>` child,
//! parses its line-based protocol into typed [`events::ControlEvent`]s, and
//! exposes a single-writer command channel with `oneshot` correlation.

pub mod commands;
pub mod control_mode;
pub mod escape;
pub mod events;
pub mod parser;

pub use control_mode::{ControlMode, TmuxConfig, connect, probe_version};
pub use events::{ControlEvent, PaneId, SessionId, WindowId};
