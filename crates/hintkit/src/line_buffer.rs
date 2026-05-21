//! Best-effort tracking of the wrapped shell's current command line
//! (SPEC §7 Phase 5).
//!
//! The shell's own readline holds the real source of truth — what's
//! visible on screen and what gets executed when the user presses
//! Enter. We can't query that without OSC 633-style extensions
//! (which aren't universal), so we model a parallel buffer fed by the
//! same byte stream the shell receives, interpreting the common
//! emacs-mode bindings that 95% of users hit:
//!
//! - Printable bytes (0x20–0xff) insert at the cursor.
//! - 0x7f (DEL) and 0x08 (BS) → backspace one byte.
//! - 0x01 (^A) → cursor to line start.
//! - 0x05 (^E) → cursor to line end.
//! - 0x15 (^U) → kill from cursor back to start.
//! - 0x0b (^K) → kill from cursor to end.
//! - 0x17 (^W) → kill word backwards.
//! - `ESC [ D` / `ESC [ C` → cursor left / right.
//! - `ESC [ A` / `ESC [ B` (history navigation) → reset, since we
//!   can't follow the shell's history buffer.
//! - 0x0d / 0x0a / 0x03 / 0x04 (Enter, ^C, ^D) → reset; the current
//!   line is submitted or cancelled.
//!
//! Known desync triggers we accept for v0.1: vi-mode keymaps,
//! reverse-search (^R), multi-line input, anything that puts the
//! shell into a sub-buffer (e.g. quote-continuation prompts). When we
//! desync we simply stop suggesting until the next OSC 133 A / B
//! resync from the integration script.

use std::mem;

#[derive(Debug, Clone, PartialEq, Eq)]
enum EscapeState {
    Idle,
    /// Saw an ESC; the next byte determines whether this is a CSI
    /// (`[`), an Alt+key shortcut, or something else.
    SawEsc,
    /// Inside `ESC [ … final` — collecting parameter bytes until the
    /// CSI final-byte (0x40–0x7e) arrives.
    Csi(Vec<u8>),
}

/// Mutable model of the in-progress command line.
#[derive(Debug)]
pub struct LineBuffer {
    buf: Vec<u8>,
    /// Cursor byte offset. Always in `0..=buf.len()`.
    cursor: usize,
    escape: EscapeState,
}

impl Default for LineBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LineBuffer {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            cursor: 0,
            escape: EscapeState::Idle,
        }
    }

    /// Clear the buffer. Called on Enter, ^C, ^D, or any
    /// integration-driven sync point (OSC 133 A / C).
    pub fn reset(&mut self) {
        self.buf.clear();
        self.cursor = 0;
        self.escape = EscapeState::Idle;
    }

    /// Current line as &str. Returns `""` if the buffer ever holds
    /// invalid UTF-8 mid-multi-byte-edit — better than panicking.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf).unwrap_or("")
    }

    /// The current cursor byte offset. Production code reads cursor
    /// position via `snapshot()`; this accessor exists for tests and
    /// for the future renderer (Phase 6).
    #[allow(dead_code)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Snapshot the current line + cursor without holding internal
    /// references (suitable for stuffing into `SharedState`).
    pub fn snapshot(&self) -> (String, usize) {
        (self.as_str().to_string(), self.cursor)
    }

    /// Feed one input byte. Returns `true` if the line content
    /// (text or cursor) changed in a way the suggestion engine
    /// should re-run for.
    pub fn observe(&mut self, byte: u8) -> bool {
        match mem::replace(&mut self.escape, EscapeState::Idle) {
            EscapeState::Idle => self.observe_idle(byte),
            EscapeState::SawEsc => self.observe_after_esc(byte),
            EscapeState::Csi(params) => self.observe_in_csi(params, byte),
        }
    }

    fn observe_idle(&mut self, byte: u8) -> bool {
        match byte {
            0x1b => {
                self.escape = EscapeState::SawEsc;
                false
            }
            0x0d | 0x0a | 0x03 | 0x04 => {
                // Enter, ^C, ^D: line is submitted or cancelled.
                let changed = !self.buf.is_empty() || self.cursor != 0;
                self.reset();
                changed
            }
            0x7f | 0x08 => self.backspace(),
            0x01 => {
                let changed = self.cursor != 0;
                self.cursor = 0;
                changed
            }
            0x05 => {
                let changed = self.cursor != self.buf.len();
                self.cursor = self.buf.len();
                changed
            }
            0x15 => self.kill_to_start(),
            0x0b => self.kill_to_end(),
            0x17 => self.kill_word_backward(),
            0x09 => false, // Tab — Phase 6 intercepts it for accept.
            0x20..=0x7e | 0x80..=0xff => {
                self.buf.insert(self.cursor, byte);
                self.cursor += 1;
                true
            }
            _ => false,
        }
    }

    fn observe_after_esc(&mut self, byte: u8) -> bool {
        if byte == b'[' {
            self.escape = EscapeState::Csi(Vec::new());
            false
        } else if byte == 0x1b {
            // Another ESC — stay in SawEsc to retry.
            self.escape = EscapeState::SawEsc;
            false
        } else {
            // Alt+key or another ESC-prefixed sequence we don't model.
            // Drop back to Idle without modifying the buffer.
            false
        }
    }

    fn observe_in_csi(&mut self, mut params: Vec<u8>, byte: u8) -> bool {
        if (0x40..=0x7e).contains(&byte) {
            // Final byte. Dispatch based on it; params are mostly
            // unused for our limited set of supported CSI sequences.
            self.handle_csi_final(&params, byte)
        } else {
            // Still collecting params (digits, ';', '?', etc.).
            if params.len() < 32 {
                params.push(byte);
            }
            self.escape = EscapeState::Csi(params);
            false
        }
    }

    fn handle_csi_final(&mut self, _params: &[u8], final_byte: u8) -> bool {
        match final_byte {
            b'D' => {
                // Left arrow.
                if self.cursor > 0 {
                    self.cursor -= 1;
                    true
                } else {
                    false
                }
            }
            b'C' => {
                // Right arrow.
                if self.cursor < self.buf.len() {
                    self.cursor += 1;
                    true
                } else {
                    false
                }
            }
            b'A' | b'B' => {
                // Up / Down arrow → history recall. Shell replaces
                // its buffer with a previous command; we have no way
                // to mirror that. Reset and stop suggesting until the
                // next prompt syncs us.
                let changed = !self.buf.is_empty();
                self.reset();
                changed
            }
            _ => false,
        }
    }

    fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        self.buf.remove(self.cursor);
        true
    }

    fn kill_to_start(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.buf.drain(..self.cursor);
        self.cursor = 0;
        true
    }

    fn kill_to_end(&mut self) -> bool {
        if self.cursor >= self.buf.len() {
            return false;
        }
        self.buf.truncate(self.cursor);
        true
    }

    fn kill_word_backward(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        // Skip trailing whitespace, then delete the preceding non-
        // whitespace run. Matches bash/zsh emacs ^W behavior closely
        // enough for v0.1.
        let mut end = self.cursor;
        while end > 0 && self.buf[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        while end > 0 && !self.buf[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        self.buf.drain(end..self.cursor);
        self.cursor = end;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(buf: &mut LineBuffer, bytes: &[u8]) -> usize {
        let mut changes = 0;
        for &b in bytes {
            if buf.observe(b) {
                changes += 1;
            }
        }
        changes
    }

    #[test]
    fn typing_appends_to_buffer() {
        let mut lb = LineBuffer::new();
        feed(&mut lb, b"git checkout");
        assert_eq!(lb.as_str(), "git checkout");
        assert_eq!(lb.cursor(), 12);
    }

    #[test]
    fn backspace_removes_one_byte() {
        let mut lb = LineBuffer::new();
        feed(&mut lb, b"abc");
        feed(&mut lb, &[0x7f]);
        assert_eq!(lb.as_str(), "ab");
        assert_eq!(lb.cursor(), 2);
    }

    #[test]
    fn ctrl_a_jumps_to_start() {
        let mut lb = LineBuffer::new();
        feed(&mut lb, b"abc");
        feed(&mut lb, &[0x01]);
        assert_eq!(lb.cursor(), 0);
        assert_eq!(lb.as_str(), "abc");
    }

    #[test]
    fn ctrl_u_kills_from_cursor_to_start() {
        let mut lb = LineBuffer::new();
        feed(&mut lb, b"hello world");
        // Cursor at 11. ^A → 0. ^U should no-op since nothing's left
        // of the cursor.
        feed(&mut lb, &[0x05]); // move to end (no-op, already there)
        feed(&mut lb, &[0x15]); // kill back to start
        assert_eq!(lb.as_str(), "");
        assert_eq!(lb.cursor(), 0);
    }

    #[test]
    fn ctrl_w_kills_word_backward() {
        let mut lb = LineBuffer::new();
        feed(&mut lb, b"git checkout main");
        feed(&mut lb, &[0x17]);
        assert_eq!(lb.as_str(), "git checkout ");
    }

    #[test]
    fn enter_resets_buffer() {
        let mut lb = LineBuffer::new();
        feed(&mut lb, b"echo hi");
        feed(&mut lb, &[0x0d]);
        assert_eq!(lb.as_str(), "");
        assert_eq!(lb.cursor(), 0);
    }

    #[test]
    fn left_arrow_moves_cursor() {
        let mut lb = LineBuffer::new();
        feed(&mut lb, b"abc");
        // ESC [ D
        feed(&mut lb, &[0x1b, b'[', b'D']);
        assert_eq!(lb.cursor(), 2);
        assert_eq!(lb.as_str(), "abc");
    }

    #[test]
    fn right_arrow_moves_cursor() {
        let mut lb = LineBuffer::new();
        feed(&mut lb, b"abc");
        feed(&mut lb, &[0x01]); // ^A
        feed(&mut lb, &[0x1b, b'[', b'C']);
        assert_eq!(lb.cursor(), 1);
    }

    #[test]
    fn up_arrow_resets_buffer_for_desync() {
        let mut lb = LineBuffer::new();
        feed(&mut lb, b"some-cmd");
        feed(&mut lb, &[0x1b, b'[', b'A']);
        assert_eq!(lb.as_str(), "");
    }

    #[test]
    fn csi_split_across_observe_calls() {
        let mut lb = LineBuffer::new();
        feed(&mut lb, b"abc");
        // Bytes arrive one at a time across multiple stdin reads.
        lb.observe(0x1b);
        lb.observe(b'[');
        let changed = lb.observe(b'D');
        assert!(changed);
        assert_eq!(lb.cursor(), 2);
    }

    #[test]
    fn insert_in_middle() {
        let mut lb = LineBuffer::new();
        feed(&mut lb, b"ac");
        feed(&mut lb, &[0x1b, b'[', b'D']); // cursor left → 1
        feed(&mut lb, b"b");
        assert_eq!(lb.as_str(), "abc");
        assert_eq!(lb.cursor(), 2);
    }

    #[test]
    fn snapshot_returns_owned_pair() {
        let mut lb = LineBuffer::new();
        feed(&mut lb, b"hi");
        let (line, cursor) = lb.snapshot();
        assert_eq!(line, "hi");
        assert_eq!(cursor, 2);
    }
}
