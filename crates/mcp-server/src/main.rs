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
//! - **`--workspace DIR`**: clone-and-track GitHub flow.
//!   `repo_management` clones GitHub repos into the workspace,
//!   maintains an inventory (with auto-rebuild gating via
//!   `last_built_sha`), and points the source tools at the active
//!   repo.
//! - **manifest `workspace: { kind: local, root, watch }`**: local-
//!   workspace flow. A fixed directory is bound as the active source
//!   root; rebuilds are triggered via `repo_management(update=True)`
//!   (gated by a cheap recursive-mtime fingerprint), and the active
//!   root can be swapped at runtime via the `set_root_dir(path)`
//!   tool. Manifest declarations win over the CLI `--workspace` flag.
//! - **`--watch DIR`**: file-watcher mode. Source roots stay pinned
//!   to the directory; downstream consumers register a rebuild
//!   callback on file changes.
//!
//! Boot sequence: parse CLI → load manifest → manifest `workspace:`
//! overrides CLI-derived mode if set → `.env` walk-up (or explicit
//! `env_file:`) → build ServerOptions → register dynamic tools →
//! apply Python extensions (manifest-declared `python:` tools and
//! embedder factory under trust gates) → spawn watcher if configured
//! → serve over stdio.
//!
//! The manifest schema mirrors the legacy `kglite-mcp-server` Python
//! CLI 1:1, so a YAML written for that CLI boots unchanged here.
//! Auto-detected paths: `<workspace>/workspace_mcp.yaml` in workspace,
//! watch, and local-workspace modes; otherwise pass `--mcp-config PATH`
//! explicitly.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmcp::transport::stdio;
use rmcp::ServiceExt;

use mcp_methods::server::manifest::{self, find_workspace_manifest, Manifest, ManifestError};
use mcp_methods::server::{
    cli as skills_cli, init_tracing, load_env_for_mode, maybe_watch, resolve_source_roots,
    workspace, McpServer, ServerOptions,
};

/// Operating mode picked from the CLI flags and the manifest's
/// optional `workspace:` block. Manifest declarations win over CLI
/// flags (same precedence rule as `source_root:`).
#[derive(Debug, Clone)]
enum Mode {
    /// Source-root mode — single fixed directory bound to the source tools.
    SourceRoot { dir: PathBuf },
    /// Workspace mode (github flavour) — clone-and-track flow, idle-sweep inventory.
    Workspace { dir: PathBuf },
    /// Workspace mode (local flavour) — bind a fixed local dir + fire
    /// post-activate hook on every change. Set via `workspace.kind: local`
    /// in the manifest. `sandbox_root` is the optional containment
    /// boundary for runtime `set_root_dir` swaps (`None` = unbounded,
    /// the default).
    LocalWorkspace {
        root: PathBuf,
        watch: bool,
        sandbox_root: Option<PathBuf>,
    },
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

Modes (set via CLI flag or manifest `workspace:` block):
  (none)                  Bare framework — ping tool plus any manifest tools.
  --source-root DIR       Bind the source tools to a fixed directory.
  --workspace DIR         Clone-and-track GitHub repos in DIR.
  --watch DIR             Watch DIR for changes; rebuild downstream artifacts.
  manifest workspace.kind: local
                          Bind a fixed local directory (no clone). Use
                          set_root_dir(path) to swap. Optionally watch.

The manifest is auto-detected at <dir>/workspace_mcp.yaml in workspace, \
watch, and local-workspace modes. Override with --mcp-config PATH. The \
manifest's source_root declaration is the manifest-driven equivalent of \
--source-root; manifest `workspace.kind: local` wins over --workspace.\
"
)]
struct Cli {
    /// Bind source tools (read_source / grep / list_source) to this directory.
    /// Equivalent to setting `source_root: DIR` in the manifest.
    #[arg(long = "source-root", conflicts_with_all = ["workspace", "watch"])]
    source_root: Option<PathBuf>,

    /// Workspace directory (clone-and-track GitHub repos).
    #[arg(long, conflicts_with_all = ["source_root", "watch"])]
    workspace: Option<PathBuf>,

    /// Local directory to watch for file changes.
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

    /// Optional skills-related subcommand. When set, runs the named
    /// command and exits without booting the MCP server.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Skills-related subcommands. Stay opt-in so the default invocation
/// (`mcp-server --workspace …`) keeps its existing semantics. Clap
/// inflects each variant to a hyphenated subcommand (e.g.
/// `Command::SkillsLint` → `mcp-server skills-lint`); the common
/// `Skills` prefix is intentional and visible to operators.
#[derive(Subcommand, Debug)]
#[allow(clippy::enum_variant_names)]
enum Command {
    /// Validate every SKILL.md in `path` against the framework's
    /// schema (frontmatter, required fields, size limits). Exits 0 on
    /// clean, 1 on any hard error.
    SkillsLint {
        /// Directory containing SKILL.md files.
        path: PathBuf,
    },
    /// List resolved skills for a manifest. Three-layer composition
    /// (project → domain-pack → bundled) is applied; output shows
    /// which layer each skill came from.
    SkillsList {
        /// Manifest YAML path.
        #[arg(long = "mcp-config")]
        mcp_config: PathBuf,
        /// Skip bundled framework defaults from the listing.
        #[arg(long = "no-bundled")]
        no_bundled: bool,
    },
    /// Print the full body of one resolved skill.
    SkillsShow {
        /// Manifest YAML path.
        #[arg(long = "mcp-config")]
        mcp_config: PathBuf,
        /// Name of the skill to show.
        name: String,
        /// Skip bundled framework defaults when resolving.
        #[arg(long = "no-bundled")]
        no_bundled: bool,
    },
    /// Scaffold a starter SKILL.md at the chosen destination. If
    /// `dest` is a directory (existing or to-be-created), the file
    /// is written to `<dest>/<name>.md`. If `dest` ends in `.md`,
    /// it is used verbatim. Refuses to overwrite an existing file.
    SkillsNew {
        /// Destination directory or explicit `.md` path.
        dest: PathBuf,
        /// Skill name. Becomes the lookup key for `prompts/get` and
        /// the filename when `dest` is a directory.
        name: String,
        /// Description shown in `prompts/list`. Required — the agent's
        /// only signal for triggering. Aim for 80-140 words with
        /// explicit TRIGGER / SKIP language; see the
        /// writing-effective-skills guide.
        description: String,
    },
}

fn pick_mode(cli: &Cli) -> Mode {
    match (&cli.source_root, &cli.workspace, &cli.watch) {
        (Some(d), _, _) => Mode::SourceRoot { dir: d.clone() },
        (_, Some(w), _) => Mode::Workspace { dir: w.clone() },
        (_, _, Some(w)) => Mode::Watch { dir: w.clone() },
        _ => Mode::Bare,
    }
}

/// Resolve a `workspace.kind: local` block into [`Mode::LocalWorkspace`].
///
/// Both `root` and `sandbox_root` resolve against the **manifest YAML's own
/// directory** (a manifest is portable; the process CWD is not) and are
/// canonicalized, so a path that does not exist is a boot error rather than
/// a boundary that silently never matches. `sandbox_root` absent is the
/// default and means unbounded `set_root_dir` swaps.
fn local_workspace_mode(
    wcfg: &mcp_methods::server::WorkspaceConfig,
    yaml_path: &Path,
) -> Result<Mode> {
    let raw_root = wcfg.root.as_ref().expect("validated by manifest loader");
    let base = yaml_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let root = base.join(raw_root).canonicalize().with_context(|| {
        format!("workspace.root {raw_root:?} resolves to a path that does not exist")
    })?;
    let sandbox_root = match wcfg.sandbox_root.as_ref() {
        Some(raw) => Some(base.join(raw).canonicalize().with_context(|| {
            format!("workspace.sandbox_root {raw:?} resolves to a path that does not exist")
        })?),
        None => None,
    };
    Ok(Mode::LocalWorkspace {
        root,
        watch: wcfg.watch,
        sandbox_root,
    })
}

fn default_manifest_path(mode: &Mode) -> Option<PathBuf> {
    match mode {
        Mode::SourceRoot { .. } => None,
        Mode::Workspace { dir } => find_workspace_manifest(dir),
        Mode::Watch { dir } => find_workspace_manifest(dir),
        Mode::LocalWorkspace { root, .. } => find_workspace_manifest(root),
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
        Mode::LocalWorkspace { .. } => "MCP Server (local-workspace)",
        Mode::Watch { .. } => "MCP Server (watch)",
        Mode::Bare => "MCP Server",
    }
}

fn print_boot_summary(
    mode: &Mode,
    manifest: Option<&Manifest>,
    source_roots: &[String],
    python_tool_count: usize,
    env_file_loaded: Option<&Path>,
) {
    let mode_label = match mode {
        Mode::SourceRoot { dir } => format!("source-root [{}]", dir.display()),
        Mode::Workspace { dir } => format!("workspace [{}]", dir.display()),
        Mode::LocalWorkspace {
            root,
            watch,
            sandbox_root,
        } => format!(
            "local-workspace [{}{}{}]",
            root.display(),
            if *watch { " +watch" } else { "" },
            match sandbox_root {
                Some(b) => format!(" sandbox={}", b.display()),
                None => String::new(),
            }
        ),
        Mode::Watch { dir } => format!("watch [{}]", dir.display()),
        Mode::Bare => "bare framework".to_string(),
    };
    let mut parts = vec![format!("mode: {mode_label}")];
    if let Some(p) = env_file_loaded {
        parts.push(format!("env: {}", p.display()));
    }
    if let Some(m) = manifest {
        parts.push(format!("manifest: {}", m.yaml_path.display()));
        if !m.tools.is_empty() {
            parts.push(format!("{} manifest tool(s)", m.tools.len()));
        }
        if m.embedder.is_some() {
            parts.push("embedder loaded".to_string());
        }
    }
    if python_tool_count > 0 {
        parts.push(format!("{python_tool_count} python tool(s) registered"));
    }
    if !source_roots.is_empty() {
        parts.push(format!("source roots: {source_roots:?}"));
    }
    eprintln!("mcp-server: {}", parts.join("; "));
}

/// Dispatch a skills subcommand and return — server boot is skipped
/// when a subcommand was supplied. Each branch prints to stdout (lint
/// also flips the process exit code on hard errors).
fn run_skills_command(cmd: &Command) -> Result<()> {
    match cmd {
        Command::SkillsLint { path } => {
            let report = skills_cli::skills_lint(path)
                .with_context(|| format!("skills-lint on {path:?}"))?;
            print!("{}", report.format());
            if report.has_errors {
                std::process::exit(1);
            }
        }
        Command::SkillsList {
            mcp_config,
            no_bundled,
        } => {
            let output = skills_cli::skills_list(mcp_config, !no_bundled)
                .map_err(|e| anyhow::anyhow!(e))
                .with_context(|| format!("skills-list on {mcp_config:?}"))?;
            print!("{output}");
        }
        Command::SkillsShow {
            mcp_config,
            name,
            no_bundled,
        } => {
            let output = skills_cli::skills_show(mcp_config, name, !no_bundled)
                .map_err(|e| anyhow::anyhow!(e))
                .with_context(|| format!("skills-show '{name}' on {mcp_config:?}"))?;
            print!("{output}");
        }
        Command::SkillsNew {
            dest,
            name,
            description,
        } => {
            let written = skills_cli::skills_new(dest, name, description)
                .map_err(|e| anyhow::anyhow!(e))
                .with_context(|| format!("skills-new '{name}' at {dest:?}"))?;
            println!("Wrote starter skill to {}", written.display());
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();

    // Skills-related subcommands short-circuit the server boot — they
    // do their work, print, and exit. The skills helpers live in
    // `mcp-methods::server::cli` so downstream binaries can wire the
    // same surface into their own CLIs.
    if let Some(cmd) = &cli.command {
        return run_skills_command(cmd);
    }

    let mut mode = pick_mode(&cli);

    if let Mode::SourceRoot { dir } = &mode {
        if !dir.is_dir() {
            anyhow::bail!(
                "--source-root path does not exist or is not a directory: {}",
                dir.display()
            );
        }
    }
    if let Mode::Watch { dir } = &mode {
        if !dir.is_dir() {
            anyhow::bail!(
                "--watch path does not exist or is not a directory: {}",
                dir.display()
            );
        }
    }

    let manifest = load_manifest(&cli, &mode).context("manifest load failed")?;

    // Manifest `workspace.kind: local` wins over CLI flags (same rule
    // as manifest `source_root:` overriding bare mode). Convert the
    // mode here before any binding logic runs.
    if let Some(m) = manifest.as_ref() {
        if let Some(wcfg) = m.workspace.as_ref() {
            if wcfg.kind == mcp_methods::server::WorkspaceKind::Local {
                mode = local_workspace_mode(wcfg, &m.yaml_path)?;
            }
        }
    }

    // Load .env before anything reads env vars (e.g. github tools' GITHUB_TOKEN).
    let env_start_dir: PathBuf = match &mode {
        Mode::SourceRoot { dir } | Mode::Workspace { dir } | Mode::Watch { dir } => dir.clone(),
        Mode::LocalWorkspace { root, .. } => root.clone(),
        Mode::Bare => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let env_file_loaded = load_env_for_mode(manifest.as_ref(), &env_start_dir)?;

    let mut options = ServerOptions::from_manifest(manifest.as_ref(), fallback_name(&mode));
    if cli.name.is_some() {
        options.name = cli.name.clone();
    }

    // Wire source roots / workspace: --source-root and --watch each pin
    // a single dir; --workspace gets a dynamic provider driven by the
    // active repo; manifest declaration applies in bare mode.
    let mut source_roots: Vec<String> = Vec::new();
    match &mode {
        Mode::SourceRoot { dir } | Mode::Watch { dir } => {
            let canon = dir
                .canonicalize()
                .with_context(|| format!("failed to canonicalize directory {}", dir.display()))?;
            source_roots.push(canon.to_string_lossy().into_owned());
        }
        Mode::Workspace { dir } => {
            let canon = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            let ws = workspace::Workspace::open(canon, cli.stale_after_days, None)
                .context("workspace initialisation failed")?;
            options = options.with_workspace(ws);
        }
        Mode::LocalWorkspace {
            root, sandbox_root, ..
        } => {
            let mut ws = workspace::Workspace::open_local(root.clone(), None)
                .context("local-workspace initialisation failed")?;
            if let Some(boundary) = sandbox_root {
                ws = ws
                    .with_sandbox_root(boundary)
                    .context("workspace.sandbox_root rejected")?;
            }
            options = options.with_workspace(ws);
        }
        Mode::Bare => {
            if let Some(m) = manifest.as_ref() {
                if !m.source_roots.is_empty() {
                    source_roots =
                        resolve_source_roots(m).context("source root resolution failed")?;
                }
            }
        }
    }
    if !source_roots.is_empty() {
        options = options.with_static_source_roots(source_roots.clone());
    }
    let server = McpServer::new(options);
    // Python tool / embedder extension hooks were removed in 0.3.26 — they
    // required PyO3 in the framework's source and violated the pure-Rust
    // contract of `mcp-methods`. Manifest entries declaring `python:`
    // tools or `embedder:` still parse, but the framework binary no
    // longer instantiates them. Downstream binaries (kglite, etc.) that
    // need a Python extension layer add a pyo3 wrapper in their own
    // cdylib + binary. Warn if the manifest uses either so operators
    // aren't silently ignored.
    let python_tool_count: usize = {
        if let Some(m) = manifest.as_ref() {
            let py_count = m
                .tools
                .iter()
                .filter(|t| matches!(t, mcp_methods::server::ToolSpec::Python(_)))
                .count();
            if py_count > 0 || m.embedder.is_some() {
                tracing::warn!(
                    "manifest declares Python extensions ({py_count} python tool(s), \
                     embedder={}); the mcp-server binary in 0.3.26+ no longer \
                     instantiates them. Use a downstream binary with its own pyo3 \
                     wrapper layer if you need this surface.",
                    m.embedder.is_some()
                );
            }
        }
        let _ = cli.trust_tools;
        0
    };

    let _watch_handle = match &mode {
        Mode::Watch { dir } => maybe_watch(Some(dir), None)?,
        Mode::LocalWorkspace {
            root, watch: true, ..
        } => maybe_watch(Some(root), None)?,
        _ => None,
    };

    print_boot_summary(
        &mode,
        manifest.as_ref(),
        &source_roots,
        python_tool_count,
        env_file_loaded.as_deref(),
    );

    let service = server
        .serve(stdio())
        .await
        .context("failed to start MCP service over stdio")?;
    service.waiting().await?;
    Ok(())
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

    /// `<tmp>/manifest_mcp.yaml` beside `<tmp>/repos/active`. Everything is
    /// canonicalized — on macOS a tempdir sits under the `/var` →
    /// `/private/var` symlink, so an un-canonicalized expectation would
    /// compare two different spellings of the same directory.
    fn manifest_layout() -> (tempfile::TempDir, PathBuf) {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        std::fs::create_dir_all(base.join("repos").join("active")).unwrap();
        let yaml = base.join("workspace_mcp.yaml");
        std::fs::write(&yaml, "workspace:\n  kind: local\n  root: ./repos/active\n").unwrap();
        (td, yaml)
    }

    #[test]
    fn local_workspace_mode_resolves_sandbox_root_against_the_manifest_dir() {
        let (_td, yaml) = manifest_layout();
        let base = yaml.parent().unwrap().to_path_buf();
        let cfg = mcp_methods::server::WorkspaceConfig {
            kind: mcp_methods::server::WorkspaceKind::Local,
            root: Some("./repos/active".to_string()),
            sandbox_root: Some("./repos".to_string()),
            ..Default::default()
        };
        match local_workspace_mode(&cfg, &yaml).unwrap() {
            Mode::LocalWorkspace {
                root, sandbox_root, ..
            } => {
                assert_eq!(root, base.join("repos").join("active"));
                assert_eq!(
                    sandbox_root,
                    Some(base.join("repos")),
                    "sandbox_root must resolve against the manifest dir, like root"
                );
            }
            other => panic!("expected LocalWorkspace, got {other:?}"),
        }
    }

    #[test]
    fn local_workspace_mode_without_sandbox_root_is_unbounded() {
        let (_td, yaml) = manifest_layout();
        let cfg = mcp_methods::server::WorkspaceConfig {
            kind: mcp_methods::server::WorkspaceKind::Local,
            root: Some("./repos/active".to_string()),
            ..Default::default()
        };
        match local_workspace_mode(&cfg, &yaml).unwrap() {
            Mode::LocalWorkspace { sandbox_root, .. } => assert!(
                sandbox_root.is_none(),
                "no sandbox_root key must stay unbounded"
            ),
            other => panic!("expected LocalWorkspace, got {other:?}"),
        }
    }

    #[test]
    fn local_workspace_mode_rejects_a_sandbox_root_that_does_not_exist() {
        let (_td, yaml) = manifest_layout();
        let cfg = mcp_methods::server::WorkspaceConfig {
            kind: mcp_methods::server::WorkspaceKind::Local,
            root: Some("./repos/active".to_string()),
            sandbox_root: Some("./nope".to_string()),
            ..Default::default()
        };
        let err = local_workspace_mode(&cfg, &yaml)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains("sandbox_root"), "unexpected error: {err}");
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
