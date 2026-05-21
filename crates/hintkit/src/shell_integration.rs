//! Bundles the per-shell integration scripts into the binary so the
//! `hintkit init <shell>` subcommand can print them on demand.
//!
//! The scripts live in `shell/` at the workspace root and are pulled in
//! via `include_str!`. Anything that changes the scripts requires a
//! rebuild — that's intentional, the scripts are part of the public
//! interface and shouldn't drift independently of the binary.

const ZSH_INTEGRATION: &str = include_str!("../../../shell/hintkit.zsh");
const BASH_INTEGRATION: &str = include_str!("../../../shell/hintkit.bash");

/// Return the integration script for a supported shell name. Returns
/// `None` if the shell isn't one of v0.1's supported set (`zsh`, `bash`).
pub fn integration_for(shell: &str) -> Option<&'static str> {
    match shell {
        "zsh" => Some(ZSH_INTEGRATION),
        "bash" => Some(BASH_INTEGRATION),
        _ => None,
    }
}

/// Names of every shell we currently bundle an integration for. Used by
/// the CLI to render helpful error messages.
pub const SUPPORTED_SHELLS: &[&str] = &["zsh", "bash"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_shells_have_integration() {
        for &shell in SUPPORTED_SHELLS {
            let script = integration_for(shell);
            assert!(script.is_some(), "no integration bundled for {shell}");
            let body = script.unwrap();
            assert!(
                body.contains("HINTKIT_WRAPPED"),
                "{shell} integration missing HINTKIT_WRAPPED guard"
            );
            assert!(
                body.contains("\\e]133;"),
                "{shell} integration missing OSC 133 emission"
            );
        }
    }

    #[test]
    fn unknown_shell_returns_none() {
        assert!(integration_for("fish").is_none());
        assert!(integration_for("").is_none());
        assert!(integration_for("powershell").is_none());
    }
}
