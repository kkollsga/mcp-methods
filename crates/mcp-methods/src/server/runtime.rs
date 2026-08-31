//! Boot-time helpers shared by the framework binary and downstream
//! domain binaries (e.g. `kglite-mcp-server`).
//!
//! Each helper is small enough to inline; collecting them here keeps
//! the duplication out of every shim's `main.rs` and gives a single
//! place to change boot-time behaviour.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use crate::server::env;
use crate::server::manifest::{Manifest, ManifestError};
use crate::server::watch;

/// Initialise stderr-only `tracing` with `RUST_LOG=info` default.
///
/// Safe to call multiple times — `try_init()` is a no-op if a global
/// subscriber is already installed.
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

/// Load environment variables from a `.env` file before any tool that
/// reads `GITHUB_TOKEN` / API credentials runs.
///
/// Resolution order:
/// 1. If the manifest sets `env_file:`, load that path (error if missing).
/// 2. Otherwise walk upward from `start_dir` looking for a `.env`.
///
/// Returns the path actually loaded (for boot-summary logging), or
/// `None` if nothing was found. Existing env vars are never overwritten.
pub fn load_env_for_mode(manifest: Option<&Manifest>, start_dir: &Path) -> Result<Option<PathBuf>> {
    if let Some(m) = manifest {
        if let Some(rel) = m.env_file.as_ref() {
            let base = m
                .yaml_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            let resolved = base.join(rel);
            env::load_env_explicit(&resolved).map_err(anyhow::Error::msg)?;
            return Ok(Some(resolved));
        }
    }
    Ok(env::load_env_walk(start_dir))
}

/// A `source_root(s)` entry that could not be resolved, as reported by
/// [`resolve_source_roots_lenient`].
///
/// Carries enough for a caller to log the failure and surface it in a
/// boot summary without re-deriving anything: the entry exactly as the
/// manifest declared it, the path it was joined to, and the same
/// [`ManifestError`] the strict [`resolve_source_roots`] would have
/// returned.
///
/// Not `Clone` only because [`ManifestError`] is not.
#[derive(Debug)]
pub struct UnresolvedSourceRoot {
    /// The entry verbatim from `source_root:` / `source_roots:`.
    pub declared: String,
    /// `declared` joined onto the manifest's directory — the path that
    /// was expected to be an existing directory.
    pub path: PathBuf,
    /// Why it failed, naming the manifest and the path.
    pub error: ManifestError,
}

/// Directory a manifest's relative paths resolve against.
fn manifest_dir(manifest: &Manifest) -> PathBuf {
    manifest
        .yaml_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Resolve one `source_root(s)` entry against the manifest's directory.
/// Shared by the strict and lenient resolvers so a failure reads
/// identically whichever one reported it.
fn resolve_one_source_root(
    manifest: &Manifest,
    base: &Path,
    raw: &str,
) -> Result<String, UnresolvedSourceRoot> {
    let candidate = base.join(raw);
    let fail = |message: String| UnresolvedSourceRoot {
        declared: raw.to_string(),
        path: candidate.clone(),
        error: ManifestError::at(&manifest.yaml_path, message),
    };
    let canon = candidate.canonicalize().map_err(|_| {
        fail(format!(
            "source root {raw:?} resolves to {:?} which is not an existing directory",
            candidate.display()
        ))
    })?;
    if !canon.is_dir() {
        return Err(fail(format!(
            "source root {raw:?} resolves to {:?} which is not a directory",
            canon.display()
        )));
    }
    Ok(canon.to_string_lossy().into_owned())
}

/// Resolve a manifest's `source_root(s)` declarations to canonical
/// absolute path strings. Each entry must canonicalise to an existing
/// directory; failures bubble as a [`ManifestError`].
///
/// All-or-nothing on purpose: this is the *validation* entry point —
/// linters, `--selftest`-style checks, anything that wants a manifest
/// declared broken rather than partly served. A boot path that should
/// degrade instead of dying wants [`resolve_source_roots_lenient`].
pub fn resolve_source_roots(manifest: &Manifest) -> Result<Vec<String>, ManifestError> {
    let base = manifest_dir(manifest);
    let mut resolved: Vec<String> = Vec::new();
    for raw in &manifest.source_roots {
        resolved.push(resolve_one_source_root(manifest, &base, raw).map_err(|u| u.error)?);
    }
    Ok(resolved)
}

/// Per-root sibling of [`resolve_source_roots`]: resolve every
/// `source_root(s)` entry independently and return the ones that
/// worked alongside the ones that did not.
///
/// A missing source directory disables `read_source` / `grep` /
/// `list_source` for that root; it does not make the server
/// unserveable. A boot path can therefore `warn!` per failure, record
/// the failures in its boot summary, serve what resolved, and still
/// answer `initialize`. Never returns `Err` — with every entry broken
/// the resolved list is simply empty.
pub fn resolve_source_roots_lenient(
    manifest: &Manifest,
) -> (Vec<String>, Vec<UnresolvedSourceRoot>) {
    let base = manifest_dir(manifest);
    let mut resolved: Vec<String> = Vec::new();
    let mut unresolved: Vec<UnresolvedSourceRoot> = Vec::new();
    for raw in &manifest.source_roots {
        match resolve_one_source_root(manifest, &base, raw) {
            Ok(path) => resolved.push(path),
            Err(u) => unresolved.push(u),
        }
    }
    (resolved, unresolved)
}

/// Spawn the framework's debounced filesystem watcher when the mode
/// requires one. Returns the handle (drop to stop watching) or `None`
/// if `dir` is `None` — useful for `let _watch = …;` bindings in
/// downstream main fns.
pub fn maybe_watch(
    dir: Option<&Path>,
    on_change: Option<watch::ChangeHandler>,
) -> Result<Option<watch::WatchHandle>> {
    let Some(d) = dir else { return Ok(None) };
    let handle = watch::watch(d, on_change, None).context("failed to start file watcher")?;
    Ok(Some(handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::manifest;

    /// A manifest declaring `a`, `missing`, `b` where only `a` and `b`
    /// exist on disk. Returns the tempdir (kept alive by the caller)
    /// and the loaded manifest.
    fn manifest_with_one_missing_root() -> (tempfile::TempDir, Manifest) {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        std::fs::create_dir(base.join("a")).unwrap();
        std::fs::create_dir(base.join("b")).unwrap();
        let yaml = base.join("roots_mcp.yaml");
        std::fs::write(&yaml, "source_roots:\n  - a\n  - missing\n  - b\n").unwrap();
        let m = manifest::load(&yaml).unwrap();
        assert_eq!(m.source_roots.len(), 3, "fixture must declare three roots");
        (td, m)
    }

    #[test]
    fn lenient_resolution_serves_the_roots_that_exist() {
        let (_td, m) = manifest_with_one_missing_root();
        let (resolved, unresolved) = resolve_source_roots_lenient(&m);

        assert_eq!(
            resolved.len(),
            2,
            "expected the two existing roots to resolve, got {resolved:?}"
        );
        assert!(
            resolved.iter().all(|p| Path::new(p).is_absolute()),
            "lenient resolution must return canonical absolute paths, got {resolved:?}"
        );
        assert!(
            resolved[0].ends_with("/a") && resolved[1].ends_with("/b"),
            "resolved roots must keep manifest order, got {resolved:?}"
        );

        assert_eq!(
            unresolved.len(),
            1,
            "expected exactly one failure, got {unresolved:?}"
        );
        let bad = &unresolved[0];
        assert_eq!(bad.declared, "missing");
        assert!(
            bad.path.ends_with("missing"),
            "failure must carry the path it tried, got {:?}",
            bad.path
        );
        assert!(
            bad.error.message.contains("missing")
                && bad.error.message.contains("not an existing directory"),
            "the error must name the missing path and say why: {}",
            bad.error.message
        );
        assert!(
            bad.error.path.ends_with("roots_mcp.yaml"),
            "the error must name the manifest, got {}",
            bad.error.path
        );
    }

    #[test]
    fn strict_resolution_still_fails_on_the_same_manifest() {
        let (_td, m) = manifest_with_one_missing_root();
        let err = resolve_source_roots(&m)
            .expect_err("strict resolution must stay all-or-nothing for validation callers");
        assert!(
            err.message.contains("missing"),
            "strict error must name the offending root: {}",
            err.message
        );
    }

    #[test]
    fn lenient_resolution_reports_no_failures_when_every_root_exists() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        std::fs::create_dir(base.join("a")).unwrap();
        let yaml = base.join("ok_mcp.yaml");
        std::fs::write(&yaml, "source_root: a\n").unwrap();
        let m = manifest::load(&yaml).unwrap();

        let (resolved, unresolved) = resolve_source_roots_lenient(&m);
        assert_eq!(resolved.len(), 1);
        assert!(
            unresolved.is_empty(),
            "a healthy manifest must report no unresolved roots, got {unresolved:?}"
        );
        assert_eq!(resolve_source_roots(&m).unwrap(), resolved);
    }

    #[test]
    fn lenient_resolution_rejects_a_root_that_is_a_file() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        std::fs::write(base.join("notadir"), "x").unwrap();
        let yaml = base.join("file_mcp.yaml");
        std::fs::write(&yaml, "source_root: notadir\n").unwrap();
        let m = manifest::load(&yaml).unwrap();

        let (resolved, unresolved) = resolve_source_roots_lenient(&m);
        assert!(resolved.is_empty(), "a file must not be served as a root");
        assert_eq!(unresolved.len(), 1);
        assert!(
            unresolved[0].error.message.contains("not a directory"),
            "error must say the root is not a directory: {}",
            unresolved[0].error.message
        );
    }

    #[test]
    fn explicit_missing_env_file_is_an_error_that_names_the_path() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        let yaml = base.join("env_mcp.yaml");
        std::fs::write(&yaml, "env_file: stash/absent.env\n").unwrap();
        let m = manifest::load(&yaml).unwrap();

        let err = load_env_for_mode(Some(&m), &base)
            .expect_err("an explicit env_file: that does not exist must be reported");
        let text = format!("{err:#}");
        assert!(
            text.contains("env_file does not exist") && text.contains("absent.env"),
            "the env_file error must name the path so a caller can log it: {text}"
        );
    }
}
