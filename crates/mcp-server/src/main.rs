//! `mcp-server` binary — generic Rust-native MCP server framework.
//!
//! Boots a Model Context Protocol server over stdio and registers a
//! configurable set of MCP tools driven by a YAML manifest. Domain-
//! agnostic by design: the binary in the mcp-methods workspace ships
//! the *generic* tool surface (source navigation, GitHub access,
//! python extensions, file watching, workspace clone-and-track),
//! plus a hookable graph-build callback. Domain-specific binaries
//! (e.g. kglite's `kglite-mcp-server`) layer on top by calling
//! [`McpServer::new`] with their own pre-registered tools and an
//! active-graph provider.
//!
//! Operating modes:
//! - **bare** (no flag): framework only — `ping` plus any
//!   manifest-declared tools. Useful for testing the protocol layer.
//! - **`--source-root DIR`** *(or via manifest)*: file-tree mode.
//!   Source tools (`read_source`, `grep`, `list_source`) operate
//!   against the configured directory.
//! - **`--workspace DIR`** *(phase 6)*: clone-and-track mode.
//!   `repo_management` clones GitHub repos into the workspace,
//!   maintains an inventory, and points the source tools at the
//!   active repo.
//! - **`--watch DIR`** *(phase 5)*: file-watcher mode. Source roots
//!   stay pinned to the directory; downstream consumers register a
//!   rebuild callback on file changes.
//!
//! The manifest schema mirrors the legacy `kglite-mcp-server` Python
//! CLI 1:1, so a YAML written for that CLI boots unchanged here.
//! Auto-detected paths: `<workspace>/workspace_mcp.yaml` in workspace
//! and watch modes; otherwise pass `--mcp-config PATH` explicitly.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use rmcp::transport::stdio;
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

mod manifest;
mod server;
mod source;

use crate::manifest::{find_workspace_manifest, Manifest, ManifestError};
use crate::server::{McpServer, ServerOptions};

/// Operating mode picked from the CLI flags.
#[derive(Debug, Clone)]
enum Mode {
    /// Source-root mode — single fixed directory bound to the source tools.
    SourceRoot { dir: PathBuf },
    /// Workspace mode — clone-and-track flow, idle-sweep inventory.
    Workspace { dir: PathBuf },
    /// Watch mode — auto-rebuild trigger on file changes.
    Watch { dir: PathBuf },
    /// Framework only — no source binding. Useful for testing the
    /// protocol layer in isolation, or as a base for downstream
    /// binaries that register their own tools.
    Bare,
}

#[derive(Parser, Debug)]
#[command(
    name = "mcp-server",
    about = "Rust-native MCP server framework — source navigation + GitHub + python tools",
    long_about = "\
Boot a Model Context Protocol server over stdio. Generic by design: ships \
folder navigation (read_source / grep / list_source), GitHub access \
(github_issues / github_api), and a manifest-driven tool surface. \
Graph-specific tools (e.g. cypher_query) are layered on top by domain \
binaries like kglite-mcp-server.

Modes:
  (none)                  Bare framework — ping tool plus any manifest tools.
  --source-root DIR       Bind the source tools to a fixed directory.
  --workspace DIR         Clone-and-track GitHub repos in DIR (phase 6).
  --watch DIR             Watch DIR for changes; rebuild downstream artifacts (phase 5).

The manifest is auto-detected: <workspace>/workspace_mcp.yaml in workspace \
and watch modes. Override with --mcp-config PATH. The manifest's source_root \
declaration is the manifest-driven equivalent of --source-root.\
"
)]
struct Cli {
    /// Bind source tools (read_source / grep / list_source) to this directory.
    /// Equivalent to setting `source_root: DIR` in the manifest.
    #[arg(long = "source-root", conflicts_with_all = ["workspace", "watch"])]
    source_root: Option<PathBuf>,

    /// Workspace directory (clone-and-track GitHub repos; phase 6).
    #[arg(long, conflicts_with_all = ["source_root", "watch"])]
    workspace: Option<PathBuf>,

    /// Local directory to watch for file changes (phase 5).
    #[arg(long, conflicts_with_all = ["source_root", "workspace"])]
    watch: Option<PathBuf>,

    /// Optional manifest YAML path. Defaults to the auto-detected workspace yaml.
    #[arg(long = "mcp-config")]
    mcp_config: Option<PathBuf>,

    /// Override the server display name (otherwise manifest.name or default).
    #[arg(long)]
    name: Option<String>,

    /// Permit loading manifest-declared `python:` tool hooks AND custom embedders.
    /// Required alongside `trust.allow_python_tools: true` / `trust.allow_embedder: true`
    /// in the YAML — both signals must be present.
    #[arg(long = "trust-tools")]
    trust_tools: bool,

    /// Workspace mode only: auto-sweep repos idle for more than N days.
    #[arg(long = "stale-after-days", default_value_t = 7)]
    stale_after_days: u32,
}

fn pick_mode(cli: &Cli) -> Mode {
    match (&cli.source_root, &cli.workspace, &cli.watch) {
        (Some(d), _, _) => Mode::SourceRoot { dir: d.clone() },
        (_, Some(w), _) => Mode::Workspace { dir: w.clone() },
        (_, _, Some(w)) => Mode::Watch { dir: w.clone() },
        _ => Mode::Bare,
    }
}

fn default_manifest_path(mode: &Mode) -> Option<PathBuf> {
    match mode {
        Mode::SourceRoot { .. } => None,
        Mode::Workspace { dir } => find_workspace_manifest(dir),
        Mode::Watch { dir } => find_workspace_manifest(dir),
        Mode::Bare => None,
    }
}

fn load_manifest(cli: &Cli, mode: &Mode) -> Result<Option<Manifest>, ManifestError> {
    let path: Option<PathBuf> = match &cli.mcp_config {
        Some(p) => {
            if !p.is_file() {
                return Err(ManifestError::bare(format!(
                    "--mcp-config path does not exist: {}",
                    p.display()
                )));
            }
            Some(p.clone())
        }
        None => default_manifest_path(mode),
    };
    match path {
        Some(p) => Ok(Some(manifest::load(&p)?)),
        None => Ok(None),
    }
}

fn fallback_name(mode: &Mode) -> &'static str {
    match mode {
        Mode::SourceRoot { .. } => "MCP Server (source-root)",
        Mode::Workspace { .. } => "MCP Server (workspace)",
        Mode::Watch { .. } => "MCP Server (watch)",
        Mode::Bare => "MCP Server",
    }
}

/// Resolve manifest-declared source_roots relative to the yaml directory.
///
/// Each entry must canonicalize to an existing directory; failures bubble
/// as a [`ManifestError`] so a typo lands on stderr at boot rather than
/// surfacing later as a path-traversal rejection.
fn resolve_source_roots(manifest: &Manifest) -> Result<Vec<String>, ManifestError> {
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

fn print_boot_summary(mode: &Mode, manifest: Option<&Manifest>, source_roots: &[String]) {
    let mode_label = match mode {
        Mode::SourceRoot { dir } => format!("source-root [{}]", dir.display()),
        Mode::Workspace { dir } => format!("workspace [{}]", dir.display()),
        Mode::Watch { dir } => format!("watch [{}]", dir.display()),
        Mode::Bare => "bare framework".to_string(),
    };
    let mut parts = vec![format!("mode: {mode_label}")];
    if let Some(m) = manifest {
        parts.push(format!("manifest: {}", m.yaml_path.display()));
        if !m.tools.is_empty() {
            parts.push(format!("{} manifest tool(s)", m.tools.len()));
        }
    }
    if !source_roots.is_empty() {
        parts.push(format!("source roots: {source_roots:?}"));
    }
    eprintln!("mcp-server: {}", parts.join("; "));
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let mode = pick_mode(&cli);

    if let Mode::SourceRoot { dir } = &mode {
        if !dir.is_dir() {
            anyhow::bail!(
                "--source-root path does not exist or is not a directory: {}",
                dir.display()
            );
        }
    }

    let manifest = load_manifest(&cli, &mode).context("manifest load failed")?;
    let mut options = ServerOptions::from_manifest(manifest.as_ref(), fallback_name(&mode));
    if cli.name.is_some() {
        options.name = cli.name.clone();
    }

    // Wire source roots: --source-root flag takes precedence over manifest declaration.
    let mut source_roots: Vec<String> = Vec::new();
    if let Mode::SourceRoot { dir } = &mode {
        let canon = dir
            .canonicalize()
            .with_context(|| format!("failed to canonicalize --source-root {}", dir.display()))?;
        source_roots.push(canon.to_string_lossy().into_owned());
    } else if let Some(m) = manifest.as_ref() {
        if !m.source_roots.is_empty() {
            source_roots = resolve_source_roots(m).context("source root resolution failed")?;
        }
    }
    if !source_roots.is_empty() {
        options = options.with_static_source_roots(source_roots.clone());
    }
    print_boot_summary(&mode, manifest.as_ref(), &source_roots);

    let server = McpServer::new(options);
    let service = server
        .serve(stdio())
        .await
        .context("failed to start MCP service over stdio")?;
    service.waiting().await?;
    Ok(())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        let mut full = vec!["mcp-server"];
        full.extend_from_slice(args);
        Cli::parse_from(full)
    }

    #[test]
    fn no_flags_defaults_to_bare() {
        let mode = pick_mode(&cli(&[]));
        assert!(matches!(mode, Mode::Bare));
    }

    #[test]
    fn source_root_mode() {
        let mode = pick_mode(&cli(&["--source-root", "/tmp/src"]));
        match mode {
            Mode::SourceRoot { dir } => assert_eq!(dir, PathBuf::from("/tmp/src")),
            _ => panic!("expected SourceRoot"),
        }
    }

    #[test]
    fn workspace_mode() {
        let mode = pick_mode(&cli(&["--workspace", "/tmp/ws"]));
        match mode {
            Mode::Workspace { dir } => assert_eq!(dir, PathBuf::from("/tmp/ws")),
            _ => panic!("expected Workspace"),
        }
    }

    #[test]
    fn watch_mode() {
        let mode = pick_mode(&cli(&["--watch", "/tmp/src"]));
        match mode {
            Mode::Watch { dir } => assert_eq!(dir, PathBuf::from("/tmp/src")),
            _ => panic!("expected Watch"),
        }
    }

    #[test]
    fn source_root_and_workspace_mutually_exclusive() {
        let res = Cli::try_parse_from([
            "mcp-server",
            "--source-root",
            "/tmp/src",
            "--workspace",
            "/tmp",
        ]);
        assert!(res.is_err());
    }

    #[test]
    fn watch_and_workspace_mutually_exclusive() {
        let res = Cli::try_parse_from(["mcp-server", "--watch", "/tmp/a", "--workspace", "/tmp/b"]);
        assert!(res.is_err());
    }

    #[test]
    fn graph_flag_no_longer_recognised() {
        // mcp-methods's binary doesn't know about graphs — that's a kglite concept.
        let res = Cli::try_parse_from(["mcp-server", "--graph", "x.kgl"]);
        assert!(res.is_err());
    }
}
