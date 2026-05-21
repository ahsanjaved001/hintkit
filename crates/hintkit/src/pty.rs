//! Minimum-viable PTY wrapper (SPEC §7 Phase 1).
//!
//! Opens a pseudo-terminal, spawns the user's `$SHELL` on the slave side,
//! and bidirectionally forwards bytes between the host terminal and the
//! shell. Handles SIGWINCH by propagating the new size to the PTY. Restores
//! terminal state on every exit path (including panics) via [`RawModeGuard`].
//!
//! This module deliberately contains no suggestion logic, no rendering, and
//! no parsing — it must be a transparent passthrough before any other layer
//! is built on top.

use std::env;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::thread;

use anyhow::{anyhow, Context, Result};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use signal_hook::consts::SIGWINCH;
use signal_hook::iterator::Signals;

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
    let mut writer = master.take_writer().context("taking pty master writer")?;

    // Raw mode must be entered AFTER spawn_command + open so any failure
    // above leaves the host terminal cooked. The guard restores cooked
    // mode on drop / unwind.
    let _raw = RawModeGuard::enable()?;

    // `master` is `Box<dyn MasterPty + Send>` (not Sync), so it's moved
    // into the SIGWINCH thread which is the only remaining owner. The
    // reader/writer above are independent handles that don't share state
    // with `master` once extracted.
    install_sigwinch_handler(master)?;

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

    // Host stdin → PTY master. Daemonized: the read blocks indefinitely,
    // so we don't try to join it on shutdown — the OS reclaims it when
    // the process exits.
    thread::Builder::new()
        .name("hintkit-stdin-writer".into())
        .spawn(move || -> io::Result<()> {
            let mut stdin = io::stdin().lock();
            let mut buf = [0u8; 8192];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => return Ok(()),
                    Ok(n) => {
                        writer.write_all(&buf[..n])?;
                        writer.flush()?;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
        })
        .context("spawning stdin-writer thread")?;

    let status = child.wait().context("waiting on shell process")?;
    // Wait for the reader thread to drain remaining output before we drop
    // the master and tear down raw mode — otherwise the user loses the
    // last few bytes (typically the prompt redraw before exit).
    let _ = reader_thread.join();

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

/// Restores cooked-mode on drop. Held for the lifetime of [`run`] so any
/// panic-unwind path also restores terminal state (SPEC §9).
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Result<Self> {
        enable_raw_mode().context("enabling raw mode on host terminal")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // Best-effort: if disabling fails, the user's terminal will be
        // wedged anyway and there's nothing useful we can print here.
        let _ = disable_raw_mode();
    }
}
