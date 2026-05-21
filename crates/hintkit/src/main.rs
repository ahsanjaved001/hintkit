mod pty;

use std::process::ExitCode;

fn main() -> ExitCode {
    // Phase 1: bare PTY passthrough. No CLI args yet — that lands in
    // Phase 3 (`hintkit init <shell>`) and Phase 7 (`doctor`, `uninstall`).
    match pty::run() {
        Ok(code) => {
            // Map the wrapped shell's exit code into our own. ExitCode
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

#[cfg(test)]
mod tests {
    #[test]
    fn version_matches_phase_zero_stub() {
        assert_eq!(env!("CARGO_PKG_VERSION"), "0.0.0");
    }
}
