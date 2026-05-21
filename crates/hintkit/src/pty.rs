//! PTY wrapper orchestration (SPEC §7 Phase 1 + Phase 2).
//!
//! Opens a pseudo-terminal, spawns the user's `$SHELL` on the slave side,
//! and wires up the I/O thread topology described in SPEC §4:
//!
//! ```text
//!   pty-reader  : master pty  ──▶ host stdout
//!   stdin-writer: host stdin  ──▶ master pty   (also feeds the debouncer)
//!   debouncer   : ticks       ──▶ InputChanged (30 ms quiet window)
//!   suggestion  : InputChanged ──▶ (stub for now; Phase 5 fills it in)
//!   sigwinch    : SIGWINCH    ──▶ master pty resize
//! ```
//!
//! All non-I/O threads communicate via `crossbeam-channel` with bounded,
//! drop-on-full semantics (SPEC §4 "Threading + paste, the failure mode
//! to specifically prevent"). Raw mode and bracketed-paste mode are
//! installed via [`RawModeGuard`] so any panic-unwind path restores the
//! terminal (SPEC §9 "Do not break the terminal").

use std::env;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::thread;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::bounded;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use signal_hook::consts::SIGWINCH;
use signal_hook::iterator::Signals;
use tracing::{debug, trace, warn};

use crate::engine::{self, EVENT_CHANNEL_CAPACITY, TICK_CHANNEL_CAPACITY};
use crate::input;

/// CSI sequence that asks the terminal to enable bracketed-paste mode.
/// Once enabled, pasted text is wrapped in `ESC [ 200 ~` / `ESC [ 201 ~`.
const BRACKETED_PASTE_ENABLE: &[u8] = b"\x1b[?2004h";
/// CSI sequence that disables bracketed-paste mode.
const BRACKETED_PASTE_DISABLE: &[u8] = b"\x1b[?2004l";

/// Entry point for the wrapper. Returns the wrapped shell's exit code.
pub fn run() -> Result<i32> {
    let shell = detect_shell()?;
    let (cols, rows) = current_terminal_size()?;

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("opening pty pair")?;

    let cmd = build_shell_command(&shell);
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .context("spawning shell on pty slave")?;

    // The child holds the slave fd internally; drop our handle so the
    // shell is the only owner. Without this, the master read never sees
    // EOF when the shell exits.
    drop(pair.slave);

    let master = pair.master;
    let mut reader = master
        .try_clone_reader()
        .context("cloning pty master reader")?;
    let writer = master.take_writer().context("taking pty master writer")?;

    // Raw mode must be entered AFTER spawn_command + open so any failure
    // above leaves the host terminal cooked. The guard restores cooked
    // mode + disables bracketed paste on drop / unwind.
    let _raw = RawModeGuard::enable()?;

    // `master` is `Box<dyn MasterPty + Send>` (not Sync), so it's moved
    // into the SIGWINCH thread which is the only remaining owner. The
    // reader/writer above are independent handles that don't share state
    // with `master` once extracted.
    install_sigwinch_handler(master)?;

    // Bounded channels with drop-on-full semantics, capacities per SPEC §4.
    let (tick_tx, tick_rx) = bounded::<()>(TICK_CHANNEL_CAPACITY);
    let (event_tx, event_rx) = bounded::<engine::Event>(EVENT_CHANNEL_CAPACITY);

    let debouncer_thread = thread::Builder::new()
        .name("hintkit-debouncer".into())
        .spawn(move || engine::run_debouncer(tick_rx, event_tx))
        .context("spawning debouncer thread")?;

    let suggestion_thread = thread::Builder::new()
        .name("hintkit-suggestion".into())
        .spawn(move || engine::run_suggestion_stub(event_rx))
        .context("spawning suggestion-stub thread")?;

    // PTY master → host stdout. EOF here means the shell exited.
    let reader_thread = thread::Builder::new()
        .name("hintkit-pty-reader".into())
        .spawn(move || -> io::Result<()> {
            let mut stdout = io::stdout().lock();
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => return Ok(()),
                    Ok(n) => {
                        stdout.write_all(&buf[..n])?;
                        stdout.flush()?;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
        })
        .context("spawning pty-reader thread")?;

    // Host stdin → PTY master + debouncer-tick channel. Daemonized: the
    // stdin read blocks indefinitely, so we never join it — the OS
    // reclaims it on process exit.
    thread::Builder::new()
        .name("hintkit-stdin-writer".into())
        .spawn(move || {
            if let Err(e) = input::run_stdin_writer(writer, tick_tx) {
                warn!("stdin-writer thread exited with error: {e:#}");
            }
        })
        .context("spawning stdin-writer thread")?;

    let status = child.wait().context("waiting on shell process")?;
    // Drain remaining shell output before we tear down raw mode — otherwise
    // the user loses the last few bytes (typically the prompt redraw).
    let _ = reader_thread.join();

    // The debouncer + suggestion threads are daemonized for the same
    // reason as stdin-writer (their channels can stay live until process
    // teardown). join_handles are dropped here without joining; the OS
    // reclaims them. We log thread handle status under debug only.
    drop(debouncer_thread);
    drop(suggestion_thread);

    Ok(status.exit_code() as i32)
}

/// Resolve the shell to spawn. Prefers `$SHELL`; falls back to `/bin/sh`,
/// which POSIX guarantees exists. Refuses an empty `$SHELL` rather than
/// silently mis-routing.
fn detect_shell() -> Result<PathBuf> {
    if let Some(s) = env::var_os("SHELL") {
        if !s.is_empty() {
            return Ok(PathBuf::from(s));
        }
    }
    let fallback = PathBuf::from("/bin/sh");
    if fallback.exists() {
        return Ok(fallback);
    }
    Err(anyhow!(
        "could not determine shell: $SHELL is unset and /bin/sh is missing"
    ))
}

fn current_terminal_size() -> Result<(u16, u16)> {
    crossterm::terminal::size().context("querying terminal size")
}

/// Build the shell command, inheriting the parent process environment.
/// `portable_pty::CommandBuilder` defaults to a *clean* environment;
/// that breaks PATH, HOME, TERM, etc. — copy them through explicitly.
fn build_shell_command(shell: &PathBuf) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(shell);
    for (k, v) in env::vars_os() {
        cmd.env(k, v);
    }
    // Mark child shells as running under hintkit. Shell integration scripts
    // (Phase 3) will key off this; for now it's a no-op observable signal.
    cmd.env("HINTKIT_WRAPPED", "1");
    cmd
}

fn install_sigwinch_handler(master: Box<dyn MasterPty + Send>) -> Result<()> {
    let mut signals = Signals::new([SIGWINCH]).context("registering SIGWINCH handler")?;
    thread::Builder::new()
        .name("hintkit-sigwinch".into())
        .spawn(move || {
            for _ in signals.forever() {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
        })
        .context("spawning sigwinch thread")?;
    Ok(())
}

/// Restores cooked-mode and disables bracketed paste on drop. Held for the
/// lifetime of [`run`] so any panic-unwind path also restores terminal
/// state (SPEC §9 "Do not break the terminal").
///
/// Order matters: on enable we set raw mode first then opt the terminal
/// into bracketed paste; on drop we reverse — disable bracketed paste,
/// then drop raw mode — so any failure mid-teardown still leaves the user
/// in a saner state than if we did it the other way.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Result<Self> {
        enable_raw_mode().context("enabling raw mode on host terminal")?;
        // Best-effort: a terminal that doesn't understand `\e[?2004h`
        // simply prints garbage or silently ignores the CSI; either way
        // we shouldn't fail wrapper startup over it.
        let mut stdout = io::stdout().lock();
        if let Err(e) = stdout.write_all(BRACKETED_PASTE_ENABLE) {
            warn!("could not enable bracketed paste: {e}");
        }
        let _ = stdout.flush();
        trace!("raw mode + bracketed paste enabled");
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // Best-effort cleanup on every exit path. If any of these fail,
        // the user's terminal will be wedged anyway and there's nothing
        // useful we can print at this point.
        {
            let mut stdout = io::stdout().lock();
            let _ = stdout.write_all(BRACKETED_PASTE_DISABLE);
            let _ = stdout.flush();
        }
        let _ = disable_raw_mode();
        debug!("raw mode + bracketed paste disabled");
    }
}
