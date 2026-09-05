//! The debug side panel's own state and pure logic — gated behind the
//! `debug-panel` Cargo feature (on by default) *and* `debug_assertions`,
//! so it's present in an ordinary `cargo build`/`cargo run` but entirely
//! absent from a release build (see `Cargo.toml`'s own comment on why:
//! the stall/failure triggers this adds are actively harmful in a real
//! user's hands, not just clutter). Rendering lives in `view.rs`
//! (`render_debug_button`/
//! `render_debug_confirm_dialog`/`render_debug_panel`), the open/close/
//! confirm transitions and the `send_command`/`poll_events` interception
//! points in `lib.rs` — same "pure state here, rendering in `view.rs`,
//! transitions in `lib.rs`" split `exit.rs` already established for the
//! exit-prompt state machine.
//!
//! Three of the four stall/failure triggers the design called for are
//! implemented (Tx stall, Tx failure, Rx stall); the fourth — a genuine
//! Rx *failure*, an `Event` `gui-core` computed but never sent — is
//! deliberately left unimplemented. Faithfully reproducing it needs real
//! `gui-core` cooperation (something like a debug-only `Command` the
//! actor honors by computing the real `Outcome` and discarding it
//! instead of sending `Event::Completed`), which would mean adding
//! debug-only surface to `gui-core`'s production `Command` enum for a
//! purely diagnostic feature — a real enough cost that it's left as an
//! open decision rather than assumed here, per this crate's own README.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use gui_core::{Command, Event};

/// Bounded so a long session's memory doesn't grow without limit — same
/// judgment-call spirit as `gui-core`'s planned undo stack cap.
const LOG_CAPACITY: usize = 500;

/// How long a triggered Tx/Rx stall holds — not a number anyone asked
/// for, long enough to be clearly observable in the panel, short enough
/// not to make the triggering session unusable for long.
const STALL_DURATION: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogDirection {
    Tx,
    /// A `Command` that was about to be sent but got dropped instead —
    /// see `DebugPanelState::on_tx`'s "Tx failure" branch.
    TxDropped,
    Rx,
}

pub(crate) struct LogEntry {
    pub at: Instant,
    pub direction: LogDirection,
    pub detail: String,
}

/// All of the debug panel's own state — one field on `GuiApp`
/// (`#[cfg(all(feature = "debug-panel", debug_assertions))] debug:
/// DebugPanelState`), so a release build carries none of this at all,
/// not even an empty struct.
#[derive(Default)]
pub(crate) struct DebugPanelState {
    pub open: bool,
    /// The "are you sure you want to open the debug panel?" modal —
    /// `Some` while it's open. Only gates *opening*; closing (clicking
    /// the same toolbar button again once `open` is already true) needs
    /// no confirmation, see `GuiApp::debug_panel_button_clicked`.
    pub confirm_open: bool,
    pub log: VecDeque<LogEntry>,
    tx_stall_until: Option<Instant>,
    held_tx: VecDeque<Command>,
    drop_next_tx: bool,
    rx_stall_until: Option<Instant>,
}

impl DebugPanelState {
    fn push_log(&mut self, direction: LogDirection, detail: String) {
        if self.log.len() >= LOG_CAPACITY {
            self.log.pop_front();
        }
        self.log.push_back(LogEntry {
            at: Instant::now(),
            direction,
            detail,
        });
    }

    /// Called for every outgoing `Command`, logging it either way.
    /// Returns `Some(command)` (unchanged) when it should actually be
    /// sent right now, `None` when this call has already handled it
    /// (dropped outright, or queued behind an active stall) and the
    /// caller must not also send it.
    pub fn on_tx(&mut self, command: Command) -> Option<Command> {
        if self.drop_next_tx {
            self.drop_next_tx = false;
            self.push_log(LogDirection::TxDropped, format!("{command:?}"));
            return None;
        }
        if self.tx_stall_until.is_some() {
            self.push_log(
                LogDirection::Tx,
                format!("{command:?} (queued — Tx stalled)"),
            );
            self.held_tx.push_back(command);
            return None;
        }
        self.push_log(LogDirection::Tx, format!("{command:?}"));
        Some(command)
    }

    pub fn log_rx(&mut self, event: &Event) {
        self.push_log(LogDirection::Rx, format!("{event:?}"));
    }

    /// `true` while a Tx stall means outgoing commands should be held
    /// rather than sent — checked by `GuiApp::send_command` via `on_tx`
    /// itself (this is really just for the panel's own "Tx Stall" button
    /// state / tests, `on_tx` doesn't call it directly).
    pub fn is_tx_stalled(&self) -> bool {
        self.tx_stall_until.is_some()
    }

    pub fn is_rx_stalled(&self, now: Instant) -> bool {
        self.rx_stall_until.is_some_and(|until| now < until)
    }

    pub fn trigger_tx_stall(&mut self, now: Instant) {
        self.tx_stall_until = Some(now + STALL_DURATION);
    }

    pub fn trigger_tx_failure(&mut self) {
        self.drop_next_tx = true;
    }

    pub fn trigger_rx_stall(&mut self, now: Instant) {
        self.rx_stall_until = Some(now + STALL_DURATION);
    }

    /// Called once per frame. If an active Tx stall has just elapsed,
    /// releases every `Command` queued up behind it, in the order they
    /// were originally sent, and returns them for the caller to actually
    /// forward to `CoreHandle::send` (this type owns no `CoreHandle`
    /// itself). An `Rx` stall needs no equivalent: `GuiApp::poll_events`
    /// just skips draining `CoreHandle::try_recv_event` while stalled,
    /// so the real events simply queue up in the channel itself with
    /// nothing for this type to hold.
    pub fn release_stalled_tx(&mut self, now: Instant) -> Vec<Command> {
        let Some(until) = self.tx_stall_until else {
            return Vec::new();
        };
        if now < until {
            return Vec::new();
        }
        self.tx_stall_until = None;
        self.held_tx.drain(..).collect()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn some_command() -> Command {
        Command::Validate { request: 1 }
    }

    #[test]
    fn a_plain_send_passes_through_and_logs_tx() {
        let mut state = DebugPanelState::default();
        let command = state.on_tx(some_command());
        assert!(command.is_some());
        assert_eq!(state.log.len(), 1);
        assert_eq!(state.log[0].direction, LogDirection::Tx);
    }

    #[test]
    fn triggered_tx_failure_drops_exactly_the_next_send() {
        let mut state = DebugPanelState::default();
        state.trigger_tx_failure();

        let dropped = state.on_tx(some_command());
        assert!(dropped.is_none());
        assert_eq!(state.log.back().unwrap().direction, LogDirection::TxDropped);

        // Only the one send was dropped — not a standing failure mode.
        let passed = state.on_tx(some_command());
        assert!(passed.is_some());
    }

    #[test]
    fn triggered_tx_stall_holds_commands_until_released() {
        let mut state = DebugPanelState::default();
        let now = Instant::now();
        state.trigger_tx_stall(now);
        assert!(state.is_tx_stalled());

        let held = state.on_tx(some_command());
        assert!(held.is_none());

        // Not yet elapsed.
        assert!(state.release_stalled_tx(now).is_empty());

        // Elapsed — the held command comes back, in order, and the
        // stall itself clears.
        let released = state.release_stalled_tx(now + STALL_DURATION);
        assert_eq!(released.len(), 1);
        assert!(!state.is_tx_stalled());
    }

    #[test]
    fn release_stalled_tx_with_no_active_stall_returns_nothing() {
        let mut state = DebugPanelState::default();
        assert!(state.release_stalled_tx(Instant::now()).is_empty());
    }

    #[test]
    fn rx_stall_is_active_only_until_it_elapses() {
        let mut state = DebugPanelState::default();
        let now = Instant::now();
        assert!(!state.is_rx_stalled(now));

        state.trigger_rx_stall(now);
        assert!(state.is_rx_stalled(now));
        assert!(!state.is_rx_stalled(now + STALL_DURATION));
    }

    #[test]
    fn the_log_is_bounded() {
        let mut state = DebugPanelState::default();
        for _ in 0..(LOG_CAPACITY + 10) {
            state.on_tx(some_command());
        }
        assert_eq!(state.log.len(), LOG_CAPACITY);
    }
}
