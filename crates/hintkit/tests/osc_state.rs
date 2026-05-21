//! Phase 3 end-to-end gate: spawn the wrapper around a real `/bin/zsh`,
//! source the integration script via `hintkit init zsh`, run one
//! trivial command, and verify that the OSC 133 A/C/D + OSC 7 sequences
//! actually appear in the wrapper's output stream.
//!
//! Pairs with the parser+state unit tests in `output.rs`. Those prove
//! the wrapper correctly interprets the sequences; this proves the
//! integration script we ship actually emits them when sourced under
//! a real shell.
//!
//! Skipped on hosts without `/bin/zsh`.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const KILL_TIMEOUT: Duration = Duration::from_secs(15);
const ZSH_PATH: &str = "/bin/zsh";

#[test]
fn zsh_integration_emits_osc_133_sequences() {
    if !Path::new(ZSH_PATH).exists() {
        eprintln!("skipping zsh_integration: {ZSH_PATH} not present on this host");
        return;
    }

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
    cmd.env("SHELL", ZSH_PATH);
    cmd.env("TERM", "xterm-256color");
    // The sourced integration uses `$HOST` for OSC 7; set a deterministic
    // value so the URL we look for is predictable.
    cmd.env("HOST", "test-host");
    // Path the inner `source <(…)` will exec.
    cmd.env("HINTKIT_BIN", bin);
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

    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);
    let reader_handle = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    captured_clone
                        .lock()
                        .expect("captured buffer poisoned")
                        .extend_from_slice(&buf[..n]);
                }
                Err(_) => break,
            }
        }
    });

    // Wait for zsh's first prompt (still without OSC 133 — the integration
    // hasn't been sourced yet).
    wait_for_bytes(&captured, 80, Duration::from_secs(5))
        .expect("no output from zsh within 5s — wrapper failed to start");

    // Source the integration via process substitution. zsh supports
    // `<(cmd)` natively; the `cmd` we invoke is our own binary's `init
    // zsh` subcommand, which prints the script and exits.
    writer
        .write_all(b"source <($HINTKIT_BIN init zsh)\n")
        .expect("write source");
    writer.flush().expect("flush source");
    thread::sleep(Duration::from_millis(400));

    // One trivial command exercises preexec (C), command, then precmd
    // (D + A + OSC 7).
    writer.write_all(b"true\n").expect("write true");
    writer.flush().expect("flush true");
    thread::sleep(Duration::from_millis(400));

    writer.write_all(b"exit\n").expect("write exit");
    writer.flush().expect("flush exit");

    let killed = Arc::new(AtomicBool::new(false));
    let killed_clone = Arc::clone(&killed);
    let mut killer = child.clone_killer();
    let _watchdog = thread::spawn(move || {
        thread::sleep(KILL_TIMEOUT);
        killed_clone.store(true, Ordering::SeqCst);
        let _ = killer.kill();
    });

    let _status = child.wait().expect("wait on hintkit");
    drop(writer);
    let _ = reader_handle.join();

    assert!(
        !killed.load(Ordering::SeqCst),
        "watchdog had to SIGKILL the wrapper — it hung past {KILL_TIMEOUT:?}"
    );

    let output = captured.lock().expect("captured buffer poisoned").clone();

    let needles: &[(&[u8], &str)] = &[
        (b"\x1b]133;A", "OSC 133 A (prompt-start)"),
        (b"\x1b]133;B", "OSC 133 B (command-input marker)"),
        (b"\x1b]133;C", "OSC 133 C (command-start, via preexec)"),
        (b"\x1b]133;D", "OSC 133 D (command-done, via precmd)"),
        (b"\x1b]7;file://test-host", "OSC 7 cwd report"),
    ];

    for (needle, label) in needles {
        assert!(
            contains(&output, needle),
            "no {label} in wrapper output ({} bytes captured) — \
             integration script did not run, or shell did not exercise the hook",
            output.len(),
        );
    }
}

fn wait_for_bytes(buf: &Arc<Mutex<Vec<u8>>>, min: usize, timeout: Duration) -> Option<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if buf.lock().expect("captured buffer poisoned").len() >= min {
            return Some(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    None
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}
