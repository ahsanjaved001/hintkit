//! Command-line parsing for hintkit (SPEC §7 Phase 5).
//!
//! Three layers, all pure functions:
//!
//! 1. [`tokenize`] — split a raw command-line string by whitespace,
//!    tracking byte ranges and the cursor's containing token.
//! 2. [`parse_context`] — walk the tokens against a [`CommandSpec`]
//!    tree to figure out *where* the cursor is logically: completing a
//!    subcommand, providing an option value, on a positional arg, etc.
//! 3. [`match_suggestions`] — given a parse context and the partial
//!    prefix under the cursor, produce a ranked list of suggestions.
//!    Pure data; generator invocation is the caller's responsibility.
//!
//! v0.1 deliberately ignores shell quoting and escaping — most
//! completion happens on unquoted bare tokens; quoting support lands
//! in v0.2 along with multi-word arg values.

use hintkit_spec::{ArgSpec, CommandSpec, GeneratorKind, OptionSpec};

// -- Layer 1: tokenization ---------------------------------------------

/// A single whitespace-delimited token from the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token<'a> {
    pub text: &'a str,
    /// Byte offset of the first character (inclusive).
    pub start: usize,
    /// Byte offset one past the last character (exclusive).
    pub end: usize,
}

/// Where the cursor sits relative to the tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorPlacement {
    /// Cursor is inside (or at the end of) a specific token. The user
    /// is mid-edit on that token's prefix.
    InToken { index: usize },
    /// Cursor is in whitespace between/after tokens — the user has
    /// finished the previous token and is about to start a new one.
    BetweenTokens,
}

/// Outcome of tokenizing a command-line + cursor pair.
#[derive(Debug, Clone)]
pub struct Tokenized<'a> {
    pub tokens: Vec<Token<'a>>,
    pub cursor: CursorPlacement,
}

impl<'a> Tokenized<'a> {
    /// The byte prefix at the cursor — the partial text the user has
    /// typed for the in-progress token. Empty when [`CursorPlacement::BetweenTokens`].
    pub fn cursor_prefix(&self, line: &'a str) -> &'a str {
        match self.cursor {
            CursorPlacement::InToken { index } => {
                let tok = &self.tokens[index];
                // We treat the cursor as always at the end of the
                // partial prefix — even if it's mid-token, completion
                // historically replaces from the cursor backwards.
                // For v0.1 simplicity, the prefix is the whole token.
                &line[tok.start..tok.end]
            }
            CursorPlacement::BetweenTokens => "",
        }
    }
}

/// Split `line` into tokens and locate where `cursor` sits among them.
/// `cursor` is a byte offset into `line`; values out of range are
/// clamped to `line.len()`.
pub fn tokenize(line: &str, cursor: usize) -> Tokenized<'_> {
    let cursor = cursor.min(line.len());
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Skip whitespace.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        tokens.push(Token {
            text: &line[start..i],
            start,
            end: i,
        });
    }

    let placement = locate_cursor(&tokens, cursor, line);
    Tokenized {
        tokens,
        cursor: placement,
    }
}

fn locate_cursor(tokens: &[Token<'_>], cursor: usize, line: &str) -> CursorPlacement {
    // Cursor is "in" a token if it's between its start (inclusive) and
    // its end (inclusive — completing-at-end is the common case).
    for (i, tok) in tokens.iter().enumerate() {
        if cursor >= tok.start && cursor <= tok.end {
            // Edge case: cursor at end of a token AND the next byte is
            // not whitespace (impossible by construction) — but cursor
            // at end immediately followed by whitespace counts as
            // BetweenTokens once the user has typed the space. Detect
            // by checking the byte at `cursor` if it exists.
            if cursor == tok.end {
                let next = line.as_bytes().get(cursor);
                if let Some(b) = next {
                    if b.is_ascii_whitespace() {
                        return CursorPlacement::BetweenTokens;
                    }
                }
            }
            return CursorPlacement::InToken { index: i };
        }
    }
    CursorPlacement::BetweenTokens
}

// -- Layer 2: parse-context walker ------------------------------------

/// What the cursor's logical position expects next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expecting<'spec> {
    /// At a position where a subcommand or an option flag could begin.
    SubcommandOrOption,
    /// In the middle of typing an option flag (token starts with `-`).
    Option,
    /// Providing a value for a specific option (e.g. `-m <here>`).
    OptionValue(&'spec OptionSpec),
    /// Providing a positional argument value.
    ArgValue(&'spec ArgSpec),
    /// Past the last expected slot — the spec has no further structure
    /// to drive completion.
    Done,
}

/// The result of walking the tokens against a spec.
#[derive(Debug, Clone)]
pub struct ParseContext<'spec> {
    /// The chain of specs we've descended into, root first. For
    /// `git checkout`, this is `[git_spec, checkout_subcommand]`.
    pub spec_path: Vec<&'spec CommandSpec>,
    pub expecting: Expecting<'spec>,
}

impl<'spec> ParseContext<'spec> {
    /// Innermost spec (the deepest subcommand the cursor is "inside").
    pub fn current_spec(&self) -> Option<&'spec CommandSpec> {
        self.spec_path.last().copied()
    }
}

/// Walk `tokenized.tokens` left-to-right against `root` to determine
/// the parse context at the cursor.
///
/// `tokenized.tokens[0]` is expected to be the command name (matched
/// against `root.name`); a mismatch returns `Expecting::Done` since
/// there's no spec context to derive completions from.
pub fn parse_context<'spec>(
    tokenized: &Tokenized<'_>,
    root: &'spec CommandSpec,
) -> ParseContext<'spec> {
    let tokens = &tokenized.tokens;
    if tokens.is_empty() {
        return ParseContext {
            spec_path: vec![root],
            expecting: Expecting::SubcommandOrOption,
        };
    }

    // Token 0 must be the root command name; otherwise there's no spec
    // to drive completion from.
    if tokens[0].text != root.name {
        return ParseContext {
            spec_path: vec![],
            expecting: Expecting::Done,
        };
    }

    let mut spec_path: Vec<&CommandSpec> = vec![root];
    let mut arg_index: usize = 0;
    let mut i: usize = 1;

    // Determine the last consumable token index — the one that gets
    // "eaten" by walking. If the cursor is mid-typing the last token,
    // we don't consume it; that token IS the completion target.
    let last_consumable = match tokenized.cursor {
        CursorPlacement::InToken { index } => index,
        CursorPlacement::BetweenTokens => tokens.len(),
    };

    while i < last_consumable {
        let tok = tokens[i].text;
        let current = *spec_path.last().expect("spec_path always non-empty");

        if tok.starts_with('-') {
            // Option flag. Check if it takes a value; if so, skip the
            // next token as its value.
            if let Some(opt) = find_option(current, tok) {
                if !opt.args.is_empty() {
                    // Consume the option-value token (best-effort — if
                    // it's missing we'll just fall through normally).
                    i += 1;
                    if i < last_consumable {
                        i += 1;
                        continue;
                    }
                }
            }
            i += 1;
            continue;
        }

        // Try to descend into a subcommand.
        if let Some(sub) = current.subcommands.iter().find(|s| s.name == tok) {
            spec_path.push(sub);
            arg_index = 0;
            i += 1;
            continue;
        }

        // Otherwise treat as a positional arg value.
        arg_index += 1;
        i += 1;
    }

    // Now figure out what's expected at the cursor.
    let current = *spec_path.last().expect("spec_path always non-empty");
    let expecting = match tokenized.cursor {
        CursorPlacement::InToken { index } => {
            let tok = tokens[index].text;
            // Are we typing into an option-value slot? That happens
            // when the immediately preceding token is an option that
            // takes a value.
            if index > 0 {
                let prev = tokens[index - 1].text;
                if prev.starts_with('-') {
                    if let Some(opt) = find_option(current, prev) {
                        if !opt.args.is_empty() {
                            return ParseContext {
                                spec_path,
                                expecting: Expecting::OptionValue(opt),
                            };
                        }
                    }
                }
            }
            if tok.starts_with('-') {
                Expecting::Option
            } else if current.args.get(arg_index).is_some() {
                Expecting::ArgValue(&current.args[arg_index])
            } else {
                Expecting::SubcommandOrOption
            }
        }
        CursorPlacement::BetweenTokens => {
            // Cursor is in trailing whitespace — about to start a new
            // token. If the previous token was an option-with-value,
            // we're now expecting that value.
            if let Some(last) = tokens.last() {
                let prev = last.text;
                if prev.starts_with('-') {
                    if let Some(opt) = find_option(current, prev) {
                        if !opt.args.is_empty() {
                            return ParseContext {
                                spec_path,
                                expecting: Expecting::OptionValue(opt),
                            };
                        }
                    }
                }
            }
            if let Some(arg) = current.args.get(arg_index) {
                Expecting::ArgValue(arg)
            } else {
                Expecting::SubcommandOrOption
            }
        }
    };

    ParseContext {
        spec_path,
        expecting,
    }
}

fn find_option<'spec>(spec: &'spec CommandSpec, token: &str) -> Option<&'spec OptionSpec> {
    spec.options
        .iter()
        .find(|o| o.names.iter().any(|n| n == token))
}

// -- Layer 3: matcher --------------------------------------------------

/// A single ranked suggestion ready to be rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// The text the user will see and accept.
    pub text: String,
    /// Optional one-line description shown alongside.
    pub description: Option<String>,
    pub kind: SuggestionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKind {
    Subcommand,
    Option,
    /// A static arg value baked into the spec (none currently bundled,
    /// but the schema supports it).
    StaticArg,
    /// A dynamic arg value resolved by running a generator at runtime.
    /// The matcher returns this kind without invoking the generator
    /// itself; the engine resolves it.
    GeneratedValue(GeneratorKind),
}

/// Produce a ranked suggestion list for the given parse context and
/// the user's typed prefix.
pub fn match_suggestions(ctx: &ParseContext<'_>, prefix: &str) -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = Vec::new();
    let Some(current) = ctx.current_spec() else {
        return out;
    };

    match &ctx.expecting {
        Expecting::SubcommandOrOption => {
            collect_subcommands(current, prefix, &mut out);
            // When the user has typed something starting with `-`,
            // they meant an option; otherwise show subcommands first
            // and options as a fallback. Keep it simple for v0.1:
            // include both, ranked by prefix match.
            if prefix.starts_with('-') || prefix.is_empty() {
                collect_options(current, prefix, &mut out);
            }
        }
        Expecting::Option => {
            collect_options(current, prefix, &mut out);
        }
        Expecting::OptionValue(opt) => {
            if let Some(arg) = opt.args.first() {
                push_arg_suggestion(arg, prefix, &mut out);
            }
        }
        Expecting::ArgValue(arg) => {
            push_arg_suggestion(arg, prefix, &mut out);
        }
        Expecting::Done => {}
    }

    rank(&mut out, prefix);
    out
}

fn collect_subcommands(spec: &CommandSpec, prefix: &str, out: &mut Vec<Suggestion>) {
    for sub in &spec.subcommands {
        if sub.name.starts_with(prefix) {
            out.push(Suggestion {
                text: sub.name.clone(),
                description: sub.description.clone(),
                kind: SuggestionKind::Subcommand,
            });
        }
    }
}

fn collect_options(spec: &CommandSpec, prefix: &str, out: &mut Vec<Suggestion>) {
    for opt in &spec.options {
        for name in &opt.names {
            if name.starts_with(prefix) {
                out.push(Suggestion {
                    text: name.clone(),
                    description: opt.description.clone(),
                    kind: SuggestionKind::Option,
                });
            }
        }
    }
}

fn push_arg_suggestion(arg: &ArgSpec, _prefix: &str, out: &mut Vec<Suggestion>) {
    if let Some(gen) = arg.generator {
        out.push(Suggestion {
            text: format!("<{}>", arg.name),
            description: arg.description.clone(),
            kind: SuggestionKind::GeneratedValue(gen),
        });
    } else {
        out.push(Suggestion {
            text: format!("<{}>", arg.name),
            description: arg.description.clone(),
            kind: SuggestionKind::StaticArg,
        });
    }
}

fn rank(out: &mut [Suggestion], prefix: &str) {
    // Two-step rank: items whose .text starts with prefix come first
    // (tie-broken alphabetically), then items where prefix appears
    // as a substring. v0.1 keeps it simple.
    out.sort_by(|a, b| {
        let a_starts = a.text.starts_with(prefix);
        let b_starts = b.text.starts_with(prefix);
        match (a_starts, b_starts) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.text.cmp(&b.text),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use hintkit_spec::{ArgSpec, CommandSpec, GeneratorKind, OptionSpec};

    // ---- tokenizer ----

    #[test]
    fn tokenize_empty_line() {
        let t = tokenize("", 0);
        assert!(t.tokens.is_empty());
        assert_eq!(t.cursor, CursorPlacement::BetweenTokens);
    }

    #[test]
    fn tokenize_single_word_cursor_at_end() {
        let t = tokenize("git", 3);
        assert_eq!(t.tokens.len(), 1);
        assert_eq!(t.tokens[0].text, "git");
        assert_eq!(t.cursor, CursorPlacement::InToken { index: 0 });
    }

    #[test]
    fn tokenize_trailing_space_means_between() {
        let t = tokenize("git ", 4);
        assert_eq!(t.tokens.len(), 1);
        assert_eq!(t.cursor, CursorPlacement::BetweenTokens);
    }

    #[test]
    fn tokenize_partial_subcommand() {
        let t = tokenize("git che", 7);
        assert_eq!(t.tokens.len(), 2);
        assert_eq!(t.tokens[1].text, "che");
        assert_eq!(t.cursor, CursorPlacement::InToken { index: 1 });
    }

    #[test]
    fn cursor_out_of_range_clamps() {
        let t = tokenize("git", 9999);
        assert_eq!(t.cursor, CursorPlacement::InToken { index: 0 });
    }

    // ---- parse-context ----

    fn git_spec() -> CommandSpec {
        CommandSpec {
            name: "git".into(),
            description: Some("the stupid content tracker".into()),
            subcommands: vec![
                CommandSpec {
                    name: "checkout".into(),
                    description: Some("Switch branches".into()),
                    subcommands: vec![],
                    options: vec![OptionSpec {
                        names: vec!["-b".into()],
                        description: Some("Create and switch to a new branch".into()),
                        args: vec![ArgSpec {
                            name: "new-branch".into(),
                            description: None,
                            generator: None,
                        }],
                    }],
                    args: vec![ArgSpec {
                        name: "branch".into(),
                        description: None,
                        generator: Some(GeneratorKind::GitBranches),
                    }],
                },
                CommandSpec {
                    name: "commit".into(),
                    description: Some("Record changes".into()),
                    subcommands: vec![],
                    options: vec![OptionSpec {
                        names: vec!["--message".into(), "-m".into()],
                        description: None,
                        args: vec![ArgSpec {
                            name: "message".into(),
                            description: None,
                            generator: None,
                        }],
                    }],
                    args: vec![],
                },
            ],
            options: vec![OptionSpec {
                names: vec!["--version".into()],
                description: None,
                args: vec![],
            }],
            args: vec![],
        }
    }

    #[test]
    fn parse_context_at_bare_command_expects_subcommand() {
        let g = git_spec();
        let t = tokenize("git ", 4);
        let ctx = parse_context(&t, &g);
        assert!(matches!(ctx.expecting, Expecting::SubcommandOrOption));
        assert_eq!(ctx.spec_path.len(), 1);
    }

    #[test]
    fn parse_context_descends_into_subcommand() {
        let g = git_spec();
        let t = tokenize("git checkout ", 13);
        let ctx = parse_context(&t, &g);
        // Past `checkout`, the next slot is the `branch` arg.
        match ctx.expecting {
            Expecting::ArgValue(a) => assert_eq!(a.name, "branch"),
            other => panic!("expected ArgValue(branch), got {other:?}"),
        }
        assert_eq!(ctx.spec_path.len(), 2);
        assert_eq!(ctx.spec_path.last().unwrap().name, "checkout");
    }

    #[test]
    fn parse_context_recognizes_option_value_slot() {
        let g = git_spec();
        // After `git commit -m `, expecting the message value.
        let t = tokenize("git commit -m ", 14);
        let ctx = parse_context(&t, &g);
        match ctx.expecting {
            Expecting::OptionValue(o) => assert!(o.names.contains(&"-m".to_string())),
            other => panic!("expected OptionValue, got {other:?}"),
        }
    }

    #[test]
    fn parse_context_partial_subcommand_in_token() {
        let g = git_spec();
        let t = tokenize("git che", 7);
        let ctx = parse_context(&t, &g);
        // We're typing the subcommand, so `current_spec` is still git.
        assert_eq!(ctx.current_spec().unwrap().name, "git");
        assert!(matches!(ctx.expecting, Expecting::SubcommandOrOption));
    }

    #[test]
    fn parse_context_partial_option() {
        let g = git_spec();
        let t = tokenize("git --ver", 9);
        let ctx = parse_context(&t, &g);
        assert!(matches!(ctx.expecting, Expecting::Option));
    }

    #[test]
    fn parse_context_unknown_command_yields_done() {
        let g = git_spec();
        let t = tokenize("nope something", 14);
        let ctx = parse_context(&t, &g);
        assert!(matches!(ctx.expecting, Expecting::Done));
        assert!(ctx.spec_path.is_empty());
    }

    // ---- matcher ----

    #[test]
    fn match_subcommands_filters_by_prefix() {
        let g = git_spec();
        let t = tokenize("git c", 5);
        let ctx = parse_context(&t, &g);
        let suggestions = match_suggestions(&ctx, "c");
        let names: Vec<&str> = suggestions.iter().map(|s| s.text.as_str()).collect();
        assert!(names.contains(&"checkout"));
        assert!(names.contains(&"commit"));
    }

    #[test]
    fn match_empty_prefix_includes_all_subcommands() {
        let g = git_spec();
        let t = tokenize("git ", 4);
        let ctx = parse_context(&t, &g);
        let suggestions = match_suggestions(&ctx, "");
        assert_eq!(suggestions.len(), 3); // checkout, commit, --version
    }

    #[test]
    fn match_arg_value_uses_generator_kind() {
        let g = git_spec();
        let t = tokenize("git checkout ", 13);
        let ctx = parse_context(&t, &g);
        let suggestions = match_suggestions(&ctx, "");
        assert_eq!(suggestions.len(), 1);
        assert!(matches!(
            suggestions[0].kind,
            SuggestionKind::GeneratedValue(GeneratorKind::GitBranches)
        ));
    }

    #[test]
    fn rank_puts_prefix_matches_first() {
        let g = git_spec();
        let t = tokenize("git ch", 6);
        let ctx = parse_context(&t, &g);
        let suggestions = match_suggestions(&ctx, "ch");
        assert_eq!(suggestions[0].text, "checkout");
    }
}
