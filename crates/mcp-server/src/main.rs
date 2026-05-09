//! `mcp-server` binary — Rust-native MCP server framework.
//!
//! Phase 1 capabilities:
//! - Loads + validates a YAML manifest (auto-detected sibling, or via `--mcp-config`).
//! - Boots an rmcp stdio server and registers a single `ping` tool.
//! - CLI surface and operating modes (`--graph`, `--workspace`, `--watch`)
//!   are accepted; only the boot path is wired so far. Tool registration
//!   per-mode lands in subsequent phases (source tools, github tools,
//!   python extension layer, watch mode, workspace mode).
//!
//! The intent is that a manifest written for the legacy
//! `kglite-mcp-server` Python CLI boots unchanged on this binary —
//! same keys, same semantics. Graph-specific tool registration is
//! deferred to a downstream binary (kglite's own server crate) that
//! depends on this crate.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use rmcp::transport::stdio;
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

mod manifest;
mod server;

use crate::manifest::{find_sibling_manifest, find_workspace_manifest, Manifest, ManifestError};
use crate::server::{McpServer, ServerOptions};

/// Operating mode picked from the CLI flags.
#[derive(Debug, Clone)]
enum Mode {
    /// Single-graph mode — load one .kgl file and serve cypher/source tools.
    SingleGraph { graph: PathBuf },
    /// Workspace mode — clone-and-build flow, idle-sweep inventory.
    Workspace { dir: PathBuf },
    /// Watch mode — auto-rebuild a code graph from a local directory.
    Watch { dir: PathBuf },
    /// Framework only — no graph or source binding. Useful for testing
    /// the protocol layer in isolation.
    Bare,
}

#[derive(Parser, Debug)]
#[command(
    name = "mcp-server",
    about = "Rust-native MCP server framework + binary",
    long_about = "\
Boot a Model Context Protocol server over stdio. Accepts the same YAML \
manifest schema as the legacy kglite-mcp-server Python CLI.

Modes:
  (none)                  Bare framework — registers a ping tool only.
  --graph X.kgl           Single-graph mode (graph tool surface coming in a later phase).
  --workspace DIR         Workspace mode (clone+build via repo_management).
  --watch DIR             Watch mode (rebuild code graph on file changes).

The manifest is auto-detected: ``<basename>_mcp.yaml`` next to the graph in \
single-graph mode, ``<workspace>/workspace_mcp.yaml`` in workspace mode. \
Override with --mcp-config PATH."
)]
struct Cli {
    /// Path to .kgl file (single-graph mode). Mutually exclusive with --workspace and --watch.
    #[arg(long, conflicts_with_all = ["workspace", "watch"])]
    graph: Option<PathBuf>,

    /// Workspace directory (multi-graph clone-and-build mode).
    #[arg(long, conflicts_with_all = ["graph", "watch"])]
    workspace: Option<PathBuf>,

    /// Local directory to watch (auto-rebuild a code-tree graph on changes).
    #[arg(long, conflicts_with_all = ["graph", "workspace"])]
    watch: Option<PathBuf>,

    /// Optional manifest YAML path. Defaults to the auto-detected sibling/workspace yaml.
    #[arg(long = "mcp-config")]
    mcp_config: Option<PathBuf>,

    /// Sentence-transformers model shortcut (only effective when manifest doesn't override).
    #[arg(long)]
    embedder: Option<String>,

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
    match (&cli.graph, &cli.workspace, &cli.watch) {
        (Some(g), _, _) => Mode::SingleGraph { graph: g.clone() },
        (_, Some(w), _) => Mode::Workspace { dir: w.clone() },
        (_, _, Some(w)) => Mode::Watch { dir: w.clone() },
        _ => Mode::Bare,
    }
}

fn default_manifest_path(mode: &Mode) -> Option<PathBuf> {
    match mode {
        Mode::SingleGraph { graph } => find_sibling_manifest(graph),
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
        Mode::SingleGraph { .. } => "MCP Server (single-graph)",
        Mode::Workspace { .. } => "MCP Server (workspace)",
        Mode::Watch { .. } => "MCP Server (watch)",
        Mode::Bare => "MCP Server",
    }
}

fn print_boot_summary(mode: &Mode, manifest: Option<&Manifest>) {
    let mode_label = match mode {
        Mode::SingleGraph { graph } => format!("single-graph [{}]", graph.display()),
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
        if !m.source_roots.is_empty() {
            parts.push(format!("source roots: {:?}", m.source_roots));
        }
    }
    eprintln!("mcp-server: {}", parts.join("; "));
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let mode = pick_mode(&cli);

    if let Mode::SingleGraph { graph } = &mode {
        if !graph.exists() {
            anyhow::bail!("--graph path does not exist: {}", graph.display());
        }
    }

    let manifest = load_manifest(&cli, &mode).context("manifest load failed")?;
    let mut options = ServerOptions::from_manifest(manifest.as_ref(), fallback_name(&mode));
    if cli.name.is_some() {
        options.name = cli.name.clone();
    }
    print_boot_summary(&mode, manifest.as_ref());

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
    fn single_graph_mode() {
        let mode = pick_mode(&cli(&["--graph", "x.kgl"]));
        match mode {
            Mode::SingleGraph { graph } => assert_eq!(graph, PathBuf::from("x.kgl")),
            _ => panic!("expected SingleGraph"),
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
    fn graph_and_workspace_mutually_exclusive() {
        let res = Cli::try_parse_from(["mcp-server", "--graph", "x.kgl", "--workspace", "/tmp"]);
        assert!(res.is_err());
    }

    #[test]
    fn watch_and_workspace_mutually_exclusive() {
        let res = Cli::try_parse_from(["mcp-server", "--watch", "/tmp/a", "--workspace", "/tmp/b"]);
        assert!(res.is_err());
    }
}
