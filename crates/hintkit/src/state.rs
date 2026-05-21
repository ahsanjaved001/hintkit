//! Shared shell-lifecycle state (SPEC §7 Phase 3).
//!
//! Tracks where the wrapped shell is in its prompt → command → exit
//! cycle, based on OSC 133 markers emitted by the shell integration
//! script. The state lives behind an `Arc<Mutex<…>>` so the pty-reader
//! thread can update it while the suggestion engine (Phase 5) reads it
//! to decide whether to fire.
//!
//! v0.1 does *not* expose the state to any other thread yet — it's
//! observed (via `tracing` under the `debug` feature) but the
//! suggestion stub doesn't gate on it. Phase 5 wires the gate.

use std::sync::{Arc, Mutex};

/// Where the wrapped shell is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellState {
    /// Wrapper started but the shell hasn't drawn its first prompt yet.
    PrePrompt,
    /// A prompt is on screen; the user may be typing into it.
    AtPrompt,
    /// User pressed Enter; a command is executing.
    CommandRunning,
}

#[derive(Debug)]
struct Inner {
    state: ShellState,
    cwd: Option<String>,
    /// Exit code of the most-recently completed command. `None` before
    /// any command has finished.
    last_exit: Option<i32>,
}

/// Cheaply-cloneable handle to the shared shell-lifecycle state.
#[derive(Debug, Clone)]
pub struct SharedState {
    inner: Arc<Mutex<Inner>>,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                state: ShellState::PrePrompt,
                cwd: None,
                last_exit: None,
            })),
        }
    }

    pub fn current_state(&self) -> ShellState {
        self.inner.lock().expect("SharedState mutex poisoned").state
    }

    pub fn current_cwd(&self) -> Option<String> {
        self.inner
            .lock()
            .expect("SharedState mutex poisoned")
            .cwd
            .clone()
    }

    pub fn last_exit(&self) -> Option<i32> {
        self.inner
            .lock()
            .expect("SharedState mutex poisoned")
            .last_exit
    }

    /// OSC 133 A — prompt is starting. Always transitions to AtPrompt.
    pub fn on_prompt_start(&self) {
        self.inner.lock().expect("mutex poisoned").state = ShellState::AtPrompt;
    }

    /// OSC 133 B — end-of-prompt / start-of-input marker. Doesn't
    /// transition state; both A and B leave us in AtPrompt. We accept
    /// it without complaint so a shell that emits only B (no A — non-
    /// conforming but observed in the wild) still ends up AtPrompt.
    pub fn on_command_input_marker(&self) {
        let mut inner = self.inner.lock().expect("mutex poisoned");
        if inner.state == ShellState::PrePrompt {
            inner.state = ShellState::AtPrompt;
        }
    }

    /// OSC 133 C — command is running.
    pub fn on_command_start(&self) {
        self.inner.lock().expect("mutex poisoned").state = ShellState::CommandRunning;
    }

    /// OSC 133 D — command finished. Records exit code, returns to
    /// PrePrompt (we'll re-enter AtPrompt on the next A).
    pub fn on_command_done(&self, exit_code: Option<i32>) {
        let mut inner = self.inner.lock().expect("mutex poisoned");
        inner.state = ShellState::PrePrompt;
        if let Some(code) = exit_code {
            inner.last_exit = Some(code);
        }
    }

    /// OSC 7 — cwd changed.
    pub fn on_cwd(&self, cwd: impl Into<String>) {
        self.inner.lock().expect("mutex poisoned").cwd = Some(cwd.into());
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_state_is_pre_prompt() {
        let s = SharedState::new();
        assert_eq!(s.current_state(), ShellState::PrePrompt);
        assert!(s.current_cwd().is_none());
        assert!(s.last_exit().is_none());
    }

    #[test]
    fn full_lifecycle_a_b_c_d() {
        let s = SharedState::new();
        s.on_prompt_start();
        assert_eq!(s.current_state(), ShellState::AtPrompt);
        s.on_command_input_marker();
        assert_eq!(s.current_state(), ShellState::AtPrompt);
        s.on_command_start();
        assert_eq!(s.current_state(), ShellState::CommandRunning);
        s.on_command_done(Some(0));
        assert_eq!(s.current_state(), ShellState::PrePrompt);
        assert_eq!(s.last_exit(), Some(0));
    }

    #[test]
    fn lone_b_marker_promotes_pre_prompt_to_at_prompt() {
        let s = SharedState::new();
        s.on_command_input_marker();
        assert_eq!(s.current_state(), ShellState::AtPrompt);
    }

    #[test]
    fn cwd_updates_persist() {
        let s = SharedState::new();
        s.on_cwd("/tmp");
        assert_eq!(s.current_cwd().as_deref(), Some("/tmp"));
        s.on_cwd("/home/test");
        assert_eq!(s.current_cwd().as_deref(), Some("/home/test"));
    }
}
