//! Shell-output handling (SPEC §7 Phase 3).
//!
//! Reads bytes from the PTY master, scans them for OSC sequences emitted
//! by the shell-integration script, updates [`crate::state::SharedState`]
//! accordingly, and forwards **every** byte unchanged to the host stdout.
//!
//! The parser is observational — never modifies the byte stream. Other
//! terminal tools may also be interpreting OSC 133 (iTerm2, WezTerm,
//! VS Code, kitty all do), and we want them to keep working.
//!
//! **No byte content is ever logged from this module** (SPEC §9). Only
//! parsed structural events (state transitions, cwd updates) and counts.

use std::io::{self, Read, Write};

use tracing::{debug, trace};

use crate::state::SharedState;

/// OSC sequences are framed by `ESC ]` … (`BEL` | `ESC \\`). Cap the
/// accumulator at this size so a malformed stream can't make us
/// allocate unboundedly — any legitimate OSC 133/7 payload is well
/// under 4 KB.
const MAX_OSC_PAYLOAD: usize = 4096;

/// What an OSC sequence successfully parsed from the shell output told
/// us about the shell's lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscEvent {
    /// `\e]133;A\e\\` — prompt is starting.
    Osc133PromptStart,
    /// `\e]133;B\e\\` — end-of-prompt / start-of-input.
    Osc133CommandInputStart,
    /// `\e]133;C\e\\` — command is running.
    Osc133CommandStart,
    /// `\e]133;D[;<exit>]\e\\` — command finished, optional exit code.
    Osc133CommandDone(Option<i32>),
    /// `\e]7;file://<host><path>\e\\` — cwd changed.
    Osc7Cwd(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    /// No active parse; scan for `ESC`.
    Idle,
    /// Saw `ESC`; the next byte determines whether this is OSC, CSI,
    /// or something else.
    SawEsc,
    /// Inside an OSC sequence body, collecting bytes into the buffer
    /// until we hit `BEL` or `ESC \\`.
    InOsc,
    /// Inside OSC and just saw `ESC`; if next byte is `\\` we have a
    /// ST terminator.
    InOscSawEsc,
}

/// Streaming OSC parser. Feed bytes one at a time via [`observe`]; the
/// method returns an event when a complete `OSC … terminator` framing
/// has been seen. Garbage between sequences is silently skipped.
#[derive(Debug)]
pub struct OscParser {
    state: ParseState,
    /// Bytes accumulated inside the current OSC (excluding the
    /// `ESC ]` introducer and any terminator).
    buf: Vec<u8>,
}

impl OscParser {
    pub fn new() -> Self {
        Self {
            state: ParseState::Idle,
            buf: Vec::with_capacity(64),
        }
    }

    pub fn observe(&mut self, byte: u8) -> Option<OscEvent> {
        match self.state {
            ParseState::Idle => {
                if byte == 0x1b {
                    self.state = ParseState::SawEsc;
                }
                None
            }
            ParseState::SawEsc => {
                if byte == b']' {
                    self.state = ParseState::InOsc;
                    self.buf.clear();
                } else if byte == 0x1b {
                    // ESC ESC — stay in SawEsc waiting for next byte.
                } else {
                    // Some other escape sequence (CSI, plain ESC, …)
                    // that we don't care about.
                    self.state = ParseState::Idle;
                }
                None
            }
            ParseState::InOsc => match byte {
                0x07 => self.finish(),
                0x1b => {
                    self.state = ParseState::InOscSawEsc;
                    None
                }
                _ => {
                    self.push(byte);
                    None
                }
            },
            ParseState::InOscSawEsc => match byte {
                b'\\' => self.finish(),
                0x1b => {
                    // First ESC was part of OSC body; this ESC may
                    // begin the real ST. Push the previous one.
                    self.push(0x1b);
                    None
                }
                _ => {
                    // The previous ESC was OSC body content too.
                    self.push(0x1b);
                    self.push(byte);
                    self.state = ParseState::InOsc;
                    None
                }
            },
        }
    }

    fn push(&mut self, byte: u8) {
        if self.buf.len() >= MAX_OSC_PAYLOAD {
            // Bail out — never let a malformed stream grow `buf`
            // unboundedly.
            self.buf.clear();
            self.state = ParseState::Idle;
            return;
        }
        self.buf.push(byte);
    }

    fn finish(&mut self) -> Option<OscEvent> {
        let event = parse_osc_payload(&self.buf);
        self.buf.clear();
        self.state = ParseState::Idle;
        event
    }
}

impl Default for OscParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse the bytes between `ESC ]` and the terminator. Format is
/// `<code> ; <params>` (UTF-8 by convention).
fn parse_osc_payload(buf: &[u8]) -> Option<OscEvent> {
    let sep = buf.iter().position(|&b| b == b';')?;
    let code_bytes = &buf[..sep];
    let params = &buf[sep + 1..];
    let code = std::str::from_utf8(code_bytes).ok()?;
    match code {
        "133" => parse_osc_133(params),
        "7" => parse_osc_7(params),
        _ => None,
    }
}

fn parse_osc_133(params: &[u8]) -> Option<OscEvent> {
    let first = *params.first()?;
    match first {
        b'A' if params.len() == 1 => Some(OscEvent::Osc133PromptStart),
        b'B' if params.len() == 1 => Some(OscEvent::Osc133CommandInputStart),
        b'C' if params.len() == 1 => Some(OscEvent::Osc133CommandStart),
        b'D' => {
            let exit = if params.len() == 1 {
                None
            } else if params[1] == b';' {
                let s = std::str::from_utf8(&params[2..]).ok()?;
                s.parse::<i32>().ok()
            } else {
                return None;
            };
            Some(OscEvent::Osc133CommandDone(exit))
        }
        _ => None,
    }
}

fn parse_osc_7(params: &[u8]) -> Option<OscEvent> {
    let s = std::str::from_utf8(params).ok()?;
    let after_scheme = s.strip_prefix("file://").unwrap_or(s);
    let path = match after_scheme.find('/') {
        Some(i) => after_scheme[i..].to_string(),
        None => after_scheme.to_string(),
    };
    Some(OscEvent::Osc7Cwd(path))
}

/// PTY master → host stdout. EOF means the wrapped shell exited.
///
/// Every byte received is unconditionally forwarded to stdout; the OSC
/// parser observes the stream in parallel and updates `state` on each
/// recognized sequence.
pub fn run_pty_reader(mut reader: Box<dyn Read + Send>, state: SharedState) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    let mut parser = OscParser::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                debug!("pty-reader: EOF, shell exited");
                return Ok(());
            }
            Ok(n) => {
                let chunk = &buf[..n];
                stdout.write_all(chunk)?;
                stdout.flush()?;
                for &byte in chunk {
                    if let Some(event) = parser.observe(byte) {
                        apply_event(&state, event);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

fn apply_event(state: &SharedState, event: OscEvent) {
    match event {
        OscEvent::Osc133PromptStart => {
            state.on_prompt_start();
            trace!("shell-state: AtPrompt (OSC 133 A)");
        }
        OscEvent::Osc133CommandInputStart => {
            state.on_command_input_marker();
            trace!("shell-state: command-input marker (OSC 133 B)");
        }
        OscEvent::Osc133CommandStart => {
            state.on_command_start();
            trace!("shell-state: CommandRunning (OSC 133 C)");
        }
        OscEvent::Osc133CommandDone(exit) => {
            state.on_command_done(exit);
            trace!(?exit, "shell-state: command done (OSC 133 D)");
        }
        OscEvent::Osc7Cwd(cwd) => {
            trace!(cwd = %cwd, "shell-state: cwd updated (OSC 7)");
            state.on_cwd(cwd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(parser: &mut OscParser, bytes: &[u8]) -> Vec<OscEvent> {
        let mut events = Vec::new();
        for &b in bytes {
            if let Some(e) = parser.observe(b) {
                events.push(e);
            }
        }
        events
    }

    #[test]
    fn osc_133_a_with_st_terminator() {
        let mut p = OscParser::new();
        let events = feed(&mut p, b"\x1b]133;A\x1b\\");
        assert_eq!(events, vec![OscEvent::Osc133PromptStart]);
    }

    #[test]
    fn osc_133_a_with_bel_terminator() {
        let mut p = OscParser::new();
        let events = feed(&mut p, b"\x1b]133;A\x07");
        assert_eq!(events, vec![OscEvent::Osc133PromptStart]);
    }

    #[test]
    fn osc_133_d_with_exit_code() {
        let mut p = OscParser::new();
        let events = feed(&mut p, b"\x1b]133;D;42\x1b\\");
        assert_eq!(events, vec![OscEvent::Osc133CommandDone(Some(42))]);
    }

    #[test]
    fn osc_133_d_without_exit_code() {
        let mut p = OscParser::new();
        let events = feed(&mut p, b"\x1b]133;D\x1b\\");
        assert_eq!(events, vec![OscEvent::Osc133CommandDone(None)]);
    }

    #[test]
    fn osc_7_cwd() {
        let mut p = OscParser::new();
        let events = feed(&mut p, b"\x1b]7;file://localhost/tmp/foo\x1b\\");
        assert_eq!(events, vec![OscEvent::Osc7Cwd("/tmp/foo".to_string())]);
    }

    #[test]
    fn osc_7_cwd_with_empty_host() {
        let mut p = OscParser::new();
        let events = feed(&mut p, b"\x1b]7;file:///tmp/foo\x1b\\");
        assert_eq!(events, vec![OscEvent::Osc7Cwd("/tmp/foo".to_string())]);
    }

    #[test]
    fn osc_split_across_buffers() {
        let mut p = OscParser::new();
        let mut events = feed(&mut p, b"\x1b]133");
        events.extend(feed(&mut p, b";A\x1b"));
        events.extend(feed(&mut p, b"\\"));
        assert_eq!(events, vec![OscEvent::Osc133PromptStart]);
    }

    #[test]
    fn garbage_between_oscs_is_ignored() {
        let mut p = OscParser::new();
        let events = feed(
            &mut p,
            b"hello world\x1b]133;A\x1b\\some text\x1b]133;C\x07tail",
        );
        assert_eq!(
            events,
            vec![OscEvent::Osc133PromptStart, OscEvent::Osc133CommandStart,]
        );
    }

    #[test]
    fn unknown_osc_code_is_dropped() {
        let mut p = OscParser::new();
        let events = feed(&mut p, b"\x1b]9001;hello\x1b\\");
        assert!(events.is_empty());
    }

    #[test]
    fn unknown_133_subcode_is_dropped() {
        let mut p = OscParser::new();
        let events = feed(&mut p, b"\x1b]133;Z\x1b\\");
        assert!(events.is_empty());
    }

    #[test]
    fn malformed_oversize_buffer_does_not_panic() {
        let mut p = OscParser::new();
        let mut payload = b"\x1b]133;A".to_vec();
        payload.extend(std::iter::repeat_n(b'x', MAX_OSC_PAYLOAD * 2));
        payload.extend(b"\x1b\\");
        let _ = feed(&mut p, &payload);
        // Parser should still accept a fresh OSC after recovery.
        let events = feed(&mut p, b"\x1b]133;A\x1b\\");
        assert_eq!(events, vec![OscEvent::Osc133PromptStart]);
    }

    #[test]
    fn esc_in_params_is_recovered() {
        // ESC followed by a non-`\` byte: the first ESC is body content.
        // After it, the parser should keep accumulating.
        let mut p = OscParser::new();
        // \e]7;file:///\eX\e\\  -- a stray ESC then 'X' then ST.
        let events = feed(&mut p, b"\x1b]7;file:///\x1bX\x1b\\");
        // This should parse OSC 7 with path "/\x1bX" — slightly odd
        // but the parser shouldn't drop the whole sequence.
        match events.as_slice() {
            [OscEvent::Osc7Cwd(_)] => {}
            other => panic!("expected one Osc7Cwd event, got {other:?}"),
        }
    }

    /// Full Phase 3 lifecycle through parser + apply_event + state.
    /// Simulates what a real zsh+integration would emit during one
    /// prompt → command → exit cycle, and checks the SharedState
    /// reflects each transition.
    #[test]
    fn parser_drives_state_through_full_lifecycle() {
        use crate::state::{SharedState, ShellState};
        let state = SharedState::new();
        let mut parser = OscParser::new();
        assert_eq!(state.current_state(), ShellState::PrePrompt);

        // Prompt being drawn: A then B (inside PROMPT). Mixed with the
        // prompt text the shell itself emits to stdout.
        for &b in b"\x1b]133;A\x1b\\zsh-test% \x1b]133;B\x1b\\" {
            if let Some(e) = parser.observe(b) {
                apply_event(&state, e);
            }
        }
        assert_eq!(state.current_state(), ShellState::AtPrompt);

        // User pressed Enter; OSC 7 cwd report fires first (preexec hooks
        // often update cwd), then C marks command execution.
        for &b in b"\x1b]7;file://localhost/tmp/work\x1b\\\x1b]133;C\x1b\\" {
            if let Some(e) = parser.observe(b) {
                apply_event(&state, e);
            }
        }
        assert_eq!(state.current_state(), ShellState::CommandRunning);
        assert_eq!(state.current_cwd().as_deref(), Some("/tmp/work"));

        // Command finished with exit 7; D reports it. State returns to
        // PrePrompt (next prompt's A will move us back to AtPrompt).
        for &b in b"\x1b]133;D;7\x1b\\" {
            if let Some(e) = parser.observe(b) {
                apply_event(&state, e);
            }
        }
        assert_eq!(state.current_state(), ShellState::PrePrompt);
        assert_eq!(state.last_exit(), Some(7));

        // Second prompt cycle confirms we cleanly loop.
        for &b in b"\x1b]133;A\x1b\\" {
            if let Some(e) = parser.observe(b) {
                apply_event(&state, e);
            }
        }
        assert_eq!(state.current_state(), ShellState::AtPrompt);
    }
}
