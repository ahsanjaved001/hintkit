//! Host-terminal input handling (SPEC §7 Phase 2).
//!
//! Reads raw bytes from the host terminal, scans them for bracketed-paste
//! delimiters, forwards them to the wrapped shell, and notifies the
//! suggestion debouncer when normal typing occurs. Paste bytes are
//! deliberately *not* fed to the debouncer (SPEC §4: "in paste mode,
//! don't parse at all until end of paste").
//!
//! **No byte content is ever logged from this module** (SPEC §9). Only
//! structural state transitions and drop counts are observable.

use std::io::{self, Read, Write};

use anyhow::{Context, Result};
use crossbeam_channel::{Sender, TrySendError};
use tracing::{debug, trace};

use crate::line_buffer::LineBuffer;
use crate::state::SharedState;

/// Bracketed-paste introducer: `ESC [ 2 0 0 ~`.
const PASTE_START: &[u8] = b"\x1b[200~";
/// Bracketed-paste terminator: `ESC [ 2 0 1 ~`.
const PASTE_END: &[u8] = b"\x1b[201~";

/// State machine that scans a host-terminal byte stream for bracketed-paste
/// delimiters. Each observed byte either advances the current partial match,
/// completes a delimiter (state transition), or resets the match. The
/// detector is purely observational — it does **not** modify the byte
/// stream; the shell still receives `\e[200~`/`\e[201~` so its own readline
/// can do bracketed-paste handling (history-expansion suppression, etc.).
#[derive(Debug)]
pub struct PasteDetector {
    state: PasteState,
    /// Number of leading bytes of the currently-expected delimiter matched
    /// so far. Reset to 0 on any byte that breaks the match, or to 1 if
    /// that byte itself is `ESC` (the start of a fresh attempted match).
    match_progress: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PasteState {
    /// Scanning for [`PASTE_START`].
    Normal,
    /// Scanning for [`PASTE_END`].
    Paste,
}

/// What observing a single byte produced. `EnteredPaste`/`ExitedPaste` are
/// the precise byte that completed the corresponding delimiter — exactly
/// one per transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectorEvent {
    /// Byte is part of normal typing.
    Normal,
    /// Byte is part of pasted content (between delimiters).
    Paste,
    /// Byte completed `PASTE_START`; state is now `Paste`.
    EnteredPaste,
    /// Byte completed `PASTE_END`; state is now `Normal`.
    ExitedPaste,
}

impl PasteDetector {
    pub fn new() -> Self {
        Self {
            state: PasteState::Normal,
            match_progress: 0,
        }
    }

    pub fn in_paste(&self) -> bool {
        matches!(self.state, PasteState::Paste)
    }

    /// Feed one byte. Returns the classification for that byte.
    pub fn observe(&mut self, byte: u8) -> DetectorEvent {
        let expected = match self.state {
            PasteState::Normal => PASTE_START,
            PasteState::Paste => PASTE_END,
        };

        let idx = self.match_progress as usize;
        if byte == expected[idx] {
            self.match_progress += 1;
            if self.match_progress as usize == expected.len() {
                let exiting = self.state == PasteState::Paste;
                self.match_progress = 0;
                self.state = if exiting {
                    PasteState::Normal
                } else {
                    PasteState::Paste
                };
                return if exiting {
                    DetectorEvent::ExitedPaste
                } else {
                    DetectorEvent::EnteredPaste
                };
            }
            // Partial match still in progress — the byte is consumed by the
            // match attempt but doesn't transition state.
            return match self.state {
                PasteState::Normal => DetectorEvent::Normal,
                PasteState::Paste => DetectorEvent::Paste,
            };
        }

        // Mismatch. If this byte is itself ESC, it's the start of a fresh
        // attempted match; otherwise reset completely.
        self.match_progress = if byte == 0x1b { 1 } else { 0 };
        match self.state {
            PasteState::Normal => DetectorEvent::Normal,
            PasteState::Paste => DetectorEvent::Paste,
        }
    }
}

/// Run the host-stdin → PTY-master forwarding loop. Daemonized: blocks on
/// stdin reads indefinitely; the OS reclaims it when the process exits.
///
/// For each read batch:
/// 1. Write every byte to the PTY (always — paste markers go to the shell
///    too so its own readline handles them).
/// 2. Update the paste-detector state on each byte.
/// 3. For non-paste bytes, feed the [`LineBuffer`] and push snapshots to
///    [`SharedState`] so the suggestion engine can see what the user is
///    typing.
/// 4. If any non-paste activity occurred, send a single tick to the
///    debouncer. `try_send` on a bounded capacity-1 channel coalesces
///    bursts naturally.
pub fn run_stdin_writer(
    mut writer: Box<dyn Write + Send>,
    tick_tx: Sender<()>,
    state: SharedState,
) -> Result<()> {
    let mut stdin = io::stdin().lock();
    let mut detector = PasteDetector::new();
    let mut line = LineBuffer::new();
    let mut buf = [0u8; 8192];
    let mut dropped_ticks: u64 = 0;

    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("reading host stdin"),
        };
        let chunk = &buf[..n];

        writer.write_all(chunk).context("writing to pty master")?;
        writer.flush().context("flushing pty master")?;

        let mut tick_needed = false;
        let mut line_changed = false;
        for &byte in chunk {
            match detector.observe(byte) {
                DetectorEvent::Normal => {
                    tick_needed = true;
                    if line.observe(byte) {
                        line_changed = true;
                    }
                }
                DetectorEvent::ExitedPaste => {
                    tick_needed = true;
                    // The paste content went straight to the shell; our
                    // line model lost track of where the cursor sits
                    // afterward. Reset to avoid producing bad suggestions
                    // off a desynced buffer.
                    line.reset();
                    line_changed = true;
                }
                DetectorEvent::EnteredPaste => {
                    debug!("entered paste mode");
                }
                DetectorEvent::Paste => {
                    // Don't feed paste bytes into the line model — they
                    // bypass interactive line-editing semantics.
                }
            }
        }

        if line_changed {
            let (snapshot, cursor) = line.snapshot();
            state.on_line_update(snapshot, cursor);
        }

        if detector.in_paste() {
            // Inside an unterminated paste — never tick.
            continue;
        }
        if tick_needed {
            match tick_tx.try_send(()) {
                Ok(()) | Err(TrySendError::Full(_)) => {
                    // Full is the intended drop-on-full behavior — the
                    // debouncer already has a tick pending and will
                    // coalesce future input into that one timer.
                }
                Err(TrySendError::Disconnected(_)) => {
                    trace!("debouncer disconnected; stdin-writer continues forwarding bytes only");
                    return Ok(());
                }
            }
            if tick_tx.is_full() {
                dropped_ticks = dropped_ticks.wrapping_add(1);
                if dropped_ticks % 1024 == 0 {
                    trace!(dropped_ticks, "debouncer tick channel pressure");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observe_all(detector: &mut PasteDetector, bytes: &[u8]) -> Vec<DetectorEvent> {
        bytes.iter().map(|&b| detector.observe(b)).collect()
    }

    #[test]
    fn normal_bytes_stay_normal() {
        let mut d = PasteDetector::new();
        let events = observe_all(&mut d, b"hello world\n");
        assert!(events.iter().all(|e| *e == DetectorEvent::Normal));
        assert!(!d.in_paste());
    }

    #[test]
    fn detects_paste_start_in_single_buffer() {
        let mut d = PasteDetector::new();
        let events = observe_all(&mut d, PASTE_START);
        let last = events.last().copied().unwrap();
        assert_eq!(last, DetectorEvent::EnteredPaste);
        assert!(d.in_paste());
    }

    #[test]
    fn detects_paste_start_split_across_buffers() {
        let mut d = PasteDetector::new();
        observe_all(&mut d, &PASTE_START[..3]);
        assert!(!d.in_paste());
        let events = observe_all(&mut d, &PASTE_START[3..]);
        assert_eq!(*events.last().unwrap(), DetectorEvent::EnteredPaste);
        assert!(d.in_paste());
    }

    #[test]
    fn paste_then_end_returns_to_normal() {
        let mut d = PasteDetector::new();
        observe_all(&mut d, PASTE_START);
        let mid = observe_all(&mut d, b"some pasted content\nwith newlines");
        assert!(mid.iter().all(|e| *e == DetectorEvent::Paste));
        let end = observe_all(&mut d, PASTE_END);
        assert_eq!(*end.last().unwrap(), DetectorEvent::ExitedPaste);
        assert!(!d.in_paste());
    }

    #[test]
    fn false_paste_start_resets_cleanly() {
        // `\e[20x` looks like the start of `\e[200~` but isn't.
        let mut d = PasteDetector::new();
        observe_all(&mut d, b"\x1b[20x");
        assert!(!d.in_paste());
        // Subsequent real paste start still works.
        observe_all(&mut d, PASTE_START);
        assert!(d.in_paste());
    }

    #[test]
    fn nested_paste_start_inside_paste_is_ignored() {
        let mut d = PasteDetector::new();
        observe_all(&mut d, PASTE_START);
        // A literal `\e[200~` inside the paste should NOT exit paste mode
        // (we're scanning for `\e[201~`, not for any escape sequence).
        observe_all(&mut d, PASTE_START);
        assert!(d.in_paste());
        observe_all(&mut d, PASTE_END);
        assert!(!d.in_paste());
    }

    #[test]
    fn esc_byte_restarts_match_progress() {
        // `\e[2` then another `\e[200~` should still match cleanly.
        let mut d = PasteDetector::new();
        observe_all(&mut d, b"\x1b[2");
        observe_all(&mut d, PASTE_START);
        assert!(d.in_paste());
    }
}
