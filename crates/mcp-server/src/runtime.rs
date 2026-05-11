//! Boot-time helpers shared by the framework binary and downstream
//! domain binaries (e.g. `kglite-mcp-server`).
//!
//! Each helper is small enough to inline; collecting them here keeps
//! the duplication out of every shim's `main.rs` and gives a single
//! place to change boot-time behaviour.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use crate::env;
use crate::manifest::{Manifest, ManifestError};
use crate::watch;

#[cfg(feature = "python")]
use std::sync::Arc;
#[cfg(feature = "python")]
use std::time::Duration;

#[cfg(feature = "python")]
use crate::embedder::{self, EmbedderHandle};
#[cfg(feature = "python")]
use crate::manifest::ToolSpec;
#[cfg(feature = "python")]
use crate::python;
#[cfg(feature = "python")]
use crate::server::McpServer;

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

/// Outcome of [`apply_python_extensions`]. Available only with the
/// `python` Cargo feature (on by default).
#[cfg(feature = "python")]
#[derive(Default)]
pub struct PythonExtensions {
    /// Number of `python:` tools registered on the router.
    pub python_tool_count: usize,
    /// Lifecycle-aware wrapper around the manifest-loaded embedder.
    /// Downstream consumers (e.g. `KnowledgeGraph::set_embedder`) read
    /// the raw `Py<PyAny>` via [`EmbedderHandle::instance`] but should
    /// drive embedding calls through the handle so the idle-watch
    /// timer sees activity.
    pub embedder: Option<Arc<EmbedderHandle>>,
    /// Cooldown extracted from `embedder.kwargs.cooldown`, when set.
    /// `None` means "no idle eviction" — the embedder lives for the
    /// process lifetime.
    pub embedder_cooldown: Option<Duration>,
    /// If a cooldown is configured, this is the abort handle of the
    /// tokio task driving the idle-unload check. Drop it to stop
    /// watching (e.g. on graceful shutdown).
    pub embedder_watcher: Option<tokio::task::AbortHandle>,
}

#[cfg(feature = "python")]
impl std::fmt::Debug for PythonExtensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PythonExtensions")
            .field("python_tool_count", &self.python_tool_count)
            .field("embedder", &self.embedder.as_ref().map(|_| "<handle>"))
            .field("embedder_cooldown", &self.embedder_cooldown)
            .field(
                "embedder_watcher",
                &self.embedder_watcher.as_ref().map(|_| "<task>"),
            )
            .finish()
    }
}

/// Wire manifest-declared `python:` tools and load the embedder
/// factory (if any). Both surfaces are trust-gated: each requires
/// the matching `trust.*: true` in the manifest *and* the operator's
/// `--trust-tools` flag (passed in as `trust_tools`).
///
/// The embedder is *returned*, not bound — the caller decides what to
/// do with it. Domain binaries (e.g. kglite) typically pass it to a
/// graph via a `set_embedder` method; the framework binary just logs.
///
/// Available only with the `python` Cargo feature (on by default).
#[cfg(feature = "python")]
pub fn apply_python_extensions(
    server: &mut McpServer,
    manifest: &Manifest,
    trust_tools: bool,
) -> Result<PythonExtensions> {
    let has_python_tools = manifest
        .tools
        .iter()
        .any(|t| matches!(t, ToolSpec::Python(_)));
    let has_embedder = manifest.embedder.is_some();
    if !has_python_tools && !has_embedder {
        return Ok(PythonExtensions::default());
    }

    if has_python_tools {
        if !manifest.trust.allow_python_tools {
            anyhow::bail!(
                "manifest declares `python:` tools but `trust.allow_python_tools: true` is not set"
            );
        }
        if !trust_tools {
            anyhow::bail!(
                "manifest declares `python:` tools but the CLI was started without --trust-tools \
                 (refusing to load arbitrary code)"
            );
        }
    }
    if has_embedder {
        if !manifest.trust.allow_embedder {
            anyhow::bail!(
                "manifest declares an embedder but `trust.allow_embedder: true` is not set"
            );
        }
        if !trust_tools {
            anyhow::bail!(
                "manifest declares an embedder but the CLI was started without --trust-tools"
            );
        }
    }

    python::ensure_python().context("Python interpreter failed to initialise")?;

    let python_tool_count = if has_python_tools {
        python::register_python_tools(server.tool_router_mut(), manifest)
            .context("python tool registration failed")?
    } else {
        0
    };

    let (embedder, embedder_cooldown, embedder_watcher) =
        if let Some(cfg) = manifest.embedder.as_ref() {
            let manifest_dir = manifest
                .yaml_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            let py_instance = python::load_embedder(cfg, &manifest_dir)
                .context("embedder factory load failed")?;
            let handle = Arc::new(EmbedderHandle::new(py_instance));
            let cooldown = embedder::extract_cooldown(&cfg.kwargs);
            let watcher = cooldown.map(|d| embedder::spawn_idle_watch(handle.clone(), d));
            (Some(handle), cooldown, watcher)
        } else {
            (None, None, None)
        };

    Ok(PythonExtensions {
        python_tool_count,
        embedder,
        embedder_cooldown,
        embedder_watcher,
    })
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
