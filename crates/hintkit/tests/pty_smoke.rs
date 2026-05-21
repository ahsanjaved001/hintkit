//! Smoke test for the Phase 1 PTY wrapper.
//!
//! Spawns the `hintkit` binary inside a synthetic PTY, feeds it a single
//! `exit 42` command, and verifies that the shell's exit code propagates
//! back. This proves the wrapper is end-to-end functional without needing
//! a human at the keyboard. Interactive concerns (vim/htop redraw,
//! window resize, alt-screen) still require manual verification.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const KILL_TIMEOUT: Duration = Duration::from_secs(15);

#[test]
fn wrapper_propagates_shell_exit_code() {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let bin = env!("CARGO_BIN_EXE_hintkit");
    let mut cmd = CommandBuilder::new(bin);
    // Pin the wrapped shell so the test doesn't depend on the developer's
    // `$SHELL`. `/bin/sh` is POSIX-mandated everywhere we target.
    cmd.env("SHELL", "/bin/sh");
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

    // Drain output on a background thread so the inner shell never blocks
    // on a full output pipe while waiting for our command to land.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reader_handle = thread::spawn(move || {
        let mut all = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    all.extend_from_slice(&buf[..n]);
                    let _ = tx.send(buf[..n].to_vec());
                }
                Err(_) => break,
            }
        }
        all
    });

    // Wait briefly for the inner shell to be ready to consume input. There
    // is no clean signal for "shell prompt drawn" without parsing OSC 133
    // (Phase 3), so we poll for *any* output as a proxy and cap the wait.
    let start = Instant::now();
    let mut saw_output = false;
    while start.elapsed() < Duration::from_secs(3) {
        if rx.recv_timeout(Duration::from_millis(100)).is_ok() {
            saw_output = true;
            break;
        }
    }
    assert!(
        saw_output,
        "no output from wrapped shell within 3s — wrapper likely failed to start"
    );

    writer.write_all(b"exit 42\n").expect("write exit");
    writer.flush().expect("flush");

    // Watchdog: SIGKILL the child if it hangs past KILL_TIMEOUT so the
    // test reports a real failure instead of a CI timeout.
    let mut killer = child.clone_killer();
    let watchdog = thread::spawn(move || {
        thread::sleep(KILL_TIMEOUT);
        let _ = killer.kill();
    });

    let status = child.wait().expect("wait on hintkit");
    drop(writer);
    let _ = reader_handle.join();
    drop(watchdog);

    assert_eq!(
        status.exit_code(),
        42,
        "hintkit did not propagate wrapped shell's exit code"
    );
}
