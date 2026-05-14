//! YAML manifest schema + loader.
//!
//! A manifest is a YAML file declaring the tools, source roots, custom
//! embedder, and trust gates the server should apply. The loader parses,
//! validates, and returns a [`Manifest`]; consumers (CLI wiring, tool
//! registration) operate on the validated structure.
//!
//! Path strings (`source_root`, `python:` tool paths, embedder module)
//! are kept as the raw user input — relative-to-yaml resolution happens
//! at the use site so the data stays pure and testable.
//!
//! Validation is fail-fast and user-facing: the caller surfaces
//! [`ManifestError`] messages directly to the operator.
//!
//! Schema mirrors the Python `kglite.mcp_server.manifest` module 1:1 so
//! a manifest written for the Python server boots unchanged on the new
//! Rust server.

// A handful of fields/helpers are exposed for downstream consumers
// (e.g. kglite-mcp-server reads `CypherTool::cypher` directly when
// registering manifest-declared tools) and so look unused from this
// crate's perspective. Silence dead-code warnings rather than chase
// every cross-crate use.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

const ALLOWED_TOP_KEYS: &[&str] = &[
    "name",
    "instructions",
    "overview_prefix",
    "source_root",
    "source_roots",
    "trust",
    "tools",
    "embedder",
    "builtins",
    "env_file",
    "workspace",
    "extensions",
    "skills",
];
const ALLOWED_WORKSPACE_KEYS: &[&str] = &["kind", "root", "watch", "applies_to"];
const VALID_WORKSPACE_KIND: &[&str] = &["github", "local"];
const ALLOWED_TRUST_KEYS: &[&str] = &[
    "allow_python_tools",
    "allow_embedder",
    "allow_query_preprocessor",
];
const ALLOWED_TOOL_KEYS: &[&str] = &[
    "name",
    "description",
    "parameters",
    "cypher",
    "python",
    "function",
    "bundled",
    "hidden",
    // 0.3.34: per-deployment rename for bundled tools (the bundled
    // override block already covers `description` and `hidden`; this
    // adds the third axis — what the agent sees in `tools/list`).
    "rename",
];
const ALLOWED_EMBEDDER_KEYS: &[&str] = &["module", "class", "kwargs"];
const ALLOWED_BUILTIN_KEYS: &[&str] = &["save_graph", "temp_cleanup"];
const VALID_TEMP_CLEANUP: &[&str] = &["never", "on_overview"];

#[derive(Debug, Error)]
#[error("{path}: {message}")]
pub struct ManifestError {
    pub path: String,
    pub message: String,
}

impl ManifestError {
    pub fn at(path: &Path, message: impl Into<String>) -> Self {
        Self {
            path: path.display().to_string(),
            message: message.into(),
        }
    }

    pub fn bare(message: impl Into<String>) -> Self {
        Self {
            path: "<manifest>".to_string(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct TrustConfig {
    pub allow_python_tools: bool,
    pub allow_embedder: bool,
    /// Advisory gate: the manifest declares that an extension-defined
    /// query preprocessor hook is permitted to run. The framework does
    /// not parse or execute the preprocessor itself — it lives in the
    /// opaque `extensions:` passthrough — but downstream consumers
    /// (e.g. kglite-mcp-server) read this flag and refuse to boot the
    /// hook when it is false. Same pattern as `allow_embedder`.
    pub allow_query_preprocessor: bool,
}

#[derive(Debug, Clone)]
pub enum ToolSpec {
    Cypher(CypherTool),
    Python(PythonTool),
    /// Override the agent-facing surface of a bundled tool (one the
    /// downstream binary provides natively — `cypher_query`,
    /// `graph_overview`, `read_source`, etc.). The framework parses
    /// the override but does not enforce that the named tool exists;
    /// the downstream consumer (e.g. `kglite-mcp-server`) is
    /// responsible for validating the name against its bundled
    /// catalogue at boot time and applying the override when
    /// emitting `tools/list`.
    ///
    /// Pre-0.3.31 the only customisation path for the bundled tool
    /// surface was the manifest's global `instructions:` block —
    /// useful for first-message orientation but not attached to
    /// individual tools. Bundled overrides let operators rewrite a
    /// specific tool's `description` (what the agent sees in
    /// `tools/list`) or `hidden`-flag it out entirely.
    Bundled(BundledOverride),
}

impl ToolSpec {
    pub fn name(&self) -> &str {
        match self {
            ToolSpec::Cypher(t) => &t.name,
            ToolSpec::Python(t) => &t.name,
            ToolSpec::Bundled(t) => &t.name,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CypherTool {
    pub name: String,
    pub cypher: String,
    pub description: Option<String>,
    pub parameters: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct PythonTool {
    pub name: String,
    pub python: String,
    pub function: String,
    pub description: Option<String>,
    pub parameters: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct BundledOverride {
    /// Name of the bundled tool to override (e.g. `cypher_query`,
    /// `repo_management`). Validation against the downstream
    /// binary's actual catalogue happens at the consumer's boot
    /// time — the framework only checks shape here.
    pub name: String,
    /// New agent-facing description that replaces the bundled
    /// tool's default. `None` means "do not override; keep the
    /// default."
    pub description: Option<String>,
    /// When true, the downstream consumer should omit this tool
    /// from `tools/list` AND reject calls to it. Defaults to
    /// false (visible).
    pub hidden: bool,
    /// Per-deployment rename: expose the bundled tool to the agent
    /// under this name instead of its canonical name. `None` keeps
    /// the canonical name. Lets operators running multiple kglite
    /// servers (each backed by a different graph) disambiguate
    /// otherwise-identical tool surfaces — without rename, an agent
    /// running three servers sees three copies of `cypher_query`,
    /// each indistinguishable in ToolSearch results. With rename,
    /// the same servers can expose `legal_cypher_query`,
    /// `prospect_cypher_query`, `open_source_cypher_query`.
    /// Must be a valid identifier (`^[a-zA-Z_][a-zA-Z0-9_]*$`);
    /// validation against duplicates across the manifest's tools is
    /// the downstream consumer's responsibility.
    pub rename: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EmbedderConfig {
    pub module: String,
    pub class: String,
    pub kwargs: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Default, Clone)]
pub struct BuiltinsConfig {
    pub save_graph: bool,
    pub temp_cleanup: TempCleanup,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TempCleanup {
    #[default]
    Never,
    OnOverview,
}

impl TempCleanup {
    pub fn as_str(&self) -> &'static str {
        match self {
            TempCleanup::Never => "never",
            TempCleanup::OnOverview => "on_overview",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkspaceKind {
    /// Clone-and-track GitHub repos. The default when no `workspace:`
    /// block is set and the operator passed `--workspace DIR`.
    #[default]
    Github,
    /// Bind a fixed local directory as the active source root. No
    /// cloning happens; `set_root_dir(path)` swaps the active root.
    Local,
}

impl WorkspaceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkspaceKind::Github => "github",
            WorkspaceKind::Local => "local",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceConfig {
    pub kind: WorkspaceKind,
    /// Local-mode only: path to the directory to bind as the source
    /// root. Relative paths resolve against the YAML's parent dir.
    pub root: Option<String>,
    /// Local-mode only: wire the framework's file watcher to `root`
    /// (debounced rebuild trigger via the post-activate hook).
    pub watch: bool,
    /// Optional opt-in for the [`find_workspace_manifest`] parent-walk
    /// fallback. When set, this manifest is auto-discovered by
    /// ``mcp-server --workspace DIR`` (and similar callers) only when
    /// the operator's ``DIR`` matches the declaration here. When
    /// unset, the parent-walk fallback NEVER fires for this manifest
    /// — operators must pass ``--mcp-config`` explicitly.
    ///
    /// Values are glob patterns matching the workspace dir's basename
    /// (single-segment match — parent-walk is always single-level).
    /// Three forms:
    ///
    /// - **Single pattern** (`./repos`, `repos`, `*`, `a*`, `prod-?`):
    ///   match against the workspace dir's basename. Literal strings
    ///   like `repos` match only `repos`; glob patterns like `*` or
    ///   `prod-*` match any name fitting the pattern.
    /// - **List of patterns** (`[./repos, ./clones]`, `[prod-*, test-*]`):
    ///   match if any pattern matches. Useful for curated subsets or
    ///   multiple naming conventions in one manifest.
    ///
    /// Leading `./` is optional and stripped at parse time. Patterns
    /// must be single-segment — `./a/b` is rejected. Invalid glob
    /// syntax is rejected at parse time.
    ///
    /// Eliminates the accidental-discovery footgun where a workspace
    /// manifest is auto-picked-up by an unrelated sibling dir. The
    /// manifest's own declaration is the opt-in.
    pub applies_to: Option<AppliesTo>,
}

/// Declaration of which workspace dirs the manifest applies to for
/// the [`find_workspace_manifest`] parent-walk fallback. See
/// [`WorkspaceConfig::applies_to`] for the full semantics. Each
/// entry is a glob pattern (literal or with `*` / `?` / `[abc]`)
/// matched against the workspace dir's basename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppliesTo {
    /// Single glob pattern. Matches if the workspace dir's basename
    /// satisfies the pattern. Literal names (`repos`) match only
    /// that name; `*` matches anything; `prod-*` matches anything
    /// starting with `prod-`.
    Pattern(String),
    /// Multiple patterns. Matches if any pattern in the list matches.
    Patterns(Vec<String>),
}

/// One source of skills declared by the manifest. Either the magic
/// "library bundled" token (rendered as the YAML boolean `true`), or
/// a filesystem path resolved against the manifest's parent dir.
///
/// Path conventions match the rest of the manifest:
/// - `./foo` or `foo` — relative to the manifest's parent dir
/// - `~/foo` — home-relative (POSIX `$HOME` expansion)
/// - `/foo` — absolute
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSource {
    /// The compile-time bundled skills shipped with `mcp-methods` plus
    /// any added by the downstream binary at registry-build time.
    /// In YAML: a bare `true` token in the `skills:` list.
    Bundled,
    /// A filesystem path containing `*.md` skill files. Walked at
    /// boot. Path resolution happens at registry-build time, not parse
    /// time — `SkillSource::Path` stores the raw operator-declared
    /// string for round-tripping through `Manifest::to_json()`.
    Path(String),
}

/// The parsed value of the `skills:` field in the manifest.
///
/// Skills are opt-in. `SkillsSource::Disabled` is the default and
/// matches verbatim-current MCP behavior: no `prompts/list`, no
/// methodology surface, identical context cost to pre-skills
/// deployments. Existing kglite manifests work unchanged.
///
/// When enabled, the [`crate::server::skills::Registry`] walks each
/// source in declaration order, layering them against the
/// project-local `<basename>.skills/` directory which is always
/// auto-detected as the top-priority layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SkillsSource {
    /// `skills: false` or no declaration. Skills disabled entirely.
    #[default]
    Disabled,
    /// One or more sources, walked in declaration order at registry
    /// build time. First-match-per-skill-name wins across the root
    /// layer; the auto-detected project layer (`<basename>.skills/`
    /// adjacent to the YAML) preempts the entire root layer.
    Sources(Vec<SkillSource>),
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub yaml_path: PathBuf,
    pub name: Option<String>,
    pub instructions: Option<String>,
    pub overview_prefix: Option<String>,
    pub source_roots: Vec<String>,
    pub trust: TrustConfig,
    pub tools: Vec<ToolSpec>,
    pub embedder: Option<EmbedderConfig>,
    pub builtins: BuiltinsConfig,
    /// Optional explicit `.env` path (relative to the YAML or absolute).
    /// When unset, the runtime walks upward from the start directory
    /// looking for a `.env` file.
    pub env_file: Option<String>,
    /// Optional explicit workspace declaration. When set, this wins
    /// over CLI `--workspace`/`--source-root` flags interpretation
    /// (manifest is the source of truth — same rule as `source_root:`).
    pub workspace: Option<WorkspaceConfig>,
    /// Raw passthrough for downstream-binary-specific manifest keys.
    /// The framework accepts any mapping under `extensions:` and stores
    /// it here without validating the inner keys; downstream consumers
    /// (e.g. kglite-mcp-server) read whatever they need from this map.
    ///
    /// This keeps the framework's strict-unknown-key validation strong
    /// for the surfaces it owns (`builtins`, `workspace`, …) while
    /// letting consumers add their own configuration namespace without
    /// per-key framework round-trips.
    pub extensions: serde_json::Map<String, serde_json::Value>,
    /// Opt-in skills declaration. `SkillsSource::Disabled` is the
    /// default and preserves current MCP behavior (no `prompts/`
    /// surface). When set to any non-`Disabled` value, downstream
    /// binaries pass this to [`crate::server::skills::Registry`] for
    /// loading + composition; the framework then exposes the
    /// resulting skill set via `prompts/list` and `prompts/get`.
    ///
    /// Three-layer composition: the operator-declared sources here
    /// form the root layer; the project-local `<basename>.skills/`
    /// directory (auto-detected) preempts them. See
    /// `dev-documentation/skills-aware-mcp.md` for the full design.
    pub skills: SkillsSource,
}

impl Manifest {
    /// JSON-friendly representation of the validated manifest for
    /// FFI / RPC exposure (pyo3 wrappers, JSON-RPC bridges, etc.).
    ///
    /// The shape is stable across patch releases: fields can be added
    /// non-breaking, but key renames or removals are breaking changes.
    /// When adding a new field to `Manifest`, extend this method too —
    /// the `to_json_shape_is_stable` test will fail until you do.
    /// The `extensions` map is passed through unchanged; downstream
    /// consumers parse their own namespace from it.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "yaml_path": self.yaml_path.display().to_string(),
            "name": self.name,
            "instructions": self.instructions,
            "overview_prefix": self.overview_prefix,
            "source_roots": self.source_roots,
            "trust": {
                "allow_python_tools": self.trust.allow_python_tools,
                "allow_embedder": self.trust.allow_embedder,
                "allow_query_preprocessor": self.trust.allow_query_preprocessor,
            },
            "tools": self.tools.iter().map(|t| match t {
                ToolSpec::Cypher(c) => serde_json::json!({
                    "kind": "cypher",
                    "name": c.name,
                    "cypher": c.cypher,
                    "description": c.description,
                    "parameters": c.parameters,
                }),
                ToolSpec::Python(p) => serde_json::json!({
                    "kind": "python",
                    "name": p.name,
                    "python": p.python,
                    "function": p.function,
                    "description": p.description,
                    "parameters": p.parameters,
                }),
                ToolSpec::Bundled(b) => serde_json::json!({
                    "kind": "bundled",
                    "name": b.name,
                    "description": b.description,
                    "hidden": b.hidden,
                    "rename": b.rename,
                }),
            }).collect::<Vec<_>>(),
            "embedder": self.embedder.as_ref().map(|e| serde_json::json!({
                "module": e.module,
                "class": e.class,
                "kwargs": e.kwargs,
            })),
            "builtins": {
                "save_graph": self.builtins.save_graph,
                "temp_cleanup": self.builtins.temp_cleanup.as_str(),
            },
            "env_file": self.env_file,
            "workspace": self.workspace.as_ref().map(|w| serde_json::json!({
                "kind": w.kind.as_str(),
                "root": w.root,
                "watch": w.watch,
                "applies_to": w.applies_to.as_ref().map(|a| match a {
                    AppliesTo::Pattern(p) => serde_json::Value::String(p.clone()),
                    AppliesTo::Patterns(ps) => serde_json::Value::Array(
                        ps.iter().map(|p| serde_json::Value::String(p.clone())).collect()
                    ),
                }),
            })),
            "extensions": self.extensions,
            "skills": self.skills_to_json(),
        })
    }

    /// JSON shape for the parsed `skills:` field. Emits the operator-
    /// declared shape unchanged (modulo normalisation), suitable for
    /// downstream pyo3 wrappers that need to introspect what the
    /// manifest declared without re-running the parser.
    ///
    /// Phase 1a (this file) emits the raw declaration only. Phase 1b
    /// adds a separate accessor on the resolved registry that exposes
    /// the *post-resolution* skill list with provenance — that's the
    /// per-skill `{path, origin, frontmatter}` shape kglite asked for
    /// in their feedback. The two surfaces are intentionally
    /// distinct: this method describes the manifest, the
    /// registry method describes the runtime resolution.
    fn skills_to_json(&self) -> serde_json::Value {
        match &self.skills {
            SkillsSource::Disabled => serde_json::Value::Bool(false),
            SkillsSource::Sources(sources) => {
                let arr: Vec<serde_json::Value> = sources
                    .iter()
                    .map(|s| match s {
                        SkillSource::Bundled => serde_json::Value::Bool(true),
                        SkillSource::Path(p) => serde_json::Value::String(p.clone()),
                    })
                    .collect();
                serde_json::Value::Array(arr)
            }
        }
    }
}

/// Auto-detect ``<basename>_mcp.yaml`` next to a graph file.
pub fn find_sibling_manifest(graph_path: &Path) -> Option<PathBuf> {
    let stem = graph_path.file_stem()?;
    let parent = graph_path.parent()?;
    let candidate = parent.join(format!("{}_mcp.yaml", stem.to_string_lossy()));
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// Auto-detect ``workspace_mcp.yaml`` for a workspace directory.
///
/// Checks two locations in strict priority order:
///
/// 1. **Primary** — ``<workspace_dir>/workspace_mcp.yaml``. The
///    documented and recommended location. If this exists, it is
///    returned unconditionally; the parent-walk fallback is NOT
///    consulted even if a parent manifest also exists. No opt-in
///    declaration required — the manifest sitting inside the
///    workspace dir is itself the operator's intent.
/// 2. **Parent-walk fallback** —
///    ``<workspace_dir>/../workspace_mcp.yaml``. Triggered only when
///    the primary is absent AND the parent manifest *declares* it
///    applies to this specific workspace dir via the
///    ``workspace.applies_to:`` field:
///
///    ```yaml
///    # open_source/workspace_mcp.yaml
///    workspace:
///      kind: github
///      applies_to: ./repos     # required for parent-walk discovery
///    ```
///
///    The framework loads the parent manifest, canonicalises
///    ``manifest.workspace.applies_to`` against the manifest's parent
///    directory, and compares it to the actual ``workspace_dir``.
///    Match → manifest is returned. No declaration or path mismatch
///    → discovery returns ``None`` (operator must pass
///    ``--mcp-config`` explicitly).
///
///    The natural layout for github-clone-tracker workspaces is:
///
///    ```text
///    open_source/
///    ├── workspace_mcp.yaml     # config sits beside the sandbox; declares
///    │                          # workspace.applies_to: ./repos
///    └── repos/                 # --workspace points here
///    ```
///
///    The ``applies_to`` opt-in eliminates the accidental-discovery
///    footgun where a manifest in a project root would auto-attach to
///    any unrelated sibling dir. Operators who didn't author the
///    manifest get the safe default (no auto-detection); operators
///    who did get the ergonomic UX (no ``--mcp-config`` boilerplate).
///
/// Bounded to one level up; will not walk past the filesystem root.
/// Symlink-safe via canonicalisation. Added per kglite operator
/// feedback after the 0.6.x → 0.9.x migration audit.
pub fn find_workspace_manifest(workspace_dir: &Path) -> Option<PathBuf> {
    let primary = workspace_dir.join("workspace_mcp.yaml");
    if primary.is_file() {
        return Some(primary);
    }
    // Parent-walk fallback. Compare against canonicalised paths to
    // handle "/" (where parent == self) and symlinks consistently.
    let parent = workspace_dir.parent()?;
    let workspace_resolved = workspace_dir.canonicalize().ok()?;
    let parent_resolved = parent.canonicalize().ok()?;
    if parent_resolved == workspace_resolved {
        // No real parent (filesystem root).
        return None;
    }
    let fallback = parent.join("workspace_mcp.yaml");
    if !fallback.is_file() {
        return None;
    }

    // The fallback manifest must declare workspace.applies_to and
    // that declaration must canonicalise to the actual workspace_dir.
    // Otherwise the discovery is unsafe (could be accidental).
    let manifest = match load(&fallback) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                manifest = %fallback.display(),
                error = %e,
                "parent-walk manifest exists but failed to parse; ignoring"
            );
            return None;
        }
    };
    let declared = manifest
        .workspace
        .as_ref()
        .and_then(|w| w.applies_to.as_ref());
    let Some(declared_applies_to) = declared else {
        tracing::info!(
            manifest = %fallback.display(),
            "parent-walk manifest does not declare workspace.applies_to; \
             ignoring (set workspace.applies_to: <pattern> to opt in)"
        );
        return None;
    };
    // Match the workspace dir's basename against the declared pattern(s).
    // The parent-walk guarantee (workspace_dir.parent() == manifest_dir)
    // is already established above — only the basename match is left.
    let Some(basename) = workspace_resolved.file_name().and_then(|n| n.to_str()) else {
        return None; // path with no usable basename, defensive
    };
    let patterns: Vec<&str> = match declared_applies_to {
        AppliesTo::Pattern(p) => vec![p.as_str()],
        AppliesTo::Patterns(ps) => ps.iter().map(String::as_str).collect(),
    };
    let matched = patterns.iter().any(|pat| {
        match globset::Glob::new(pat) {
            Ok(g) => g.compile_matcher().is_match(basename),
            Err(_) => {
                // Should not happen — patterns were validated at parse
                // time. Defensive: treat as non-match.
                false
            }
        }
    });
    if matched {
        tracing::info!(
            workspace_dir = %workspace_dir.display(),
            manifest = %fallback.display(),
            "manifest discovered via parent-walk fallback (workspace.applies_to matched)"
        );
        Some(fallback)
    } else {
        tracing::info!(
            workspace_dir = %workspace_resolved.display(),
            manifest = %fallback.display(),
            basename = %basename,
            patterns = ?patterns,
            "parent-walk manifest's workspace.applies_to does not match \
             this workspace_dir's basename; ignoring"
        );
        None
    }
}

/// Parse and validate a manifest YAML file.
pub fn load(yaml_path: &Path) -> Result<Manifest, ManifestError> {
    let text = fs::read_to_string(yaml_path)
        .map_err(|e| ManifestError::at(yaml_path, format!("read error: {e}")))?;
    let raw: serde_yaml::Value = serde_yaml::from_str(&text)
        .map_err(|e| ManifestError::at(yaml_path, format!("YAML parse error: {e}")))?;
    let raw = match raw {
        serde_yaml::Value::Null => serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        v => v,
    };
    let map = raw
        .as_mapping()
        .ok_or_else(|| ManifestError::at(yaml_path, "top-level must be a mapping"))?;
    build(map, yaml_path)
}

fn build(raw: &serde_yaml::Mapping, yaml_path: &Path) -> Result<Manifest, ManifestError> {
    check_keys(raw, ALLOWED_TOP_KEYS, "top-level keys", yaml_path)?;

    if raw.contains_key("source_root") && raw.contains_key("source_roots") {
        return Err(ManifestError::at(
            yaml_path,
            "specify either source_root (str) or source_roots (list), not both",
        ));
    }

    let mut source_roots: Vec<String> = Vec::new();
    if let Some(v) = raw.get("source_root") {
        let s = v.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
            ManifestError::at(yaml_path, "source_root must be a non-empty string")
        })?;
        source_roots.push(s.to_string());
    } else if let Some(v) = raw.get("source_roots") {
        let seq = v.as_sequence().ok_or_else(|| {
            ManifestError::at(
                yaml_path,
                "source_roots must be a list of non-empty strings",
            )
        })?;
        if seq.is_empty() {
            return Err(ManifestError::at(
                yaml_path,
                "source_roots must be non-empty when set",
            ));
        }
        for item in seq {
            let s = item.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
                ManifestError::at(
                    yaml_path,
                    "source_roots must be a list of non-empty strings",
                )
            })?;
            source_roots.push(s.to_string());
        }
    }

    let trust = build_trust(raw.get("trust"), yaml_path)?;
    let tools = build_tools(raw.get("tools"), yaml_path)?;
    let embedder = build_embedder(raw.get("embedder"), yaml_path)?;
    let builtins = build_builtins(raw.get("builtins"), yaml_path)?;
    let workspace = build_workspace(raw.get("workspace"), yaml_path)?;
    let extensions = build_extensions(raw.get("extensions"), yaml_path)?;
    let skills = build_skills(raw.get("skills"), yaml_path)?;

    Ok(Manifest {
        yaml_path: yaml_path.to_path_buf(),
        name: optional_str(raw, "name", yaml_path)?,
        instructions: optional_str(raw, "instructions", yaml_path)?,
        overview_prefix: optional_str(raw, "overview_prefix", yaml_path)?,
        source_roots,
        trust,
        tools,
        embedder,
        builtins,
        env_file: optional_str(raw, "env_file", yaml_path)?,
        workspace,
        extensions,
        skills,
    })
}

/// Parse the polymorphic `skills:` field. Accepts:
///
/// - **Absent or `false`** → [`SkillsSource::Disabled`]. Pure-current
///   MCP behavior. This is the default and what existing deployments
///   resolve to without any YAML change.
/// - **`skills: true`** → single bundled source. Sugar for
///   `skills: [true]`.
/// - **`skills: <path-string>`** → single path source. Sugar for
///   `skills: [<path>]`.
/// - **`skills: [bool, string, ...]`** → ordered list. Booleans MUST
///   be `true` (the bundled marker); `false` is rejected at parse
///   time as nonsense in list context. Each path is stored verbatim
///   as the operator wrote it; resolution against the manifest's
///   parent dir happens at registry-build time, not here.
///
/// Empty lists are accepted and parsed as `SkillsSource::Sources(vec![])`;
/// the registry treats them as "skills opted in but no root layer,"
/// meaning the project-local `<basename>.skills/` auto-detection
/// still fires while the bundled + custom-path layers stay empty.
/// Useful for operators who want to rely solely on adjacent project
/// skills.
fn build_skills(
    raw: Option<&serde_yaml::Value>,
    yaml_path: &Path,
) -> Result<SkillsSource, ManifestError> {
    use serde_yaml::Value;

    match raw {
        None | Some(Value::Null) | Some(Value::Bool(false)) => Ok(SkillsSource::Disabled),
        Some(Value::Bool(true)) => Ok(SkillsSource::Sources(vec![SkillSource::Bundled])),
        Some(Value::String(s)) => {
            if s.is_empty() {
                return Err(ManifestError::at(
                    yaml_path,
                    "skills: path must be a non-empty string",
                ));
            }
            Ok(SkillsSource::Sources(vec![SkillSource::Path(s.clone())]))
        }
        Some(Value::Sequence(seq)) => {
            let mut sources = Vec::with_capacity(seq.len());
            for (idx, item) in seq.iter().enumerate() {
                match item {
                    Value::Bool(true) => sources.push(SkillSource::Bundled),
                    Value::Bool(false) => {
                        return Err(ManifestError::at(
                            yaml_path,
                            format!(
                                "skills[{idx}]: `false` is not a valid entry in a `skills:` \
                                 list (only `true` for bundled, or a path string)"
                            ),
                        ));
                    }
                    Value::String(s) => {
                        if s.is_empty() {
                            return Err(ManifestError::at(
                                yaml_path,
                                format!("skills[{idx}]: path must be a non-empty string"),
                            ));
                        }
                        sources.push(SkillSource::Path(s.clone()));
                    }
                    _ => {
                        return Err(ManifestError::at(
                            yaml_path,
                            format!(
                                "skills[{idx}]: each entry must be `true` (for bundled) or a \
                                 path string"
                            ),
                        ));
                    }
                }
            }
            Ok(SkillsSource::Sources(sources))
        }
        Some(_) => Err(ManifestError::at(
            yaml_path,
            "skills must be `false`, `true`, a path string, or a list of \
             (true | path string) entries",
        )),
    }
}

fn build_extensions(
    raw: Option<&serde_yaml::Value>,
    yaml_path: &Path,
) -> Result<serde_json::Map<String, serde_json::Value>, ManifestError> {
    let Some(raw) = raw else {
        return Ok(serde_json::Map::new());
    };
    if matches!(raw, serde_yaml::Value::Null) {
        return Ok(serde_json::Map::new());
    }
    if !raw.is_mapping() {
        return Err(ManifestError::at(
            yaml_path,
            "extensions must be a mapping (downstream-binary-specific keys)",
        ));
    }
    match yaml_to_json(raw.clone())? {
        serde_json::Value::Object(o) => Ok(o),
        _ => Err(ManifestError::at(yaml_path, "extensions must be a mapping")),
    }
}

fn build_workspace(
    raw: Option<&serde_yaml::Value>,
    yaml_path: &Path,
) -> Result<Option<WorkspaceConfig>, ManifestError> {
    let Some(raw) = raw else { return Ok(None) };
    if matches!(raw, serde_yaml::Value::Null) {
        return Ok(None);
    }
    let map = raw
        .as_mapping()
        .ok_or_else(|| ManifestError::at(yaml_path, "workspace must be a mapping"))?;
    check_keys(map, ALLOWED_WORKSPACE_KEYS, "workspace keys", yaml_path)?;
    let kind = match map.get("kind") {
        None | Some(serde_yaml::Value::Null) => WorkspaceKind::default(),
        Some(serde_yaml::Value::String(s)) => match s.as_str() {
            "github" => WorkspaceKind::Github,
            "local" => WorkspaceKind::Local,
            other => {
                return Err(ManifestError::at(
                    yaml_path,
                    format!(
                        "workspace.kind must be one of {VALID_WORKSPACE_KIND:?}, got {other:?}"
                    ),
                ));
            }
        },
        Some(_) => {
            return Err(ManifestError::at(
                yaml_path,
                format!("workspace.kind must be one of {VALID_WORKSPACE_KIND:?}"),
            ))
        }
    };
    let root = match map.get("root") {
        None | Some(serde_yaml::Value::Null) => None,
        Some(serde_yaml::Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => {
            return Err(ManifestError::at(
                yaml_path,
                "workspace.root must be a non-empty string",
            ))
        }
    };
    let watch = match map.get("watch") {
        None | Some(serde_yaml::Value::Null) => false,
        Some(serde_yaml::Value::Bool(b)) => *b,
        Some(_) => {
            return Err(ManifestError::at(
                yaml_path,
                "workspace.watch must be a bool",
            ))
        }
    };
    let applies_to =
        match map.get("applies_to") {
            None | Some(serde_yaml::Value::Null) => None,
            Some(serde_yaml::Value::String(s)) => {
                Some(AppliesTo::Pattern(parse_applies_to_pattern(s, yaml_path)?))
            }
            Some(serde_yaml::Value::Sequence(seq)) => {
                if seq.is_empty() {
                    return Err(ManifestError::at(
                        yaml_path,
                        "workspace.applies_to: list must contain at least one pattern",
                    ));
                }
                let mut patterns = Vec::with_capacity(seq.len());
                for (i, item) in seq.iter().enumerate() {
                    let s = item.as_str().ok_or_else(|| {
                        ManifestError::at(
                            yaml_path,
                            format!("workspace.applies_to[{i}] must be a string"),
                        )
                    })?;
                    let cleaned = parse_applies_to_pattern(s, yaml_path).map_err(|e| {
                        ManifestError::at(
                            yaml_path,
                            format!("workspace.applies_to[{i}]: {}", e.message),
                        )
                    })?;
                    patterns.push(cleaned);
                }
                Some(AppliesTo::Patterns(patterns))
            }
            _ => return Err(ManifestError::at(
                yaml_path,
                "workspace.applies_to must be a non-empty string (a pattern) or a list of patterns",
            )),
        };
    if kind == WorkspaceKind::Local && root.is_none() {
        return Err(ManifestError::at(
            yaml_path,
            "workspace.kind: local requires workspace.root to be set",
        ));
    }
    if kind == WorkspaceKind::Github && watch {
        return Err(ManifestError::at(
            yaml_path,
            "workspace.watch is only valid with workspace.kind: local",
        ));
    }
    Ok(Some(WorkspaceConfig {
        kind,
        root,
        watch,
        applies_to,
    }))
}

/// Parse + validate a single ``workspace.applies_to`` entry. Accepts
/// any glob pattern matching a single path segment (no embedded
/// slashes, no `..`). The leading ``./`` is optional and stripped.
/// Validates glob syntax via `globset::Glob::new` so invalid patterns
/// surface clear errors at boot.
///
/// Returns the cleaned pattern string (without `./` prefix) on
/// success.
fn parse_applies_to_pattern(raw: &str, yaml_path: &Path) -> Result<String, ManifestError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ManifestError::at(
            yaml_path,
            "workspace.applies_to: pattern must not be empty",
        ));
    }
    // Strip a single leading `./` for ergonomic equivalence between
    // `./repos` and `repos`. Both forms commonly appear in operator
    // muscle memory; normalise so storage + glob matching is uniform.
    let stripped = trimmed.strip_prefix("./").unwrap_or(trimmed);
    if stripped.is_empty() {
        return Err(ManifestError::at(
            yaml_path,
            "workspace.applies_to: pattern must not be empty after stripping `./` prefix",
        ));
    }
    if stripped.contains('/') {
        return Err(ManifestError::at(
            yaml_path,
            format!(
                "workspace.applies_to: pattern {raw:?} must be a single path segment \
                 (no embedded `/`) — parent-walk discovery is bounded to one level"
            ),
        ));
    }
    if stripped == ".." || stripped.starts_with("../") {
        return Err(ManifestError::at(
            yaml_path,
            format!("workspace.applies_to: pattern {raw:?} must not contain `..`"),
        ));
    }
    if Path::new(stripped).is_absolute() {
        return Err(ManifestError::at(
            yaml_path,
            format!("workspace.applies_to: pattern {raw:?} must be relative, not absolute"),
        ));
    }
    // Validate glob syntax. Construct a Glob to surface any syntax
    // errors immediately — we don't keep the compiled form (cheap to
    // re-compile at match time, keeps `WorkspaceConfig` Clone-cheap).
    globset::Glob::new(stripped).map_err(|e| {
        ManifestError::at(
            yaml_path,
            format!("workspace.applies_to: invalid glob pattern {raw:?}: {e}"),
        )
    })?;
    Ok(stripped.to_string())
}

fn check_keys(
    map: &serde_yaml::Mapping,
    allowed: &[&str],
    label: &str,
    yaml_path: &Path,
) -> Result<(), ManifestError> {
    let mut unknown: Vec<String> = Vec::new();
    for (k, _) in map {
        let key = k.as_str().unwrap_or("<non-string-key>");
        if !allowed.contains(&key) {
            unknown.push(key.to_string());
        }
    }
    if !unknown.is_empty() {
        unknown.sort();
        return Err(ManifestError::at(
            yaml_path,
            format!("unknown {label}: {unknown:?}. Allowed: {allowed:?}"),
        ));
    }
    Ok(())
}

fn optional_str(
    raw: &serde_yaml::Mapping,
    key: &str,
    yaml_path: &Path,
) -> Result<Option<String>, ManifestError> {
    match raw.get(key) {
        None | Some(serde_yaml::Value::Null) => Ok(None),
        Some(serde_yaml::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(ManifestError::at(
            yaml_path,
            format!("{key} must be a string"),
        )),
    }
}

fn build_trust(
    raw: Option<&serde_yaml::Value>,
    yaml_path: &Path,
) -> Result<TrustConfig, ManifestError> {
    let Some(raw) = raw else {
        return Ok(TrustConfig::default());
    };
    let map = raw
        .as_mapping()
        .ok_or_else(|| ManifestError::at(yaml_path, "trust must be a mapping"))?;
    check_keys(map, ALLOWED_TRUST_KEYS, "trust keys", yaml_path)?;
    let mut cfg = TrustConfig::default();
    if let Some(v) = map.get("allow_python_tools") {
        cfg.allow_python_tools = v.as_bool().ok_or_else(|| {
            ManifestError::at(yaml_path, "trust.allow_python_tools must be a bool")
        })?;
    }
    if let Some(v) = map.get("allow_embedder") {
        cfg.allow_embedder = v
            .as_bool()
            .ok_or_else(|| ManifestError::at(yaml_path, "trust.allow_embedder must be a bool"))?;
    }
    if let Some(v) = map.get("allow_query_preprocessor") {
        cfg.allow_query_preprocessor = v.as_bool().ok_or_else(|| {
            ManifestError::at(yaml_path, "trust.allow_query_preprocessor must be a bool")
        })?;
    }
    Ok(cfg)
}

fn build_tools(
    raw: Option<&serde_yaml::Value>,
    yaml_path: &Path,
) -> Result<Vec<ToolSpec>, ManifestError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let seq = raw
        .as_sequence()
        .ok_or_else(|| ManifestError::at(yaml_path, "tools must be a list"))?;
    let mut tools: Vec<ToolSpec> = Vec::new();
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();
    for (i, entry) in seq.iter().enumerate() {
        let tool = build_tool(entry, i, yaml_path)?;
        let name = tool.name().to_string();
        if seen.insert(name.clone(), ()).is_some() {
            return Err(ManifestError::at(
                yaml_path,
                format!("duplicate tool name: {name:?}"),
            ));
        }
        tools.push(tool);
    }
    Ok(tools)
}

fn build_tool(
    entry: &serde_yaml::Value,
    idx: usize,
    yaml_path: &Path,
) -> Result<ToolSpec, ManifestError> {
    let map = entry
        .as_mapping()
        .ok_or_else(|| ManifestError::at(yaml_path, format!("tools[{idx}] must be a mapping")))?;
    check_keys(map, ALLOWED_TOOL_KEYS, "tool keys", yaml_path)?;

    // Kind detection. `cypher` and `python` are tool-creation kinds
    // (operator declares a new named tool); `bundled` is a tool-
    // override kind (operator picks a bundled tool name and customises
    // its agent-facing surface). Exactly one must be present.
    let has_cypher = map.contains_key("cypher");
    let has_python = map.contains_key("python");
    let has_bundled = map.contains_key("bundled");
    let kinds_present: Vec<&str> = [
        ("cypher", has_cypher),
        ("python", has_python),
        ("bundled", has_bundled),
    ]
    .into_iter()
    .filter(|(_, p)| *p)
    .map(|(k, _)| k)
    .collect();
    if kinds_present.is_empty() {
        return Err(ManifestError::at(
            yaml_path,
            format!("tools[{idx}] needs exactly one of: [\"cypher\", \"python\", \"bundled\"]"),
        ));
    }
    if kinds_present.len() > 1 {
        return Err(ManifestError::at(
            yaml_path,
            format!("tools[{idx}] has multiple kinds set ({kinds_present:?}); pick exactly one"),
        ));
    }

    // The `bundled` kind takes its name from the `bundled:` value
    // itself (e.g. `bundled: cypher_query`) and forbids the
    // tool-creation fields. Branch early so we don't run the
    // tool-creation `name:` requirement against an override entry.
    if has_bundled {
        return build_bundled_override(map, idx, yaml_path);
    }

    let name = map
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| valid_identifier(s))
        .ok_or_else(|| {
            ManifestError::at(
                yaml_path,
                format!("tools[{idx}] needs a string `name:` matching ^[a-zA-Z_][a-zA-Z0-9_]*$"),
            )
        })?
        .to_string();

    // `hidden:` is only valid on bundled overrides (`hidden:`-flagging
    // a tool you're declaring inline doesn't make sense — just don't
    // declare it). Reject early so the operator gets a clear error.
    if map.contains_key("hidden") {
        return Err(ManifestError::at(
            yaml_path,
            format!(
                "tools[{idx}] ({name:?}) `hidden:` is only valid on `bundled:` override entries"
            ),
        ));
    }

    let description = match map.get("description") {
        None | Some(serde_yaml::Value::Null) => None,
        Some(serde_yaml::Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return Err(ManifestError::at(
                yaml_path,
                format!("tools[{idx}] ({name:?}).description must be a string"),
            ))
        }
    };

    let parameters = match map.get("parameters") {
        None | Some(serde_yaml::Value::Null) => None,
        Some(v) if v.is_mapping() => Some(yaml_to_json(v.clone())?),
        Some(_) => {
            return Err(ManifestError::at(
                yaml_path,
                format!("tools[{idx}] ({name:?}).parameters must be a mapping"),
            ))
        }
    };

    if has_cypher {
        let cypher = map
            .get("cypher")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ManifestError::at(
                    yaml_path,
                    format!("tools[{idx}] ({name:?}).cypher must be a non-empty string"),
                )
            })?
            .to_string();
        return Ok(ToolSpec::Cypher(CypherTool {
            name,
            cypher,
            description,
            parameters,
        }));
    }

    // python tool
    let python = map
        .get("python")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ManifestError::at(
                yaml_path,
                format!("tools[{idx}] ({name:?}).python must be a non-empty path string"),
            )
        })?
        .to_string();
    let function = map
        .get("function")
        .and_then(|v| v.as_str())
        .filter(|s| valid_identifier(s))
        .ok_or_else(|| {
            ManifestError::at(
                yaml_path,
                format!(
                    "tools[{idx}] ({name:?}) python tools need `function:` set to a valid Python identifier"
                ),
            )
        })?
        .to_string();
    Ok(ToolSpec::Python(PythonTool {
        name,
        python,
        function,
        description,
        parameters,
    }))
}

/// Parse a `bundled:` override entry from `tools[idx]`. The caller
/// (`build_tool`) has already established that the entry has
/// `bundled:` set as the kind discriminator.
fn build_bundled_override(
    map: &serde_yaml::Mapping,
    idx: usize,
    yaml_path: &Path,
) -> Result<ToolSpec, ManifestError> {
    let name = map
        .get("bundled")
        .and_then(|v| v.as_str())
        .filter(|s| valid_identifier(s))
        .ok_or_else(|| {
            ManifestError::at(
                yaml_path,
                format!(
                    "tools[{idx}] `bundled:` must be a string naming a bundled tool \
                     (must match ^[a-zA-Z_][a-zA-Z0-9_]*$)"
                ),
            )
        })?
        .to_string();

    // Tool-creation fields are forbidden on override entries — the
    // override only customises an existing bundled tool's surface,
    // it doesn't declare a new tool. Catch these at parse time so
    // operators get a clear error rather than silent confusion.
    for forbidden in ["name", "parameters", "function"] {
        if map.contains_key(forbidden) {
            return Err(ManifestError::at(
                yaml_path,
                format!(
                    "tools[{idx}] bundled override {name:?} cannot set `{forbidden}:` \
                     (only `description:`, `hidden:`, and `rename:` are permitted on overrides)"
                ),
            ));
        }
    }

    let description = match map.get("description") {
        None | Some(serde_yaml::Value::Null) => None,
        Some(serde_yaml::Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return Err(ManifestError::at(
                yaml_path,
                format!("tools[{idx}] bundled override {name:?}.description must be a string"),
            ))
        }
    };

    let hidden = match map.get("hidden") {
        None | Some(serde_yaml::Value::Null) => false,
        Some(serde_yaml::Value::Bool(b)) => *b,
        Some(_) => {
            return Err(ManifestError::at(
                yaml_path,
                format!("tools[{idx}] bundled override {name:?}.hidden must be a bool"),
            ))
        }
    };

    // 0.3.34: optional per-deployment rename. Validated as an
    // identifier here; cross-tool collision check is the consumer's
    // job (it knows what other names — bundled, cypher, python — it
    // has in scope).
    let rename = match map.get("rename") {
        None | Some(serde_yaml::Value::Null) => None,
        Some(serde_yaml::Value::String(s)) => {
            if !valid_identifier(s) {
                return Err(ManifestError::at(
                    yaml_path,
                    format!(
                        "tools[{idx}] bundled override {name:?}.rename must be a valid identifier \
                         (^[a-zA-Z_][a-zA-Z0-9_]*$), got {s:?}"
                    ),
                ));
            }
            Some(s.clone())
        }
        Some(_) => {
            return Err(ManifestError::at(
                yaml_path,
                format!("tools[{idx}] bundled override {name:?}.rename must be a string"),
            ))
        }
    };

    Ok(ToolSpec::Bundled(BundledOverride {
        name,
        description,
        hidden,
        rename,
    }))
}

fn build_embedder(
    raw: Option<&serde_yaml::Value>,
    yaml_path: &Path,
) -> Result<Option<EmbedderConfig>, ManifestError> {
    let Some(raw) = raw else { return Ok(None) };
    if matches!(raw, serde_yaml::Value::Null) {
        return Ok(None);
    }
    let map = raw
        .as_mapping()
        .ok_or_else(|| ManifestError::at(yaml_path, "embedder must be a mapping"))?;
    check_keys(map, ALLOWED_EMBEDDER_KEYS, "embedder keys", yaml_path)?;
    let module = map
        .get("module")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ManifestError::at(
                yaml_path,
                "embedder.module must be a non-empty string (path or dotted name)",
            )
        })?
        .to_string();
    let class = map
        .get("class")
        .and_then(|v| v.as_str())
        .filter(|s| valid_identifier(s))
        .ok_or_else(|| {
            ManifestError::at(
                yaml_path,
                "embedder.class must be a valid identifier matching ^[a-zA-Z_][a-zA-Z0-9_]*$",
            )
        })?
        .to_string();
    let kwargs = match map.get("kwargs") {
        None | Some(serde_yaml::Value::Null) => serde_json::Map::new(),
        Some(v) if v.is_mapping() => match yaml_to_json(v.clone())? {
            serde_json::Value::Object(o) => o,
            _ => {
                return Err(ManifestError::at(
                    yaml_path,
                    "embedder.kwargs must be a mapping",
                ))
            }
        },
        Some(_) => {
            return Err(ManifestError::at(
                yaml_path,
                "embedder.kwargs must be a mapping",
            ))
        }
    };
    Ok(Some(EmbedderConfig {
        module,
        class,
        kwargs,
    }))
}

fn build_builtins(
    raw: Option<&serde_yaml::Value>,
    yaml_path: &Path,
) -> Result<BuiltinsConfig, ManifestError> {
    let Some(raw) = raw else {
        return Ok(BuiltinsConfig::default());
    };
    if matches!(raw, serde_yaml::Value::Null) {
        return Ok(BuiltinsConfig::default());
    }
    let map = raw
        .as_mapping()
        .ok_or_else(|| ManifestError::at(yaml_path, "builtins must be a mapping"))?;
    check_keys(map, ALLOWED_BUILTIN_KEYS, "builtins keys", yaml_path)?;
    let mut cfg = BuiltinsConfig::default();
    if let Some(v) = map.get("save_graph") {
        cfg.save_graph = v
            .as_bool()
            .ok_or_else(|| ManifestError::at(yaml_path, "builtins.save_graph must be a bool"))?;
    }
    if let Some(v) = map.get("temp_cleanup") {
        let s = v.as_str().ok_or_else(|| {
            ManifestError::at(
                yaml_path,
                format!("builtins.temp_cleanup must be one of {VALID_TEMP_CLEANUP:?}"),
            )
        })?;
        cfg.temp_cleanup = match s {
            "never" => TempCleanup::Never,
            "on_overview" => TempCleanup::OnOverview,
            other => {
                return Err(ManifestError::at(
                    yaml_path,
                    format!(
                        "builtins.temp_cleanup must be one of {VALID_TEMP_CLEANUP:?}, got {other:?}"
                    ),
                ))
            }
        };
    }
    Ok(cfg)
}

fn valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn yaml_to_json(v: serde_yaml::Value) -> Result<serde_json::Value, ManifestError> {
    serde_json::to_value(&v)
        .map_err(|e| ManifestError::bare(format!("yaml→json conversion failed: {e}")))
}

#[derive(Debug, Deserialize)]
struct _Reserved;

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(text: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, text.as_bytes()).unwrap();
        f
    }

    #[test]
    fn loads_minimal_empty_manifest() {
        let f = write_tmp("");
        let m = load(f.path()).unwrap();
        assert_eq!(m.tools.len(), 0);
        assert_eq!(m.source_roots.len(), 0);
        assert!(!m.trust.allow_python_tools);
        assert!(!m.trust.allow_embedder);
        assert_eq!(m.builtins.temp_cleanup, TempCleanup::Never);
    }

    #[test]
    fn loads_name_and_instructions() {
        let f = write_tmp("name: Demo\ninstructions: |\n  multi-line\n  block\n");
        let m = load(f.path()).unwrap();
        assert_eq!(m.name.as_deref(), Some("Demo"));
        assert!(m.instructions.unwrap().contains("multi-line"));
    }

    #[test]
    fn rejects_unknown_top_key() {
        let f = write_tmp("bogus: 1\n");
        let err = load(f.path()).unwrap_err();
        assert!(err.message.contains("unknown top-level"));
    }

    #[test]
    fn source_root_string_normalises_to_list() {
        let f = write_tmp("source_root: ./data\n");
        let m = load(f.path()).unwrap();
        assert_eq!(m.source_roots, vec!["./data".to_string()]);
    }

    #[test]
    fn source_roots_list_preserved() {
        let f = write_tmp("source_roots:\n  - ./a\n  - ./b\n");
        let m = load(f.path()).unwrap();
        assert_eq!(m.source_roots, vec!["./a".to_string(), "./b".to_string()]);
    }

    #[test]
    fn rejects_both_source_root_and_source_roots() {
        let f = write_tmp("source_root: ./a\nsource_roots: [./b]\n");
        assert!(load(f.path()).unwrap_err().message.contains("not both"));
    }

    #[test]
    fn cypher_tool_parses() {
        let f = write_tmp("tools:\n  - name: lookup\n    cypher: MATCH (n) RETURN n\n");
        let m = load(f.path()).unwrap();
        assert_eq!(m.tools.len(), 1);
        match &m.tools[0] {
            ToolSpec::Cypher(t) => {
                assert_eq!(t.name, "lookup");
                assert!(t.cypher.contains("MATCH"));
            }
            _ => panic!("expected cypher tool"),
        }
    }

    #[test]
    fn python_tool_parses() {
        let f =
            write_tmp("tools:\n  - name: detail\n    python: ./tools.py\n    function: detail\n");
        let m = load(f.path()).unwrap();
        match &m.tools[0] {
            ToolSpec::Python(t) => {
                assert_eq!(t.python, "./tools.py");
                assert_eq!(t.function, "detail");
            }
            _ => panic!("expected python tool"),
        }
    }

    #[test]
    fn rejects_tool_with_both_kinds() {
        let f = write_tmp(
            "tools:\n  - name: x\n    cypher: 'MATCH (n) RETURN n'\n    python: ./t.py\n    function: x\n",
        );
        assert!(load(f.path())
            .unwrap_err()
            .message
            .contains("multiple kinds"));
    }

    #[test]
    fn rejects_tool_with_no_kind() {
        let f = write_tmp("tools:\n  - name: x\n");
        assert!(load(f.path())
            .unwrap_err()
            .message
            .contains("needs exactly one"));
    }

    #[test]
    fn rejects_duplicate_tool_names() {
        let f = write_tmp(
            "tools:\n  - name: same\n    cypher: 'MATCH (n) RETURN n'\n  - name: same\n    cypher: 'MATCH (m) RETURN m'\n",
        );
        assert!(load(f.path()).unwrap_err().message.contains("duplicate"));
    }

    // ─── Bundled override shape (0.3.31) ────────────────────────

    #[test]
    fn bundled_override_with_description_parses() {
        let f =
            write_tmp("tools:\n  - bundled: repo_management\n    description: \"FIRST STEP\"\n");
        let m = load(f.path()).unwrap();
        assert_eq!(m.tools.len(), 1);
        match &m.tools[0] {
            ToolSpec::Bundled(b) => {
                assert_eq!(b.name, "repo_management");
                assert_eq!(b.description.as_deref(), Some("FIRST STEP"));
                assert!(!b.hidden);
            }
            _ => panic!("expected bundled override"),
        }
    }

    #[test]
    fn bundled_override_with_hidden_parses() {
        let f = write_tmp("tools:\n  - bundled: ping\n    hidden: true\n");
        let m = load(f.path()).unwrap();
        match &m.tools[0] {
            ToolSpec::Bundled(b) => {
                assert_eq!(b.name, "ping");
                assert!(b.hidden);
                assert!(b.description.is_none());
            }
            _ => panic!("expected bundled override"),
        }
    }

    #[test]
    fn bundled_override_alongside_cypher_tools_parses() {
        let f = write_tmp(
            "tools:\n\
             \x20\x20- bundled: cypher_query\n\
             \x20\x20\x20\x20description: \"Custom server description\"\n\
             \x20\x20- name: lookup\n\
             \x20\x20\x20\x20cypher: \"MATCH (n) RETURN n\"\n",
        );
        let m = load(f.path()).unwrap();
        assert_eq!(m.tools.len(), 2);
        assert!(matches!(m.tools[0], ToolSpec::Bundled(_)));
        assert!(matches!(m.tools[1], ToolSpec::Cypher(_)));
    }

    #[test]
    fn rejects_bundled_with_cypher_kind() {
        let f =
            write_tmp("tools:\n  - bundled: cypher_query\n    cypher: \"MATCH (n) RETURN n\"\n");
        let err = load(f.path()).unwrap_err();
        assert!(
            err.message.contains("multiple kinds"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_bundled_with_name_field() {
        let f = write_tmp("tools:\n  - bundled: ping\n    name: ping\n");
        let err = load(f.path()).unwrap_err();
        assert!(
            err.message.contains("cannot set `name:`"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_bundled_with_parameters_field() {
        let f =
            write_tmp("tools:\n  - bundled: cypher_query\n    parameters:\n      type: object\n");
        let err = load(f.path()).unwrap_err();
        assert!(
            err.message.contains("cannot set `parameters:`"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_bundled_with_non_bool_hidden() {
        let f = write_tmp("tools:\n  - bundled: ping\n    hidden: yes-please\n");
        let err = load(f.path()).unwrap_err();
        assert!(
            err.message.contains("hidden must be a bool"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_hidden_on_cypher_tool() {
        let f = write_tmp(
            "tools:\n  - name: lookup\n    cypher: \"MATCH (n) RETURN n\"\n    hidden: true\n",
        );
        let err = load(f.path()).unwrap_err();
        assert!(
            err.message
                .contains("`hidden:` is only valid on `bundled:` override entries"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_duplicate_bundled_overrides() {
        // The dedup check is on tool name; two `bundled: ping` entries
        // share the same name and should be rejected the same way
        // duplicate cypher tools are.
        let f = write_tmp(
            "tools:\n  - bundled: ping\n    hidden: true\n  - bundled: ping\n    description: \"x\"\n",
        );
        assert!(load(f.path()).unwrap_err().message.contains("duplicate"));
    }

    #[test]
    fn rejects_bundled_with_invalid_identifier() {
        let f = write_tmp("tools:\n  - bundled: \"123-bad\"\n    hidden: true\n");
        let err = load(f.path()).unwrap_err();
        assert!(
            err.message.contains("must be a string"),
            "got: {}",
            err.message
        );
    }

    // 0.3.34 — `tools[].bundled: rename:` per-deployment override
    #[test]
    fn bundled_rename_parses_when_valid_identifier() {
        let f = write_tmp("tools:\n  - bundled: cypher_query\n    rename: legal_cypher_query\n");
        let m = load(f.path()).unwrap();
        match &m.tools[0] {
            ToolSpec::Bundled(b) => {
                assert_eq!(b.name, "cypher_query");
                assert_eq!(b.rename.as_deref(), Some("legal_cypher_query"));
                assert!(!b.hidden);
                assert!(b.description.is_none());
            }
            _ => panic!("expected bundled override"),
        }
    }

    #[test]
    fn bundled_rename_alongside_description_parses() {
        let f = write_tmp(
            "tools:\n  - bundled: cypher_query\n    rename: legal_cypher_query\n    description: \"Legal-corpus cypher\"\n",
        );
        let m = load(f.path()).unwrap();
        match &m.tools[0] {
            ToolSpec::Bundled(b) => {
                assert_eq!(b.rename.as_deref(), Some("legal_cypher_query"));
                assert_eq!(b.description.as_deref(), Some("Legal-corpus cypher"));
            }
            _ => panic!("expected bundled override"),
        }
    }

    #[test]
    fn bundled_rename_defaults_to_none() {
        let f = write_tmp("tools:\n  - bundled: cypher_query\n    description: \"x\"\n");
        let m = load(f.path()).unwrap();
        match &m.tools[0] {
            ToolSpec::Bundled(b) => assert!(b.rename.is_none()),
            _ => panic!("expected bundled override"),
        }
    }

    #[test]
    fn rejects_bundled_rename_with_invalid_identifier() {
        let f = write_tmp("tools:\n  - bundled: cypher_query\n    rename: \"123-bad\"\n");
        let err = load(f.path()).unwrap_err();
        assert!(
            err.message.contains("rename must be a valid identifier"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_bundled_rename_with_non_string_value() {
        let f = write_tmp("tools:\n  - bundled: cypher_query\n    rename: 42\n");
        let err = load(f.path()).unwrap_err();
        assert!(
            err.message.contains("rename must be a string"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn bundled_rename_serialises_to_json() {
        let f = write_tmp("tools:\n  - bundled: cypher_query\n    rename: legal_cypher_query\n");
        let m = load(f.path()).unwrap();
        let json = m.to_json();
        let tools = json.get("tools").and_then(|t| t.as_array()).unwrap();
        let entry = &tools[0];
        assert_eq!(entry.get("kind").and_then(|v| v.as_str()), Some("bundled"));
        assert_eq!(
            entry.get("name").and_then(|v| v.as_str()),
            Some("cypher_query")
        );
        assert_eq!(
            entry.get("rename").and_then(|v| v.as_str()),
            Some("legal_cypher_query")
        );
    }

    #[test]
    fn bundled_override_to_json_shape() {
        let f = write_tmp(
            "tools:\n  - bundled: repo_management\n    description: \"FIRST STEP\"\n    hidden: false\n",
        );
        let m = load(f.path()).unwrap();
        let v = m.to_json();
        assert_eq!(v["tools"][0]["kind"], "bundled");
        assert_eq!(v["tools"][0]["name"], "repo_management");
        assert_eq!(v["tools"][0]["description"], "FIRST STEP");
        assert_eq!(v["tools"][0]["hidden"], false);
    }

    #[test]
    fn embedder_parses() {
        let f = write_tmp(
            "embedder:\n  module: ./e.py\n  class: GraphEmbedder\n  kwargs:\n    cooldown: 900\n",
        );
        let m = load(f.path()).unwrap();
        let e = m.embedder.unwrap();
        assert_eq!(e.module, "./e.py");
        assert_eq!(e.class, "GraphEmbedder");
        assert_eq!(e.kwargs.get("cooldown").unwrap().as_i64(), Some(900));
    }

    #[test]
    fn builtins_parses_temp_cleanup() {
        let f = write_tmp("builtins:\n  save_graph: true\n  temp_cleanup: on_overview\n");
        let m = load(f.path()).unwrap();
        assert!(m.builtins.save_graph);
        assert_eq!(m.builtins.temp_cleanup, TempCleanup::OnOverview);
    }

    #[test]
    fn rejects_invalid_temp_cleanup() {
        let f = write_tmp("builtins:\n  temp_cleanup: nuke\n");
        assert!(load(f.path()).unwrap_err().message.contains("temp_cleanup"));
    }

    #[test]
    fn allow_embedder_trust_parses() {
        let f = write_tmp("trust:\n  allow_embedder: true\n");
        let m = load(f.path()).unwrap();
        assert!(m.trust.allow_embedder);
    }

    #[test]
    fn allow_query_preprocessor_trust_parses() {
        let f = write_tmp("trust:\n  allow_query_preprocessor: true\n");
        let m = load(f.path()).unwrap();
        assert!(m.trust.allow_query_preprocessor);
        assert!(!m.trust.allow_embedder);
        assert!(!m.trust.allow_python_tools);
    }

    #[test]
    fn allow_query_preprocessor_rejects_non_bool() {
        let f = write_tmp("trust:\n  allow_query_preprocessor: \"yes\"\n");
        let err = load(f.path()).unwrap_err();
        assert!(err
            .message
            .contains("allow_query_preprocessor must be a bool"));
    }

    #[test]
    fn find_sibling_works() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("demo.kgl");
        std::fs::write(&graph, b"\x00").unwrap();
        let sibling = dir.path().join("demo_mcp.yaml");
        std::fs::write(&sibling, "name: x\n").unwrap();
        assert_eq!(find_sibling_manifest(&graph), Some(sibling));
    }

    #[test]
    fn workspace_local_parses() {
        let f = write_tmp("workspace:\n  kind: local\n  root: ./src\n  watch: true\n");
        let m = load(f.path()).unwrap();
        let w = m.workspace.unwrap();
        assert_eq!(w.kind, WorkspaceKind::Local);
        assert_eq!(w.root.as_deref(), Some("./src"));
        assert!(w.watch);
    }

    #[test]
    fn workspace_github_default_kind() {
        let f = write_tmp("workspace: {}\n");
        let m = load(f.path()).unwrap();
        let w = m.workspace.unwrap();
        assert_eq!(w.kind, WorkspaceKind::Github);
        assert!(w.root.is_none());
        assert!(!w.watch);
    }

    #[test]
    fn workspace_local_without_root_errors() {
        let f = write_tmp("workspace:\n  kind: local\n");
        let err = load(f.path()).unwrap_err();
        assert!(err.message.contains("requires workspace.root"));
    }

    #[test]
    fn workspace_unknown_key_rejected() {
        let f = write_tmp("workspace:\n  kind: local\n  root: ./x\n  bogus: 1\n");
        let err = load(f.path()).unwrap_err();
        assert!(err.message.contains("unknown workspace keys"));
    }

    #[test]
    fn workspace_invalid_kind_rejected() {
        let f = write_tmp("workspace:\n  kind: docker\n  root: ./x\n");
        let err = load(f.path()).unwrap_err();
        assert!(err.message.contains("workspace.kind"));
    }

    #[test]
    fn workspace_watch_invalid_for_github() {
        let f = write_tmp("workspace:\n  kind: github\n  watch: true\n");
        let err = load(f.path()).unwrap_err();
        assert!(err.message.contains("watch is only valid"));
    }

    #[test]
    fn extensions_passthrough_parses() {
        let f = write_tmp(
            "extensions:\n  csv_http_server: true\n  csv_http_server_dir: temp/\n  arbitrary:\n    nested: 1\n",
        );
        let m = load(f.path()).unwrap();
        assert_eq!(
            m.extensions
                .get("csv_http_server")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            m.extensions
                .get("csv_http_server_dir")
                .and_then(|v| v.as_str()),
            Some("temp/")
        );
        // Nested values pass through unchanged.
        assert_eq!(
            m.extensions
                .get("arbitrary")
                .and_then(|v| v.get("nested"))
                .and_then(|v| v.as_i64()),
            Some(1)
        );
    }

    #[test]
    fn extensions_absent_defaults_to_empty() {
        let f = write_tmp("name: x\n");
        let m = load(f.path()).unwrap();
        assert!(m.extensions.is_empty());
    }

    #[test]
    fn extensions_inner_keys_unvalidated() {
        // The framework intentionally does NOT validate keys inside
        // `extensions:` — they're downstream-binary concerns. Any shape
        // that's a YAML mapping must round-trip.
        let f = write_tmp(
            "extensions:\n  whatever_kglite_wants: foo\n  some_other_consumer: { a: 1, b: 2 }\n",
        );
        load(f.path()).unwrap();
    }

    #[test]
    fn extensions_must_be_a_mapping() {
        let f = write_tmp("extensions: not-a-mapping\n");
        let err = load(f.path()).unwrap_err();
        assert!(err.message.contains("extensions must be a mapping"));
    }

    #[test]
    fn env_file_key_parses() {
        let f = write_tmp("env_file: ../.env\n");
        let m = load(f.path()).unwrap();
        assert_eq!(m.env_file.as_deref(), Some("../.env"));
    }

    #[test]
    fn env_file_unset_is_none() {
        let f = write_tmp("name: Demo\n");
        let m = load(f.path()).unwrap();
        assert!(m.env_file.is_none());
    }

    #[test]
    fn find_workspace_works() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("workspace_mcp.yaml");
        std::fs::write(&manifest, "name: ws\n").unwrap();
        assert_eq!(find_workspace_manifest(dir.path()), Some(manifest));
    }

    #[test]
    fn find_workspace_walks_one_level_up_with_applies_to() {
        // Layout: <tmp>/parent/workspace_mcp.yaml (declares
        // workspace.applies_to: ./repos) + <tmp>/parent/repos/.
        // Discovery from <tmp>/parent/repos/ should walk up one level
        // and find the sibling manifest because applies_to matches.
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let manifest = parent.join("workspace_mcp.yaml");
        std::fs::write(
            &manifest,
            "workspace:\n  kind: github\n  applies_to: ./repos\n",
        )
        .unwrap();
        let repos = parent.join("repos");
        std::fs::create_dir(&repos).unwrap();

        // Primary location still works.
        assert_eq!(find_workspace_manifest(&parent), Some(manifest.clone()));

        // Parent-walk fallback resolves to the same manifest. Compare
        // canonicalised paths to handle macOS /private/var vs /var.
        let found = find_workspace_manifest(&repos).expect("parent fallback should fire");
        assert_eq!(
            found.canonicalize().unwrap(),
            manifest.canonicalize().unwrap()
        );
    }

    #[test]
    fn find_workspace_ignores_parent_without_applies_to() {
        // Parent manifest exists but does NOT declare workspace.applies_to.
        // The parent-walk fallback must refuse to auto-detect it —
        // otherwise an unrelated workspace_mcp.yaml in a sibling dir
        // could surprise-attach to whatever --workspace path the
        // operator passes. Safe default: require the opt-in.
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let manifest = parent.join("workspace_mcp.yaml");
        std::fs::write(&manifest, "name: not for repos\n").unwrap();
        let repos = parent.join("repos");
        std::fs::create_dir(&repos).unwrap();

        assert_eq!(
            find_workspace_manifest(&repos),
            None,
            "parent manifest without workspace.applies_to must NOT auto-attach"
        );
    }

    #[test]
    fn find_workspace_ignores_parent_with_mismatched_applies_to() {
        // Parent manifest declares applies_to: ./repos but the
        // actual --workspace path is ./other_dir. The mismatch must
        // suppress auto-detection.
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let manifest = parent.join("workspace_mcp.yaml");
        std::fs::write(
            &manifest,
            "workspace:\n  kind: github\n  applies_to: ./repos\n",
        )
        .unwrap();
        let other = parent.join("other_dir");
        std::fs::create_dir(&other).unwrap();

        assert_eq!(
            find_workspace_manifest(&other),
            None,
            "applies_to: ./repos must NOT match --workspace ./other_dir"
        );
    }

    #[test]
    fn find_workspace_applies_to_wildcard_matches_any_child() {
        // applies_to: '*' (or './*') means "any direct child of the
        // manifest's parent dir." Three different child names should
        // all auto-detect the manifest.
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let manifest = parent.join("workspace_mcp.yaml");
        std::fs::write(&manifest, "workspace:\n  kind: github\n  applies_to: '*'\n").unwrap();
        for child_name in ["repos", "clones", "totally-different-name"] {
            let child = parent.join(child_name);
            std::fs::create_dir(&child).unwrap();
            let found =
                find_workspace_manifest(&child).expect("wildcard should match any direct child");
            assert_eq!(
                found.canonicalize().unwrap(),
                manifest.canonicalize().unwrap(),
                "wildcard should match child {child_name:?}"
            );
        }
    }

    #[test]
    fn find_workspace_applies_to_glob_matches_prefix() {
        // applies_to: './prod-*' should match any direct child whose
        // basename starts with "prod-".
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let manifest = parent.join("workspace_mcp.yaml");
        std::fs::write(
            &manifest,
            "workspace:\n  kind: github\n  applies_to: ./prod-*\n",
        )
        .unwrap();
        // Match cases.
        for child_name in ["prod-api", "prod-web", "prod-"] {
            let child = parent.join(child_name);
            std::fs::create_dir(&child).unwrap();
            assert!(
                find_workspace_manifest(&child).is_some(),
                "prod-* should match {child_name:?}"
            );
        }
        // Non-match cases.
        for child_name in ["test-api", "stage-web", "random"] {
            let child = parent.join(child_name);
            std::fs::create_dir(&child).unwrap();
            assert_eq!(
                find_workspace_manifest(&child),
                None,
                "prod-* should NOT match {child_name:?}"
            );
        }
    }

    #[test]
    fn find_workspace_applies_to_list_matches_any_entry() {
        // applies_to: [./repos, ./clones] should match either name
        // but reject anything else.
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let manifest = parent.join("workspace_mcp.yaml");
        std::fs::write(
            &manifest,
            "workspace:\n  kind: github\n  applies_to:\n    - ./repos\n    - ./clones\n",
        )
        .unwrap();
        for matching in ["repos", "clones"] {
            let child = parent.join(matching);
            std::fs::create_dir(&child).unwrap();
            assert!(
                find_workspace_manifest(&child).is_some(),
                "list should match {matching:?}"
            );
        }
        let other = parent.join("scratch");
        std::fs::create_dir(&other).unwrap();
        assert_eq!(
            find_workspace_manifest(&other),
            None,
            "list with [repos, clones] must NOT match scratch"
        );
    }

    #[test]
    fn applies_to_rejects_deep_path_at_parse_time() {
        let f = write_tmp("workspace:\n  kind: github\n  applies_to: ./too/deep/path\n");
        let err = load(f.path()).unwrap_err();
        assert!(
            err.message.contains("must be a single path segment"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn applies_to_rejects_invalid_glob_at_parse_time() {
        // globset rejects unterminated character class.
        let f = write_tmp("workspace:\n  kind: github\n  applies_to: './[unterminated'\n");
        let err = load(f.path()).unwrap_err();
        assert!(
            err.message.contains("invalid glob pattern"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn applies_to_rejects_parent_relative() {
        // Bare `..` is caught by the `..` rejection branch. The
        // multi-segment form `../foo` is caught earlier by the
        // single-segment check; either is rejected.
        let f = write_tmp("workspace:\n  kind: github\n  applies_to: '..'\n");
        let err = load(f.path()).unwrap_err();
        assert!(err.message.contains("must not contain `..`"));

        let f2 = write_tmp("workspace:\n  kind: github\n  applies_to: '../up'\n");
        let err2 = load(f2.path()).unwrap_err();
        assert!(err2.message.contains("must be a single path segment"));
    }

    #[test]
    fn find_workspace_returns_none_when_missing_everywhere() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        std::fs::create_dir(&child).unwrap();
        // No manifest in either child or its parent (tmpdir root).
        assert_eq!(find_workspace_manifest(&child), None);
    }

    #[test]
    fn find_workspace_primary_wins_over_parent_fallback() {
        // Both primary AND parent-fallback exist. The primary must
        // win — this anchors the precedence rule documented on
        // `find_workspace_manifest`. The parent declares applies_to
        // matching the child dir, so it WOULD be a valid fallback —
        // but the primary preempts it. If a future refactor swaps
        // the order, this test fails loudly.
        let dir = tempfile::tempdir().unwrap();
        let parent_manifest = dir.path().join("workspace_mcp.yaml");
        std::fs::write(
            &parent_manifest,
            "workspace:\n  kind: github\n  applies_to: ./repos\n",
        )
        .unwrap();
        let child = dir.path().join("repos");
        std::fs::create_dir(&child).unwrap();
        let child_manifest = child.join("workspace_mcp.yaml");
        std::fs::write(&child_manifest, "name: child\n").unwrap();

        // Discovery from `child` should return the child manifest,
        // NOT the parent's. Compare canonicalised to handle the
        // macOS /private/var vs /var symlink consistently.
        let found = find_workspace_manifest(&child).expect("primary should resolve");
        assert_eq!(
            found.canonicalize().unwrap(),
            child_manifest.canonicalize().unwrap(),
            "primary location must win when both primary and parent fallback exist"
        );
    }

    #[test]
    fn to_json_shape_is_stable() {
        let f = write_tmp(
            r#"
name: KGLite Codebase
source_roots: [src, lib]
trust:
  allow_embedder: true
embedder:
  module: kglite.embed
  class: SentenceTransformerEmbedder
builtins:
  save_graph: true
  temp_cleanup: on_overview
"#,
        );
        let m = load(f.path()).unwrap();
        let actual = m.to_json();
        let expected = serde_json::json!({
            "yaml_path": f.path().display().to_string(),
            "name": "KGLite Codebase",
            "instructions": null,
            "overview_prefix": null,
            "source_roots": ["src", "lib"],
            "trust": {
                "allow_python_tools": false,
                "allow_embedder": true,
                "allow_query_preprocessor": false,
            },
            "tools": [],
            "embedder": {
                "module": "kglite.embed",
                "class": "SentenceTransformerEmbedder",
                "kwargs": {},
            },
            "builtins": { "save_graph": true, "temp_cleanup": "on_overview" },
            "env_file": null,
            "workspace": null,
            "extensions": {},
            "skills": false,
        });
        assert_eq!(actual, expected);
    }

    #[test]
    fn to_json_round_trips_tools_and_workspace() {
        let f = write_tmp(
            r#"
name: Full Surface
source_root: ./src
trust:
  allow_python_tools: true
tools:
  - name: nodes_for
    cypher: "MATCH (n {name: $name}) RETURN n"
    description: "fetch nodes by name"
  - name: run_query
    python: tools.py
    function: run
workspace:
  kind: local
  root: /tmp/ws
  watch: true
builtins:
  save_graph: false
env_file: .env.local
extensions:
  kglite:
    flavour: standard
"#,
        );
        let m = load(f.path()).unwrap();
        let v = m.to_json();
        assert_eq!(v["name"], "Full Surface");
        assert_eq!(v["trust"]["allow_python_tools"], true);
        assert_eq!(v["workspace"]["kind"], "local");
        assert_eq!(v["workspace"]["root"], "/tmp/ws");
        assert_eq!(v["workspace"]["watch"], true);
        assert_eq!(v["env_file"], ".env.local");
        assert_eq!(v["tools"][0]["kind"], "cypher");
        assert_eq!(v["tools"][0]["name"], "nodes_for");
        assert_eq!(v["tools"][1]["kind"], "python");
        assert_eq!(v["tools"][1]["name"], "run_query");
        assert_eq!(v["tools"][1]["python"], "tools.py");
        assert_eq!(v["tools"][1]["function"], "run");
        assert_eq!(v["extensions"]["kglite"]["flavour"], "standard");
    }

    // ─── Skills schema (Phase 1a — manifest-level only) ───────────

    #[test]
    fn skills_disabled_by_default() {
        let f = write_tmp("name: x\n");
        let m = load(f.path()).unwrap();
        assert_eq!(m.skills, SkillsSource::Disabled);
        assert_eq!(m.to_json()["skills"], serde_json::Value::Bool(false));
    }

    #[test]
    fn skills_explicit_false_disabled() {
        let f = write_tmp("name: x\nskills: false\n");
        let m = load(f.path()).unwrap();
        assert_eq!(m.skills, SkillsSource::Disabled);
    }

    #[test]
    fn skills_bool_true_parses_to_single_bundled() {
        let f = write_tmp("name: x\nskills: true\n");
        let m = load(f.path()).unwrap();
        assert_eq!(m.skills, SkillsSource::Sources(vec![SkillSource::Bundled]));
        // JSON shape: list with one boolean true.
        let v = m.to_json();
        assert_eq!(v["skills"], serde_json::json!([true]));
    }

    #[test]
    fn skills_path_string_parses_to_single_path() {
        let f = write_tmp("name: x\nskills: ./local-skills/\n");
        let m = load(f.path()).unwrap();
        assert_eq!(
            m.skills,
            SkillsSource::Sources(vec![SkillSource::Path("./local-skills/".into())])
        );
        // JSON round-trip preserves the operator-declared path verbatim.
        let v = m.to_json();
        assert_eq!(v["skills"], serde_json::json!(["./local-skills/"]));
    }

    #[test]
    fn skills_list_polymorphic_parses() {
        let f =
            write_tmp("name: x\nskills:\n  - true\n  - ./local-overrides/\n  - ~/shared-skills/\n");
        let m = load(f.path()).unwrap();
        assert_eq!(
            m.skills,
            SkillsSource::Sources(vec![
                SkillSource::Bundled,
                SkillSource::Path("./local-overrides/".into()),
                SkillSource::Path("~/shared-skills/".into()),
            ])
        );
        // JSON preserves entry types: bool for bundled, string for paths.
        let v = m.to_json();
        assert_eq!(
            v["skills"],
            serde_json::json!([true, "./local-overrides/", "~/shared-skills/"])
        );
    }

    #[test]
    fn skills_empty_list_parses_as_opt_in_with_no_root_sources() {
        // Empty list means "opt in but only the auto-detected project
        // layer fires." The registry treats this as `Sources(vec![])`,
        // not `Disabled`. Operators relying solely on
        // `<basename>.skills/` adjacent to the YAML use this form.
        let f = write_tmp("name: x\nskills: []\n");
        let m = load(f.path()).unwrap();
        assert_eq!(m.skills, SkillsSource::Sources(vec![]));
    }

    #[test]
    fn skills_false_in_list_rejected() {
        let f = write_tmp("name: x\nskills:\n  - false\n");
        let err = load(f.path()).unwrap_err();
        assert!(
            err.message.contains("skills[0]")
                && err.message.contains("`false` is not a valid entry"),
            "unexpected: {}",
            err.message
        );
    }

    #[test]
    fn skills_invalid_type_rejected() {
        let f = write_tmp("name: x\nskills: 42\n");
        let err = load(f.path()).unwrap_err();
        assert!(
            err.message.contains("skills must be"),
            "unexpected: {}",
            err.message
        );
    }

    #[test]
    fn skills_empty_path_string_rejected() {
        let f = write_tmp("name: x\nskills: \"\"\n");
        let err = load(f.path()).unwrap_err();
        assert!(
            err.message.contains("non-empty string"),
            "unexpected: {}",
            err.message
        );
    }

    #[test]
    fn skills_field_is_purely_additive_on_existing_manifests() {
        // A manifest written before the skills field existed (i.e. no
        // `skills:` declaration) must still parse cleanly with
        // SkillsSource::Disabled. This is the "no impact on existing
        // MCP servers" guarantee at the schema level.
        let f = write_tmp(
            r#"
name: legacy
source_roots: [src]
trust:
  allow_python_tools: true
workspace:
  kind: github
"#,
        );
        let m = load(f.path()).unwrap();
        assert_eq!(m.skills, SkillsSource::Disabled);
        assert_eq!(m.to_json()["skills"], serde_json::Value::Bool(false));
    }
}
