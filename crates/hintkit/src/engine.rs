//! Suggestion engine plumbing (SPEC §7 Phase 2 skeleton; Phase 5 fills in
//! the real engine).
//!
//! Two threads, two bounded channels, drop-on-full semantics throughout
//! — the architectural property that prevents the Kiro paste-crash
//! failure mode (SPEC §4 "Threading + paste, the failure mode to
//! specifically prevent").
//!
//! ```text
//! stdin-writer ── bounded(1, drop-on-full) ──▶ debouncer
//!                                                  │
//!                                                  │ 30 ms quiet
//!                                                  ▼
//!                              bounded(16, drop-on-full) ──▶ suggestion-stub
//! ```

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use crossbeam_channel::{after, select, Receiver, Sender, TrySendError};
use hintkit_parser::{match_suggestions, parse_context, tokenize, Suggestion, SuggestionKind};
use hintkit_specs_bundled::SpecDb;
use tracing::{debug, trace};

use crate::generators;
use crate::state::{SharedState, ShellState};

/// Cap on how many suggestions the engine emits per tick. Larger
/// lists are not useful (the popup will only show ~5 anyway in
/// Phase 6) and they slow ranking + rendering.
const MAX_SUGGESTIONS: usize = 16;

/// Quiet window before the debouncer emits an [`Event::InputChanged`].
/// SPEC §4 specifies 30–50 ms; we sit at the low end of that range so
/// suggestions feel "live" without churning on every keystroke.
const DEBOUNCE_QUIET: Duration = Duration::from_millis(30);

/// Capacity of the debouncer-tick channel between [`crate::input`] and
/// [`run_debouncer`]. One slot is sufficient: the debouncer treats any
/// pending tick the same, so coalescing into a single slot is free.
pub const TICK_CHANNEL_CAPACITY: usize = 1;

/// Capacity of the change-event channel between [`run_debouncer`] and the
/// suggestion consumer. SPEC §4 calls for 16. Drop-on-full when the
/// consumer is slow — better to lose redundant "changed" notifications
/// than to back-pressure the I/O thread.
pub const EVENT_CHANNEL_CAPACITY: usize = 16;

/// Messages flowing from the debouncer to the suggestion engine. Phase 2
/// uses the marker variant only; Phase 5 will attach a snapshot of the
/// current command-line state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The user's command line may have changed; re-parse and re-suggest.
    InputChanged,
}

/// Read ticks, coalesce bursts within [`DEBOUNCE_QUIET`], emit one
/// [`Event::InputChanged`] per quiet window. Exits when `tick_rx`
/// disconnects (the upstream stdin-writer dropped its sender).
pub fn run_debouncer(tick_rx: Receiver<()>, events_tx: Sender<Event>) -> Result<()> {
    loop {
        // Wait for the first tick of a new burst. Disconnected means
        // graceful shutdown — drop our sender and let the suggestion
        // consumer exit too.
        if tick_rx.recv().is_err() {
            return Ok(());
        }

        // Coalesce: stay here as long as ticks keep arriving within
        // DEBOUNCE_QUIET of each other.
        loop {
            select! {
                recv(tick_rx) -> r => {
                    if r.is_err() {
                        // Upstream gone; flush one last event so the
                        // consumer sees the final state, then exit.
                        let _ = events_tx.try_send(Event::InputChanged);
                        return Ok(());
                    }
                },
                recv(after(DEBOUNCE_QUIET)) -> _ => break,
            }
        }

        match events_tx.try_send(Event::InputChanged) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                trace!("input-changed dropped: consumer backlogged");
            }
            Err(TrySendError::Disconnected(_)) => return Ok(()),
        }
    }
}

/// Suggestion thread main loop (Phase 5b wiring).
///
/// On each `InputChanged` event:
/// 1. Skip if the shell isn't `AtPrompt` (no point suggesting while a
///    command is running or no prompt is on screen).
/// 2. Snapshot the current line + cursor + cwd from `SharedState`.
/// 3. Tokenize, look up the first token in the bundled `SpecDb`. No
///    spec → no suggestions (we don't have anything to match against).
/// 4. Walk the parse context, get ranked candidates from the matcher.
/// 5. For any `GeneratedValue(kind)` suggestions, expand via the
///    native generator allowlist and inline the results in place of
///    the placeholder. File / dir / git_branches / package_json_scripts
///    only — anything else from the spec was already filtered out by
///    the ingest pipeline.
/// 6. Trace the top N under the `debug` feature. Phase 6 wires this
///    list to the renderer.
pub fn run_suggestion_stub(events_rx: Receiver<Event>, state: SharedState) -> Result<()> {
    let mut received: u64 = 0;
    let db = SpecDb::global();
    while let Ok(_event) = events_rx.recv() {
        received = received.wrapping_add(1);
        let shell_state = state.current_state();
        if shell_state != ShellState::AtPrompt {
            trace!(
                ?shell_state,
                received,
                "skipping suggestion compute: shell not AtPrompt"
            );
            continue;
        }

        let (line, cursor) = state.current_line();
        if line.is_empty() {
            continue;
        }
        let cwd = state
            .current_cwd()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        let suggestions = compute_suggestions(db, &line, cursor, &cwd);
        if suggestions.is_empty() {
            continue;
        }
        // Phase 5b: visibility-only. Phase 6 hands this list to the
        // renderer for the on-screen popup.
        let preview: Vec<String> = suggestions.iter().take(5).map(|s| s.text.clone()).collect();
        trace!(
            received,
            line = %line,
            cursor,
            total = suggestions.len(),
            preview = ?preview,
            "suggestions ready (top 5 shown)"
        );
    }
    debug!(received, "suggestion thread exiting");
    Ok(())
}

/// Pure helper exposed for testing: take a line + cursor + cwd, return
/// the ranked suggestion list. No side effects beyond reading the
/// filesystem (for `file_path` etc.) and possibly spawning a 200 ms-
/// capped subprocess (for `git_branches`).
pub fn compute_suggestions(
    db: &SpecDb,
    line: &str,
    cursor: usize,
    cwd: &std::path::Path,
) -> Vec<Suggestion> {
    let tokenized = tokenize(line, cursor);
    let command_name = match tokenized.tokens.first() {
        Some(t) => t.text,
        None => return Vec::new(),
    };
    let Some(spec) = db.lookup(command_name) else {
        return Vec::new();
    };
    let ctx = parse_context(&tokenized, &spec);
    let prefix = tokenized.cursor_prefix(line);
    let raw = match_suggestions(&ctx, prefix);

    // Expand any GeneratedValue placeholders by invoking the native
    // generators. Capped at MAX_SUGGESTIONS to keep the list usable.
    let mut out: Vec<Suggestion> = Vec::new();
    for s in raw {
        if out.len() >= MAX_SUGGESTIONS {
            break;
        }
        match s.kind {
            SuggestionKind::GeneratedValue(kind) => {
                for value in generators::resolve(kind, cwd) {
                    if !value.starts_with(prefix) {
                        continue;
                    }
                    out.push(Suggestion {
                        text: value,
                        description: s.description.clone(),
                        kind: SuggestionKind::StaticArg,
                    });
                    if out.len() >= MAX_SUGGESTIONS {
                        break;
                    }
                }
            }
            _ => out.push(s),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn debouncer_coalesces_burst_into_single_event() {
        let (tick_tx, tick_rx) = bounded(TICK_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = bounded(EVENT_CHANNEL_CAPACITY);

        let handle = thread::spawn(move || run_debouncer(tick_rx, event_tx));

        // Fire 50 ticks in a tight loop (much faster than the 30 ms
        // window). They must coalesce into one event.
        for _ in 0..50 {
            let _ = tick_tx.try_send(());
        }

        // After ~30 ms of quiet, one event should arrive.
        let start = Instant::now();
        let event = event_rx
            .recv_timeout(Duration::from_millis(200))
            .expect("debouncer should emit within 200 ms");
        assert_eq!(event, Event::InputChanged);
        assert!(
            start.elapsed() >= Duration::from_millis(20),
            "debouncer fired before the quiet window completed"
        );

        // No further event should fire — the burst was a single one.
        assert!(event_rx.recv_timeout(Duration::from_millis(80)).is_err());

        drop(tick_tx);
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn debouncer_exits_when_ticks_disconnect() {
        let (tick_tx, tick_rx) = bounded::<()>(TICK_CHANNEL_CAPACITY);
        let (event_tx, _event_rx) = bounded(EVENT_CHANNEL_CAPACITY);
        let handle = thread::spawn(move || run_debouncer(tick_rx, event_tx));
        drop(tick_tx);
        handle
            .join()
            .expect("debouncer thread panicked")
            .expect("debouncer returned error");
    }

    #[test]
    fn separate_bursts_yield_separate_events() {
        let (tick_tx, tick_rx) = bounded(TICK_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = bounded(EVENT_CHANNEL_CAPACITY);
        let handle = thread::spawn(move || run_debouncer(tick_rx, event_tx));

        let _ = tick_tx.try_send(());
        let first = event_rx
            .recv_timeout(Duration::from_millis(200))
            .expect("first event");
        assert_eq!(first, Event::InputChanged);

        // Wait out the quiet window, then fire a second burst.
        thread::sleep(Duration::from_millis(60));
        let _ = tick_tx.try_send(());
        let second = event_rx
            .recv_timeout(Duration::from_millis(200))
            .expect("second event");
        assert_eq!(second, Event::InputChanged);

        drop(tick_tx);
        handle.join().unwrap().unwrap();
    }

    /// End-to-end pipeline test against the real bundled `git` spec.
    /// Proves: line → tokenize → SpecDb lookup → parse_context → match
    /// produces sensible suggestions for the partial input most
    /// real-world v0.1 users hit.
    #[test]
    fn compute_suggestions_against_bundled_git_spec() {
        let db = SpecDb::global();
        let cwd = std::env::temp_dir();

        // `git c` — should suggest subcommands starting with c (checkout,
        // commit, clone, …).
        let suggestions = compute_suggestions(db, "git c", 5, &cwd);
        let names: Vec<&str> = suggestions.iter().map(|s| s.text.as_str()).collect();
        assert!(
            names.contains(&"checkout"),
            "expected `checkout` in suggestions for `git c`, got {names:?}"
        );
        assert!(
            names.contains(&"commit"),
            "expected `commit` in suggestions for `git c`, got {names:?}"
        );

        // `git ` (trailing space) — should suggest *all* subcommands.
        let suggestions = compute_suggestions(db, "git ", 4, &cwd);
        assert!(
            suggestions.len() > 10,
            "git's bundled spec has >10 subcommands; got {} suggestions",
            suggestions.len()
        );
    }

    #[test]
    fn compute_suggestions_unknown_command_yields_empty() {
        let db = SpecDb::global();
        let cwd = std::env::temp_dir();
        let suggestions = compute_suggestions(db, "this-tool-does-not-exist ", 25, &cwd);
        assert!(suggestions.is_empty());
    }
}
