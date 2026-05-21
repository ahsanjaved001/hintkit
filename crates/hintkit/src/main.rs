mod engine;
mod input;
mod pty;

use std::process::ExitCode;

fn main() -> ExitCode {
    init_tracing();
    // Phase 1 + 2: PTY wrapper with bracketed-paste-aware input throttling.
    // No CLI args yet — that lands in Phase 3 (`hintkit init <shell>`) and
    // Phase 7 (`doctor`, `uninstall`).
    match pty::run() {
        Ok(code) => {
            // Map the wrapped shell's exit code into our own. `ExitCode`
            // can't represent the full i32 range, so clamp negatives and
            // out-of-range positives to 1 (POSIX "general failure").
            let code = u8::try_from(code).unwrap_or(1);
            ExitCode::from(code)
        }
        Err(e) => {
            eprintln!("hintkit: {e:#}");
            ExitCode::from(1)
        }
    }
}

/// Initialize the `tracing` subscriber when the `debug` Cargo feature is
/// active. SPEC §9: traces NEVER include byte content — only structural
/// state transitions. Default release builds compile this to a no-op.
#[cfg(feature = "debug")]
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

#[cfg(not(feature = "debug"))]
fn init_tracing() {}

#[cfg(test)]
mod tests {
    #[test]
    fn version_matches_phase_zero_stub() {
        assert_eq!(env!("CARGO_PKG_VERSION"), "0.0.0");
    }
}
