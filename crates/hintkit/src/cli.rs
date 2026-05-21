//! Command-line interface (SPEC §7 Phase 3 — `hintkit init <shell>`;
//! Phase 7 will add `doctor`, `uninstall`).
//!
//! No-arg invocation runs the PTY wrapper — the default behavior that
//! the shell-integration line targets. Subcommands provide
//! shell-integration emission and (later) diagnostics.

use clap::{Parser, Subcommand};

use crate::shell_integration::{integration_for, SUPPORTED_SHELLS};

#[derive(Debug, Parser)]
#[command(
    name = "hintkit",
    version,
    about = "Lightweight, local, no-account terminal autocomplete.",
    long_about = None,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the shell-integration script for `<shell>` to stdout.
    /// Pipe it into your rc file, e.g. `hintkit init zsh >> ~/.zshrc`.
    Init {
        /// One of `zsh`, `bash`.
        shell: String,
    },
}

/// Run the `init` subcommand. Returns the process exit code.
pub fn run_init(shell: &str) -> i32 {
    match integration_for(shell) {
        Some(script) => {
            // Write directly to stdout — println! would tack an extra
            // newline that confuses `>> ~/.zshrc` users into thinking
            // the script ends with a blank line.
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            if let Err(e) = out.write_all(script.as_bytes()) {
                eprintln!("hintkit: writing init script: {e}");
                return 1;
            }
            0
        }
        None => {
            eprintln!(
                "hintkit: unknown shell '{shell}'. Supported: {}",
                SUPPORTED_SHELLS.join(", ")
            );
            // POSIX exit 2 = misuse of shell builtin / invalid arguments.
            2
        }
    }
}
