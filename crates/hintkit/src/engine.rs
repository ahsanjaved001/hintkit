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

use std::time::Duration;

use anyhow::Result;
use crossbeam_channel::{after, select, Receiver, Sender, TrySendError};
use tracing::{debug, trace};

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

/// Phase 2 stub. Receives change events and discards them. Phase 5 will
/// replace this with parser-driven suggestion lookup. Exists now so the
/// channel has a real consumer (otherwise `try_send` would always
/// `Disconnected`-error on the first send).
pub fn run_suggestion_stub(events_rx: Receiver<Event>) -> Result<()> {
    let mut received: u64 = 0;
    while let Ok(_event) = events_rx.recv() {
        received = received.wrapping_add(1);
        // Phase 5 will do real parsing/spec-lookup work here.
    }
    debug!(received, "suggestion-stub thread exiting");
    Ok(())
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
}
