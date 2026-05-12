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

/// Resolve a manifest's `source_root(s)` declarations to canonical
/// absolute path strings. Each entry must canonicalise to an existing
/// directory; failures bubble as a [`ManifestError`].
pub fn resolve_source_roots(manifest: &Manifest) -> Result<Vec<String>, ManifestError> {
    let base = manifest
        .yaml_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let mut resolved: Vec<String> = Vec::new();
    for raw in &manifest.source_roots {
        let candidate = base.join(raw);
        let canon = candidate.canonicalize().map_err(|_| {
            ManifestError::at(
                &manifest.yaml_path,
                format!(
                    "source root {raw:?} resolves to {:?} which is not an existing directory",
                    candidate.display()
                ),
            )
        })?;
        if !canon.is_dir() {
            return Err(ManifestError::at(
                &manifest.yaml_path,
                format!(
                    "source root {raw:?} resolves to {:?} which is not a directory",
                    canon.display()
                ),
            ));
        }
        resolved.push(canon.to_string_lossy().into_owned());
    }
    Ok(resolved)
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
