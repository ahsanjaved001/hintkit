//! `.hintkitspec` schema and loader (SPEC §7 Phase 4).
//!
//! A `.hintkitspec` is a JSON file describing a CLI's surface — its
//! subcommand tree, options (flags), positional arguments, and any
//! allowlisted dynamic generators. The format is consumed by the
//! suggestion engine (Phase 5) and produced by the `tools/ingest-specs`
//! Node.js script (Phase 4b) from upstream `withfig/autocomplete`
//! TypeScript specs.
//!
//! Format: JSON for v0.1 (text, diff-friendly, easy to inspect during
//! development). PLAN.md notes postcard as a future option once binary
//! size pressure materializes — not now.
//!
//! Schema invariants:
//! - `name` is the canonical identifier used for lookup (`SpecDb::lookup`).
//! - Generators are an enum, not arbitrary code. v0.1 ships a tiny
//!   allowlist (file path, dir path, git branches, package.json scripts).
//!   Specs referencing unknown generators are rejected by the ingest
//!   pipeline, not silently dropped at runtime.

use serde::{Deserialize, Serialize};

/// A single CLI tool's complete completion specification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandSpec {
    /// Canonical name. For `git`, this is `"git"`. Matches the command
    /// the user types and the filename `<name>.hintkitspec.json`.
    pub name: String,
    /// One-liner shown alongside the command in suggestions. Optional
    /// because many short utilities don't carry an upstream description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Nested subcommand tree. Each element is itself a full
    /// `CommandSpec` — the schema is recursive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subcommands: Vec<CommandSpec>,
    /// Flags/options that apply at this command level.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<OptionSpec>,
    /// Positional arguments (in order). Most commands have 0–1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<ArgSpec>,
}

/// A flag/option, e.g. `-v` / `--verbose` or `--message <msg>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OptionSpec {
    /// All accepted spellings, e.g. `["--message", "-m"]`. Conventionally
    /// long-form first.
    pub names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// If the option takes a value, the schema for that value lives here.
    /// Empty when the option is a pure boolean flag.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<ArgSpec>,
}

/// A positional or option-value argument.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArgSpec {
    /// Human-facing name for the arg slot (e.g. `"branch"`, `"path"`).
    /// Shown in suggestion descriptions like `git checkout <branch>`.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional dynamic-completion generator. Allowlisted to a small,
    /// safe set; see [`GeneratorKind`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator: Option<GeneratorKind>,
}

/// The set of dynamic generators the v0.1 engine knows how to run.
/// Anything else is rejected at ingest time — the runtime never
/// evaluates arbitrary shell code from a spec (SPEC §3 commitment #2,
/// no untrusted code execution).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GeneratorKind {
    /// Complete to file paths relative to cwd.
    FilePath,
    /// Complete to directory paths relative to cwd.
    DirPath,
    /// Local git branch names (runs `git branch`, 200 ms timeout).
    GitBranches,
    /// Script names from `package.json`'s `scripts` block.
    PackageJsonScripts,
}

/// Parse a `.hintkitspec.json` from text. Errors surface as
/// `serde_json::Error` so callers can format diagnostic context.
pub fn parse_json(text: &str) -> Result<CommandSpec, serde_json::Error> {
    serde_json::from_str(text)
}

/// Serialize a `CommandSpec` to compact JSON. Used by the ingest
/// pipeline and by tests; not exposed on the runtime hot path.
pub fn to_json(spec: &CommandSpec) -> Result<String, serde_json::Error> {
    serde_json::to_string(spec)
}

/// Serialize a `CommandSpec` to pretty JSON for human inspection.
pub fn to_json_pretty(spec: &CommandSpec) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_git() -> CommandSpec {
        CommandSpec {
            name: "git".to_string(),
            description: Some("the stupid content tracker".to_string()),
            subcommands: vec![CommandSpec {
                name: "checkout".to_string(),
                description: Some("Switch branches".to_string()),
                subcommands: vec![],
                options: vec![OptionSpec {
                    names: vec!["-b".to_string()],
                    description: Some("Create and switch to a new branch".to_string()),
                    args: vec![ArgSpec {
                        name: "new-branch".to_string(),
                        description: None,
                        generator: None,
                    }],
                }],
                args: vec![ArgSpec {
                    name: "branch".to_string(),
                    description: Some("Branch to switch to".to_string()),
                    generator: Some(GeneratorKind::GitBranches),
                }],
            }],
            options: vec![],
            args: vec![],
        }
    }

    #[test]
    fn roundtrips_through_json() {
        let spec = sample_git();
        let json = to_json(&spec).unwrap();
        let parsed = parse_json(&json).unwrap();
        assert_eq!(spec, parsed);
    }

    #[test]
    fn optional_fields_serialize_compactly() {
        let minimal = CommandSpec {
            name: "ls".to_string(),
            description: None,
            subcommands: vec![],
            options: vec![],
            args: vec![],
        };
        let json = to_json(&minimal).unwrap();
        assert_eq!(json, r#"{"name":"ls"}"#);
    }

    #[test]
    fn generator_kinds_use_snake_case() {
        let spec = CommandSpec {
            name: "git".into(),
            description: None,
            subcommands: vec![],
            options: vec![],
            args: vec![ArgSpec {
                name: "branch".into(),
                description: None,
                generator: Some(GeneratorKind::GitBranches),
            }],
        };
        let json = to_json(&spec).unwrap();
        assert!(json.contains("\"git_branches\""), "got: {json}");
    }

    #[test]
    fn unknown_generator_kind_in_json_is_rejected() {
        let bad = r#"{
            "name":"git",
            "args":[{"name":"x","generator":"do_arbitrary_eval"}]
        }"#;
        assert!(parse_json(bad).is_err());
    }
}
