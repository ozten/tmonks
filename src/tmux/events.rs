//! Typed events emitted by the control-mode parser.

/// A pane id (`%0`, `%1`, …) from tmux. We keep it as `String` because it's
/// already short and we move it around a lot — avoiding numeric parsing keeps
/// us forward-compatible if tmux ever changes the prefix.
pub type PaneId = String;
pub type SessionId = String;
pub type WindowId = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    /// `%output %<pane> <encoded>` — raw, already-decoded pane bytes.
    Output { pane: PaneId, data: Vec<u8> },

    /// `%begin <ts> <cmd_num> <flags>` — start of a response block.
    ///
    /// `flags` distinguishes:
    /// * `0` = auto-emitted (initial attach state, etc.) — no caller is waiting.
    /// * `1` = response to an explicit command — must be FIFO-matched to a
    ///   pending oneshot.
    ///
    /// `cmd_num` is tmux's server-wide counter and is NOT useful for
    /// correlation (it starts at whatever value the server happens to be at).
    BeginBlock { cmd_num: u32, flags: u32 },

    /// `%end <ts> <cmd_num> <flags>` — successful end of a response block.
    EndBlock { cmd_num: u32, flags: u32 },

    /// `%error <ts> <cmd_num> <flags>` — failed end of a response block.
    ErrorBlock { cmd_num: u32, flags: u32 },

    /// Notification: the active session for the attached client changed.
    SessionChanged { session_id: SessionId, name: String },

    /// Notification: the set of sessions on the server changed (created/removed).
    SessionsChanged,

    /// Notification: a window was added.
    WindowAdd { window_id: WindowId },

    /// Notification: a window was closed.
    WindowClose { window_id: WindowId },

    /// Notification: a window was renamed.
    WindowRenamed { window_id: WindowId, name: String },

    /// Notification: the active pane within a window changed.
    /// (`%window-pane-changed @<wid> %<pid>`)
    WindowPaneChanged { window_id: WindowId, pane: PaneId },

    /// Notification: a window's layout changed.
    LayoutChange { window_id: WindowId, layout: String },

    /// Notification: a pane entered or left copy/scroll mode.
    PaneModeChanged { pane: PaneId },

    /// Notification: a client was detached.
    ClientDetached,

    /// Notification: tmux is about to exit. Treat as terminal.
    Exit { code: Option<i32> },

    /// Inside-block content — only emitted while a `%begin` is open. The
    /// reader stitches these into command responses.
    BlockLine { cmd_num: u32, line: Vec<u8> },

    /// Catch-all for notifications we recognise but don't model. Logged at
    /// debug; kept here so the reader doesn't have to maintain its own
    /// `should-ignore` set in a separate place.
    Ignored { kind: String, raw: Vec<u8> },
}
