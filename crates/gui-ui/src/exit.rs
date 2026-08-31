//! The exit-prompt state machine. See `README.md`'s "Exit" section —
//! "Stage 1: prompt to save, bounded so it cannot hang".

use std::time::Instant;

use gui_core::RequestId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitDialogState {
    Asking,
    Saving { request: RequestId, deadline: Instant },
    TimedOut { request: RequestId },
    /// The awaited save completed (or the dialog was dismissed via
    /// Discard/"Exit anyway") — nothing left to show; `GuiApp::ui`
    /// consumes this on the next frame to run Stage 2 (send
    /// `Command::Shutdown`, close the viewport) exactly once. Kept
    /// distinct from `None`, since "no dialog open" and "dialog just
    /// resolved, close now" are not the same state.
    Ready,
}
