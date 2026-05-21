//! Bundled `.hintkitspec.json` files for hintkit's v0.1 curated command
//! set (SPEC §7 Phase 4). Files live in `data/` and are embedded at
//! compile time via [`include_dir!`]; lookups parse the JSON on first
//! access and cache the result, so a cold lookup is `O(file size)` and
//! every subsequent lookup is `O(hash)`.
//!
//! At Phase 4a, `data/` is seeded with one hand-written spec (`git`)
//! to prove the pipeline end-to-end. Phase 4b replaces these with
//! output from `tools/ingest-specs` against `withfig/autocomplete`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use hintkit_spec::CommandSpec;
use include_dir::{include_dir, Dir};

/// All bundled spec files, embedded at compile time.
const SPEC_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/data");

/// Filename suffix that marks a bundled spec.
const SPEC_SUFFIX: &str = ".hintkitspec.json";

/// Lookup-by-name registry over the bundled specs. Constructed lazily
/// on first call to any method; parses are cached per spec so we never
/// re-parse the same file.
#[derive(Default)]
pub struct SpecDb {
    cache: Mutex<HashMap<String, CommandSpec>>,
}

impl SpecDb {
    /// Get the process-wide bundled spec database. Cheaper than
    /// constructing one per call since each instance maintains its
    /// own parse cache.
    pub fn global() -> &'static SpecDb {
        static INSTANCE: OnceLock<SpecDb> = OnceLock::new();
        INSTANCE.get_or_init(SpecDb::default)
    }

    /// Return the bundled spec for `name`, parsed and cached.
    ///
    /// Returns `None` if there's no bundled spec, or if the bundled
    /// file failed to parse (in which case a diagnostic is written to
    /// stderr — bundled files are vetted by build-time CI, so a parse
    /// failure here indicates a genuine bug).
    pub fn lookup(&self, name: &str) -> Option<CommandSpec> {
        {
            let cache = self.cache.lock().expect("SpecDb cache poisoned");
            if let Some(spec) = cache.get(name) {
                return Some(spec.clone());
            }
        }
        let filename = format!("{name}{SPEC_SUFFIX}");
        let file = SPEC_DIR.get_file(&filename)?;
        let text = file.contents_utf8()?;
        match hintkit_spec::parse_json(text) {
            Ok(spec) => {
                let mut cache = self.cache.lock().expect("SpecDb cache poisoned");
                cache.insert(name.to_string(), spec.clone());
                Some(spec)
            }
            Err(e) => {
                eprintln!("hintkit-specs-bundled: failed to parse {filename}: {e}");
                None
            }
        }
    }

    /// Iterator over every bundled spec name. Order is unspecified
    /// (matches `include_dir`'s file iteration).
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        SPEC_DIR.files().filter_map(|f| {
            f.path()
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(SPEC_SUFFIX))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_known_name_returns_parsed_spec() {
        let db = SpecDb::default();
        let git = db.lookup("git").expect("git spec should be bundled");
        assert_eq!(git.name, "git");
        assert!(
            git.subcommands.iter().any(|sc| sc.name == "checkout"),
            "git spec should include `checkout` subcommand"
        );
    }

    #[test]
    fn lookup_unknown_name_returns_none() {
        let db = SpecDb::default();
        assert!(db.lookup("definitely-not-a-real-command").is_none());
    }

    #[test]
    fn names_lists_every_bundled_spec() {
        let db = SpecDb::default();
        let names: Vec<&str> = db.names().collect();
        assert!(
            names.contains(&"git"),
            "expected `git` in bundled names; got: {names:?}"
        );
        // Every name listed must round-trip back to a successful lookup.
        for name in names {
            assert!(
                db.lookup(name).is_some(),
                "names() returned `{name}` but lookup() failed"
            );
        }
    }

    #[test]
    fn lookup_is_cached() {
        let db = SpecDb::default();
        let first = db.lookup("git").unwrap();
        let second = db.lookup("git").unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn every_bundled_spec_parses_cleanly() {
        // Acts as a build-time gate: any future hand-written or
        // ingest-generated spec that fails to parse fails CI here.
        let db = SpecDb::default();
        for name in db.names() {
            db.lookup(name)
                .unwrap_or_else(|| panic!("bundled spec {name} failed to parse"));
        }
    }
}
