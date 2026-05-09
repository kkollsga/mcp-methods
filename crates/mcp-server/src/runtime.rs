//! Boot-time helpers shared by the framework binary and downstream
//! domain binaries (e.g. `kglite-mcp-server`).
//!
//! Each helper is small enough to inline; collecting them here keeps
//! the duplication out of every shim's `main.rs` and gives a single
//! place to change boot-time behaviour.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use crate::manifest::{Manifest, ManifestError, ToolSpec};
use crate::server::McpServer;
use crate::{python, watch};

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

/// Outcome of [`apply_python_extensions`].
#[derive(Debug, Default)]
pub struct PythonExtensions {
    /// Number of `python:` tools registered on the router.
    pub python_tool_count: usize,
    /// The instantiated embedder, when the manifest declared one. The
    /// caller is responsible for binding it to whatever consumes the
    /// embedder (e.g. `KnowledgeGraph::set_embedder`).
    pub embedder: Option<pyo3::Py<pyo3::PyAny>>,
}

/// Wire manifest-declared `python:` tools and load the embedder
/// factory (if any). Both surfaces are trust-gated: each requires
/// the matching `trust.*: true` in the manifest *and* the operator's
/// `--trust-tools` flag (passed in as `trust_tools`).
///
/// The embedder is *returned*, not bound — the caller decides what to
/// do with it. Domain binaries (e.g. kglite) typically pass it to a
/// graph via a `set_embedder` method; the framework binary just logs.
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

    let embedder = if let Some(cfg) = manifest.embedder.as_ref() {
        let manifest_dir = manifest
            .yaml_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        Some(python::load_embedder(cfg, &manifest_dir).context("embedder factory load failed")?)
    } else {
        None
    };

    Ok(PythonExtensions {
        python_tool_count,
        embedder,
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
