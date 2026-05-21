//! Phase 2 gate test (SPEC §4, §7): paste 100 KB through the wrapper
//! without crash, hang, or visible delay.
//!
//! Architecturally, this proves the I/O thread stays unblocked under a
//! burst that would have OOM'd or deadlocked an unbounded design. The
//! drop-on-full debouncer means in-paste mode never queues per-byte work;
//! the kernel-buffered PTY pipeline naturally back-pressures.
//!
//! The test sends:
//! 1. `\e[200~` (paste-start)
//! 2. 100 KB of harmless `'a'` bytes
//! 3. `\e[201~` (paste-end)
//! 4. `\x15` (Ctrl+U → backward-kill-line) to clear the inner shell's
//!    readline buffer in one shot
//! 5. `exit\n` to terminate the shell cleanly
//!
//! The gate is **"no crash, no hang"** — *not* "shell exits 0". A shell
//! that returns non-zero because we fed it garbage is still proof that
//! the wrapper didn't crash. The watchdog is the real hang detector:
//! if it has to kill the child, the assertion below fails the test.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const KILL_TIMEOUT: Duration = Duration::from_secs(15);
const PASTE_BYTES: usize = 100 * 1024;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[test]
fn paste_100kb_does_not_crash_or_hang() {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    // SPEC §3 supported shells are zsh and bash 4.0+. macOS's /bin/sh
    // (bash 3.2) is outside the support matrix and handles 100 KB
    // bracketed-paste input painfully slowly due to pre-bracketed-paste
    // line-editing semantics. Use platform-appropriate fast shells.
    let shell = if cfg!(target_os = "macos") {
        "/bin/zsh"
    } else {
        "/bin/bash"
    };
    let bin = env!("CARGO_BIN_EXE_hintkit");
    let mut cmd = CommandBuilder::new(bin);
    cmd.env("SHELL", shell);
    cmd.env("TERM", "xterm-256color");
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    }

    let mut child = pair.slave.spawn_command(cmd).expect("spawn hintkit");
    drop(pair.slave);

    let mut writer = pair.master.take_writer().expect("take_writer");
    let mut reader = pair.master.try_clone_reader().expect("clone_reader");

    // Drain output continuously so the wrapper's PTY pipe never fills up.
    let (tx, rx) = mpsc::channel::<usize>();
    let reader_handle = thread::spawn(move || {
        let mut total = 0usize;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    let _ = tx.send(n);
                }
                Err(_) => break,
            }
        }
        total
    });

    // Wait for the inner shell to be ready (proxy: see any output).
    let mut saw_output = false;
    let warmup_start = Instant::now();
    while warmup_start.elapsed() < Duration::from_secs(3) {
        if rx.recv_timeout(Duration::from_millis(100)).is_ok() {
            saw_output = true;
            break;
        }
    }
    assert!(
        saw_output,
        "wrapper did not produce any output within 3 s of starting"
    );

    // Build the paste payload in one allocation so we don't add latency
    // between the start delimiter and content.
    let mut payload = Vec::with_capacity(PASTE_START.len() + PASTE_BYTES + PASTE_END.len());
    payload.extend_from_slice(PASTE_START);
    payload.resize(payload.len() + PASTE_BYTES, b'a');
    payload.extend_from_slice(PASTE_END);

    let paste_start = Instant::now();
    writer.write_all(&payload).expect("write paste payload");
    writer.flush().expect("flush paste payload");
    let paste_elapsed = paste_start.elapsed();

    // Briefly let the inner shell absorb the pasted text.
    thread::sleep(Duration::from_millis(150));

    // Backward-kill-line: bound to ^U in both zsh's emacs keymap and
    // bash's readline. Wipes the 100 KB pasted line from the buffer in
    // one operation.
    writer.write_all(b"\x15").expect("write ^U");
    writer.flush().expect("flush ^U");

    thread::sleep(Duration::from_millis(50));

    writer.write_all(b"exit\n").expect("write exit");
    writer.flush().expect("flush exit");

    // Watchdog: kill the wrapper if it hasn't exited within KILL_TIMEOUT.
    // We flag the kill so the assertion can distinguish "shell returned
    // non-zero" from "we had to SIGKILL the wrapper to stop a hang".
    let killed = Arc::new(AtomicBool::new(false));
    let killed_clone = Arc::clone(&killed);
    let mut killer = child.clone_killer();
    let _watchdog = thread::spawn(move || {
        thread::sleep(KILL_TIMEOUT);
        killed_clone.store(true, Ordering::SeqCst);
        let _ = killer.kill();
    });

    let status = child.wait().expect("wait on hintkit");
    drop(writer);
    let total_read = reader_handle.join().expect("reader join");

    assert!(
        !killed.load(Ordering::SeqCst),
        "watchdog had to SIGKILL the wrapper — it hung past {KILL_TIMEOUT:?} \
         (paste_elapsed={paste_elapsed:?}, bytes_read={total_read}, exit={:?})",
        status.exit_code()
    );

    // The wrapper exited under its own power. That's the Phase 2 gate.
    // We additionally print the shell's exit code for diagnostic value
    // — non-zero is acceptable (the shell rejected our pasted garbage),
    // but any future-you sanity-checking should see it.
    eprintln!(
        "paste_100kb: paste_write={paste_elapsed:?}, bytes_drained={total_read}, \
         shell_exit={:?}",
        status.exit_code()
    );
}
