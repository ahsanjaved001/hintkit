mod cli;
mod engine;
mod generators;
mod input;
mod line_buffer;
mod output;
mod pty;
mod shell_integration;
mod state;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    init_tracing();
    let cli = cli::Cli::parse();
    let exit_code = match cli.command {
        Some(cli::Command::Init { shell }) => cli::run_init(&shell),
        None => run_wrapper(),
    };
    let code = u8::try_from(exit_code).unwrap_or(1);
    ExitCode::from(code)
}

fn run_wrapper() -> i32 {
    match pty::run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("hintkit: {e:#}");
            1
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
