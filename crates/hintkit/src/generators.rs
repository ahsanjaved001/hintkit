//! Native dynamic-suggestion generators (SPEC §7 Phase 5 step 4).
//!
//! Each generator is a pure-Rust function that returns candidate
//! strings given a working directory. Generators that shell out (only
//! `git_branches` today) enforce a 200 ms wall-clock cap by reading
//! the child's stdout on a worker thread and racing against
//! `Receiver::recv_timeout`; on timeout we SIGKILL the child and
//! return an empty result rather than risk blocking the engine.
//!
//! SPEC §3 commitment #2: the runtime never evaluates arbitrary
//! shell code from a spec. Every generator here is a fixed,
//! allowlisted operation — no spec-supplied strings reach the
//! command line of a spawned subprocess.
//!
//! Phase 5b wires `resolve()` into the suggestion thread; the
//! individual generator fns are private (only `resolve` is the public
//! entry point) so they don't trip dead-code complaints at the call
//! site.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use hintkit_spec::GeneratorKind;
use tracing::{debug, trace};

/// Wall-clock cap on subprocess generators (SPEC §4 "Each generator
/// has a hard 200 ms timeout. Killed if it exceeds.").
const SUBPROCESS_TIMEOUT: Duration = Duration::from_millis(200);

/// Dispatch a [`GeneratorKind`] to its native implementation in the
/// given working directory. Always returns — generator failures (no
/// git repo, missing package.json, permission denied on a dir, …)
/// surface as an empty `Vec` rather than an error.
pub fn resolve(kind: GeneratorKind, cwd: &Path) -> Vec<String> {
    match kind {
        GeneratorKind::FilePath => file_path(cwd),
        GeneratorKind::DirPath => dir_path(cwd),
        GeneratorKind::GitBranches => git_branches(cwd),
        GeneratorKind::PackageJsonScripts => package_json_scripts(cwd),
    }
}

fn file_path(cwd: &Path) -> Vec<String> {
    list_dir(cwd, |_| true)
}

fn dir_path(cwd: &Path) -> Vec<String> {
    list_dir(cwd, |entry| {
        entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
    })
}

fn list_dir(cwd: &Path, predicate: impl Fn(&fs::DirEntry) -> bool) -> Vec<String> {
    let read = match fs::read_dir(cwd) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        if !predicate(&entry) {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            out.push(name.to_string());
        }
    }
    out.sort();
    out
}

fn git_branches(cwd: &Path) -> Vec<String> {
    let mut cmd = Command::new("git");
    cmd.arg("branch")
        .arg("--format=%(refname:short)")
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    let raw = match run_with_timeout(cmd, SUBPROCESS_TIMEOUT) {
        Some(s) => s,
        None => return Vec::new(),
    };
    raw.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn package_json_scripts(cwd: &Path) -> Vec<String> {
    let path = cwd.join("package.json");
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(scripts) = value.get("scripts").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = scripts.keys().cloned().collect();
    names.sort();
    names
}

/// Spawn the configured subprocess, race its stdout drain against the
/// timer, and SIGKILL on timeout. Returns the stdout text on success
/// or `None` on any failure (spawn error, timeout, non-UTF-8).
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Option<String> {
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            debug!("generator subprocess failed to spawn: {e}");
            return None;
        }
    };
    let stdout = child.stdout.take()?;

    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut reader = stdout;
        let mut buf = String::new();
        let _ = reader.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });

    match rx.recv_timeout(timeout) {
        Ok(buf) => {
            // Best-effort wait so we don't leak the zombie; should be
            // already-exited at this point.
            let _ = child.wait();
            Some(buf)
        }
        Err(_) => {
            trace!("generator subprocess hit {SUBPROCESS_TIMEOUT:?} cap; killing");
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write as _;
    use std::path::PathBuf;

    /// A throwaway directory under the system temp dir for one test.
    /// Cleaned up by Drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "hintkit-test-{prefix}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            // Be aggressive: nuke any leftover from a prior crashed test.
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).expect("create tempdir");
            Self { path: p }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn file_path_lists_directory_contents() {
        let tmp = TempDir::new("file_path");
        File::create(tmp.path.join("alpha.txt")).unwrap();
        File::create(tmp.path.join("beta.rs")).unwrap();
        fs::create_dir(tmp.path.join("subdir")).unwrap();

        let names = file_path(&tmp.path);
        assert!(names.contains(&"alpha.txt".into()));
        assert!(names.contains(&"beta.rs".into()));
        assert!(names.contains(&"subdir".into()));
    }

    #[test]
    fn dir_path_includes_only_directories() {
        let tmp = TempDir::new("dir_path");
        File::create(tmp.path.join("a-file")).unwrap();
        fs::create_dir(tmp.path.join("a-dir")).unwrap();

        let names = dir_path(&tmp.path);
        assert!(!names.contains(&"a-file".into()));
        assert!(names.contains(&"a-dir".into()));
    }

    #[test]
    fn file_path_returns_empty_for_missing_dir() {
        let names = file_path(&PathBuf::from(
            "/tmp/this-path-definitely-does-not-exist-hintkit",
        ));
        assert!(names.is_empty());
    }

    #[test]
    fn package_json_scripts_returns_sorted_keys() {
        let tmp = TempDir::new("pkg_scripts");
        let pkg = tmp.path.join("package.json");
        let mut f = File::create(&pkg).unwrap();
        writeln!(
            f,
            r#"{{ "name": "x", "scripts": {{ "test": "jest", "build": "tsc", "lint": "eslint ." }} }}"#
        )
        .unwrap();

        let names = package_json_scripts(&tmp.path);
        assert_eq!(names, vec!["build", "lint", "test"]);
    }

    #[test]
    fn package_json_scripts_handles_missing_file() {
        let tmp = TempDir::new("pkg_missing");
        assert!(package_json_scripts(&tmp.path).is_empty());
    }

    #[test]
    fn package_json_scripts_handles_malformed_json() {
        let tmp = TempDir::new("pkg_malformed");
        let pkg = tmp.path.join("package.json");
        let mut f = File::create(&pkg).unwrap();
        write!(f, "this is not JSON").unwrap();
        assert!(package_json_scripts(&tmp.path).is_empty());
    }

    #[test]
    fn git_branches_outside_a_repo_returns_empty() {
        // A tempdir is guaranteed to not be a git repo (unless your
        // OS temp dir is the world's strangest place).
        let tmp = TempDir::new("git_outside");
        assert!(git_branches(&tmp.path).is_empty());
    }

    #[test]
    fn timeout_helper_caps_long_running_subprocess() {
        // `sleep 2` would take 2 s if uncapped; our timeout is 200 ms.
        let mut cmd = Command::new("sleep");
        cmd.arg("2")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
        let start = std::time::Instant::now();
        let result = run_with_timeout(cmd, Duration::from_millis(200));
        let elapsed = start.elapsed();
        assert!(result.is_none());
        assert!(
            elapsed < Duration::from_millis(500),
            "timeout was supposed to fire at 200 ms; elapsed = {elapsed:?}"
        );
    }
}
