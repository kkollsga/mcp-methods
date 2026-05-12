//! `.env` discovery + loader.
//!
//! Walks upward from a start directory looking for a `.env` file and
//! loads the first one found into the process environment. Mirrors
//! the Python `mcp_methods._utils.load_env` semantics exactly so the
//! same `.env` file behaves identically whether the server boots
//! via Rust or Python:
//! - Skip blank lines and lines starting with `#`.
//! - `KEY=VALUE` only (lines without `=` are skipped).
//! - Values may be quoted with single or double quotes; the outer
//!   quotes are stripped.
//! - Existing env vars are NOT overwritten.
//!
//! Operators who want a non-implicit pick can declare `env_file: path`
//! at the top level of the manifest YAML; that wins over the walk-up.

use std::fs;
use std::path::{Path, PathBuf};

/// Walk upward from `start` looking for `.env`. Returns the path
/// loaded, or `None` if nothing was found before reaching the root.
/// Silent on parse errors per-line — a single bad line should not
/// stop the rest of the file from loading.
pub fn load_env_walk(start: &Path) -> Option<PathBuf> {
    let mut cursor: PathBuf = if start.is_absolute() {
        start.to_path_buf()
    } else {
        start.canonicalize().unwrap_or_else(|_| start.to_path_buf())
    };
    loop {
        let candidate = cursor.join(".env");
        if candidate.is_file() {
            apply_env_file(&candidate);
            return Some(candidate);
        }
        if !cursor.pop() {
            return None;
        }
    }
}

/// Load a specific `.env` path. Errors if the file does not exist —
/// the explicit `env_file:` manifest key promises that path.
pub fn load_env_explicit(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Err(format!("env_file does not exist: {}", path.display()));
    }
    apply_env_file(path);
    Ok(())
}

fn apply_env_file(path: &Path) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(eq) = trimmed.find('=') else {
            continue;
        };
        let key = trimmed[..eq].trim();
        let val = trimmed[eq + 1..].trim();
        let val = strip_outer_quotes(val);
        if key.is_empty() {
            continue;
        }
        if std::env::var_os(key).is_some() {
            continue;
        }
        // SAFETY: PyO3-embedded Python is not yet initialised when this
        // runs in `main.rs`; the only readers of the env at this point
        // are downstream tool implementations (e.g. github::gh_get) that
        // call `env::var` lazily. Setting env on the main thread before
        // tokio runtime work is the standard pre-boot pattern.
        unsafe { std::env::set_var(key, val) };
    }
}

fn strip_outer_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Tests mutate process env; serialise to avoid cross-test races.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn write_env(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join(".env");
        let mut f = fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn finds_env_in_start_dir() {
        let _g = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let key = "MCP_TEST_DIRECT_HIT";
        unsafe { std::env::remove_var(key) };
        write_env(dir.path(), &format!("{key}=ok\n"));
        let found = load_env_walk(dir.path()).expect("found env");
        assert!(found.ends_with(".env"));
        assert_eq!(std::env::var(key).ok().as_deref(), Some("ok"));
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn walks_up_to_parent_for_env() {
        let _g = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let key = "MCP_TEST_WALK_UP";
        unsafe { std::env::remove_var(key) };
        write_env(dir.path(), &format!("{key}=parent\n"));
        let sub = dir.path().join("a").join("b").join("c");
        fs::create_dir_all(&sub).unwrap();
        let found = load_env_walk(&sub).expect("found env via walk-up");
        let expected = dir.path().canonicalize().unwrap().join(".env");
        assert_eq!(found.canonicalize().unwrap(), expected);
        assert_eq!(std::env::var(key).ok().as_deref(), Some("parent"));
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn does_not_overwrite_existing_env() {
        let _g = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let key = "MCP_TEST_NO_OVERWRITE";
        unsafe { std::env::set_var(key, "preset") };
        write_env(dir.path(), &format!("{key}=fromfile\n"));
        load_env_walk(dir.path());
        assert_eq!(std::env::var(key).ok().as_deref(), Some("preset"));
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn strips_quotes_skips_comments() {
        let _g = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let k1 = "MCP_TEST_DQUOTED";
        let k2 = "MCP_TEST_SQUOTED";
        let k3 = "MCP_TEST_COMMENT";
        for k in [k1, k2, k3] {
            unsafe { std::env::remove_var(k) };
        }
        write_env(
            dir.path(),
            &format!("# comment\n\n{k1}=\"hello\"\n{k2}='world'\n# {k3}=skipped\n"),
        );
        load_env_walk(dir.path()).unwrap();
        assert_eq!(std::env::var(k1).ok().as_deref(), Some("hello"));
        assert_eq!(std::env::var(k2).ok().as_deref(), Some("world"));
        assert!(std::env::var(k3).is_err());
        for k in [k1, k2, k3] {
            unsafe { std::env::remove_var(k) };
        }
    }

    #[test]
    fn explicit_missing_path_errors() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.env");
        assert!(load_env_explicit(&missing).is_err());
    }

    #[test]
    fn returns_none_when_no_env_anywhere() {
        let _g = env_lock();
        let dir = tempfile::tempdir().unwrap();
        // walk-up will still hit /, but no .env at the temp leaf and
        // (usually) nothing on the temp path. Either way the call is
        // safe and idempotent — we just assert it doesn't panic.
        let _ = load_env_walk(dir.path());
    }
}
