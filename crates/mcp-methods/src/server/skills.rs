//! Skills-aware MCP — runtime types, frontmatter parsing, three-layer
//! resolution, and the [`Registry`] builder downstream binaries
//! consume to wire skills into their MCP server.
//!
//! # The shape downstream binaries adopt
//!
//! ```ignore
//! use mcp_methods::server::skills::{Registry, BundledSkill};
//! use mcp_methods::server::manifest::load;
//!
//! let manifest = load(yaml_path)?;
//! let registry = Registry::new()
//!     // Domain-specific bundled skills (one per custom tool):
//!     .add_bundled(BundledSkill {
//!         name: "cypher_query",
//!         body: include_str!("skills/cypher_query.md"),
//!     })
//!     .add_bundled(BundledSkill {
//!         name: "graph_overview",
//!         body: include_str!("skills/graph_overview.md"),
//!     })
//!     // Framework defaults (ripgrep, github_discussions, etc.):
//!     .merge_framework_defaults()
//!     // Operator-declared paths from the manifest's `skills:` field:
//!     .layer_dirs(&manifest.skills, &manifest.yaml_path)?
//!     // Project-local <basename>.skills/ adjacent to the YAML:
//!     .auto_detect_project_layer(&manifest.yaml_path)
//!     // Resolve all layers, run lint, return the resolved registry:
//!     .finalise()?;
//!
//! // Phase 1c wires this into `serve_prompts(&registry, &mut server)`.
//! ```
//!
//! # Three-layer composition
//!
//! 1. **Project layer (top priority).** Auto-detected from
//!    `<manifest_basename>.skills/` adjacent to the YAML. Files there
//!    override every other layer per skill name. This is the operator's
//!    per-deployment tweak zone.
//! 2. **Root layer (middle).** Each entry in the manifest's `skills:`
//!    list, walked in declaration order. First-match-per-name wins.
//!    This is where operator-curated domain skill-packs sit
//!    (`kglite-skills-legal/`, etc.).
//! 3. **Bundled layer (bottom).** Compile-time defaults shipped with
//!    `mcp-methods` plus any added by the downstream binary via
//!    [`Registry::add_bundled`]. Library authors ship protocol-level
//!    methodology here; operators inherit it.
//!
//! Within the bundled layer, the downstream binary's skills win over
//! the framework's defaults when names collide.
//!
//! # Static markdown — no dynamic rendering
//!
//! Skills are pure markdown bodies with YAML frontmatter. The framework
//! does NOT splice tool output, run shell commands, or evaluate
//! templates server-side. Skills teach the agent *how* to use tools;
//! tools provide dynamic content when invoked. This keeps skill loading
//! deterministic and cheap, and matches Anthropic's own skill format.
//!
//! See `dev-documentation/skills-aware-mcp.md` for the full design.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use super::manifest::{SkillSource, SkillsSource};

// ─── Public types ─────────────────────────────────────────────────

/// A compile-time bundled skill, embedded into the binary via
/// `include_str!`. Downstream binaries (e.g. `kglite-mcp-server`)
/// construct these for their custom tools; the framework constructs
/// them for its own (`grep`, `read_source`, etc.).
///
/// Bundled skills sit at the bottom of the three-layer composition —
/// project and root-layer entries override them when names collide.
#[derive(Debug, Clone)]
pub struct BundledSkill {
    /// Skill name. Must match the `name` field in the markdown
    /// frontmatter. Used as the lookup key in `prompts/get`.
    pub name: &'static str,
    /// The full SKILL.md content — frontmatter + body. Parsed at
    /// `Registry::add_bundled` time; malformed bundled skills are
    /// errors (caught by the framework's CI tests), not warnings.
    pub body: &'static str,
}

/// Parsed YAML frontmatter of a SKILL.md file.
///
/// Phase 1b stores all declared fields as raw values. Phase 1f / 2a
/// will add validation (`applies_to` semver checks, `references_tools`
/// against the active tool catalogue, `references_arguments` against
/// each tool's input schema). For now: parse and preserve; the lint
/// step in `Registry::finalise()` walks these and surfaces issues as
/// log warnings.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SkillFrontmatter {
    /// Skill name. Must match the lookup key used in `prompts/get`.
    /// Required; empty after deserialization triggers a clear
    /// [`SkillError::MissingRequiredField`] rather than a generic
    /// YAML parse failure.
    #[serde(default)]
    pub name: String,
    /// One-line description shown in `prompts/list`. Required —
    /// the agent uses this to decide whether to load the full body.
    #[serde(default)]
    pub description: String,

    /// Version constraints. Parsed lazily — Phase 1b stores raw
    /// values, Phase 1f adds semver validation.
    #[serde(default)]
    pub applies_to: Option<HashMap<String, String>>,

    /// Tools this skill teaches or references in prose. Used for
    /// auto-inject discoverability hints (Phase 1c) and staleness
    /// detection (Phase 1f).
    #[serde(default)]
    pub references_tools: Vec<String>,

    /// Specific tool argument names referenced in the skill body
    /// (e.g. `"cypher_query.format"`). Lint warns when references
    /// don't match the tool's actual input schema.
    #[serde(default)]
    pub references_arguments: Vec<String>,

    /// Graph properties / domain-specific references the skill calls
    /// out (e.g. `"Function.module"`). For domain skill-packs to
    /// declare their domain assumptions. The framework can't validate
    /// these statically; they're documentation-grade metadata.
    #[serde(default)]
    pub references_properties: Vec<String>,

    /// When `true` (the default) AND the skill's name matches a
    /// registered MCP tool, the framework injects a "see `prompts/get`
    /// `<name>` for full methodology" pointer into the tool's
    /// description. Phase 1c wires this up.
    #[serde(default = "default_auto_inject_hint")]
    pub auto_inject_hint: bool,

    /// `applies_when:` predicate set. Bounded — not a DSL. All
    /// populated fields must evaluate true (AND semantics) for the
    /// skill to surface in `prompts/list` and `prompts/get`. The
    /// framework dispatches `tool_registered` and `extension_enabled`
    /// itself; domain predicates (`graph_has_node_type`,
    /// `graph_has_property`) are evaluated via the optional
    /// [`SkillPredicateEvaluator`] registered on the
    /// [`Registry`].
    ///
    /// `None` (the default) means "always active" — the skill applies
    /// regardless of runtime state.
    #[serde(default)]
    pub applies_when: Option<AppliesWhen>,
}

fn default_auto_inject_hint() -> bool {
    true
}

/// The parsed shape of a SKILL.md's `applies_when:` block. Each field
/// is one predicate; `None` means "this predicate is not applied".
/// All populated fields are ANDed.
///
/// Adding a new predicate requires extending this struct and the
/// matching arm in [`Registry::evaluate_clause`]. The bounded-set
/// design is intentional — operators get type-checked semantics
/// instead of an open-ended DSL.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct AppliesWhen {
    /// Active when the running graph has *any* of the listed node
    /// types in its schema. Domain predicate — evaluated via the
    /// consumer's [`SkillPredicateEvaluator`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_has_node_type: Option<Vec<String>>,

    /// Active when the running graph has the named property on the
    /// named node type. Domain predicate — evaluated via the
    /// consumer's [`SkillPredicateEvaluator`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_has_property: Option<GraphPropertyCheck>,

    /// Active when the named tool is in the registered catalogue
    /// at boot. Framework-internal — dispatched against
    /// `server.tool_router` without consulting any evaluator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_registered: Option<String>,

    /// Active when the manifest's `extensions:` block has the named
    /// key set to a truthy value (not absent, not null, not `false`).
    /// Framework-internal — dispatched against `manifest.extensions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_enabled: Option<String>,
}

/// Nested shape for the `graph_has_property:` predicate.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct GraphPropertyCheck {
    pub node_type: String,
    pub prop_name: String,
}

/// A single predicate clause, passed to a
/// [`SkillPredicateEvaluator`] one at a time. Borrowed slices so
/// the evaluator doesn't have to allocate.
#[derive(Debug)]
pub enum PredicateClause<'a> {
    /// `graph_has_node_type: [Function, Class]`
    GraphHasNodeType(&'a [String]),
    /// `graph_has_property: { node_type: Function, prop_name: module }`
    GraphHasProperty {
        node_type: &'a str,
        prop_name: &'a str,
    },
    /// `tool_registered: cypher_query`
    ToolRegistered(&'a str),
    /// `extension_enabled: csv_http_server`
    ExtensionEnabled(&'a str),
}

/// Per-clause result of evaluating an `applies_when:` block. Surfaced
/// via [`SkillActivation`] so the operator-facing `skills-list` and
/// boot log can show *which* predicate suppressed a skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PredicateOutcome {
    /// Predicate evaluated to true.
    Satisfied,
    /// Predicate evaluated to false. The skill is inactive.
    Unsatisfied,
    /// No evaluator recognized the predicate. Treated as
    /// `Unsatisfied` for safety — a typo'd predicate must not
    /// silently activate the skill against the wrong domain.
    Unknown,
}

/// Activation state for a single skill, post-predicate-evaluation.
/// Skills without an `applies_when:` block resolve to `Active` with
/// an empty `clauses` vec.
#[derive(Debug, Clone, Default)]
pub struct SkillActivation {
    /// Whether the skill should appear in `prompts/list` /
    /// `prompts/get`.
    pub active: bool,
    /// Per-clause evaluation outcomes, in declaration order. Empty
    /// for skills without an `applies_when:` block.
    pub clauses: Vec<(String, PredicateOutcome)>,
}

/// Trait downstream binaries implement to evaluate domain-specific
/// predicates. Framework-internal predicates (`tool_registered`,
/// `extension_enabled`) are dispatched without consulting this trait;
/// you only handle the domain ones (`graph_has_node_type`,
/// `graph_has_property`).
///
/// Return `Some(true)` / `Some(false)` when you have an answer;
/// return `None` when the predicate doesn't apply to your domain
/// (the framework will mark it `Unknown` and the skill will be
/// inactive — safer than silently activating the wrong skill).
///
/// # Example
///
/// ```ignore
/// struct KgliteEvaluator {
///     graph: Arc<Graph>,
/// }
///
/// impl SkillPredicateEvaluator for KgliteEvaluator {
///     fn evaluate(&self, clause: &PredicateClause<'_>) -> Option<bool> {
///         match clause {
///             PredicateClause::GraphHasNodeType(types) => {
///                 Some(types.iter().any(|t| self.graph.has_node_type(t)))
///             }
///             PredicateClause::GraphHasProperty { node_type, prop_name } => {
///                 Some(self.graph.has_property(node_type, prop_name))
///             }
///             _ => None,   // framework dispatches the rest
///         }
///     }
/// }
/// ```
pub trait SkillPredicateEvaluator: Send + Sync {
    fn evaluate(&self, clause: &PredicateClause<'_>) -> Option<bool>;
}

/// Where a [`Skill`] came from. Used for the boot-time collision-
/// resolution log and surfaced via the JSON shape kglite consumes
/// from `to_json()` (in Phase 1d).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillProvenance {
    /// Auto-detected from `<basename>.skills/` adjacent to the
    /// manifest YAML — top-priority operator overrides.
    Project,
    /// Loaded from an operator-declared path in the manifest's
    /// `skills:` list (a domain skill-pack or shared library).
    DomainPack(PathBuf),
    /// Compile-time bundled — shipped with `mcp-methods` (framework
    /// defaults) or with a downstream binary like `kglite-mcp-server`.
    Bundled,
}

/// A loaded skill, post-parse + post-resolution. The body is the
/// markdown content after the closing `---` frontmatter delimiter.
#[derive(Debug, Clone)]
pub struct Skill {
    pub frontmatter: SkillFrontmatter,
    pub body: String,
    pub provenance: SkillProvenance,
}

impl Skill {
    /// Convenience accessor for the skill's name (read from
    /// frontmatter at parse time).
    pub fn name(&self) -> &str {
        &self.frontmatter.name
    }

    /// One-line description for `prompts/list` responses.
    pub fn description(&self) -> &str {
        &self.frontmatter.description
    }
}

// ─── Errors ───────────────────────────────────────────────────────

/// Errors surfaced during skill loading + resolution. Variants are
/// kept distinct so downstream binaries (and the future skills-lint
/// CLI) can report locations and surface fixes precisely.
#[derive(Debug)]
pub enum SkillError {
    /// Filesystem error reading the skill file.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Missing or malformed frontmatter delimiters.
    MissingFrontmatter { path: PathBuf },
    /// Frontmatter present but invalid YAML.
    InvalidFrontmatter { path: PathBuf, message: String },
    /// Required frontmatter field missing (name or description).
    MissingRequiredField { path: PathBuf, field: &'static str },
    /// Skill body exceeds the hard size limit (16 KB by default).
    SkillTooLarge {
        path: PathBuf,
        bytes: usize,
        limit: usize,
    },
    /// Path declared in the manifest's `skills:` list doesn't exist
    /// or isn't a directory.
    PathNotFound { raw: String, resolved: PathBuf },
    /// Compile-time bundled skill (added via `add_bundled`) failed to
    /// parse. This is a framework-author or downstream-binary-author
    /// bug — the bundled skill files should round-trip through their
    /// own CI tests before shipping.
    BundledSkillInvalid { name: &'static str, message: String },
}

impl std::fmt::Display for SkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillError::Io { path, source } => {
                write!(f, "skill I/O error at {}: {source}", path.display())
            }
            SkillError::MissingFrontmatter { path } => write!(
                f,
                "skill at {} is missing the `---` YAML frontmatter delimiter at the start of the file",
                path.display()
            ),
            SkillError::InvalidFrontmatter { path, message } => {
                write!(
                    f,
                    "skill frontmatter at {} is not valid YAML: {message}",
                    path.display()
                )
            }
            SkillError::MissingRequiredField { path, field } => write!(
                f,
                "skill at {} is missing required frontmatter field `{field}`",
                path.display()
            ),
            SkillError::SkillTooLarge {
                path,
                bytes,
                limit,
            } => write!(
                f,
                "skill at {} is {bytes} bytes; exceeds the {limit} byte hard limit",
                path.display()
            ),
            SkillError::PathNotFound { raw, resolved } => write!(
                f,
                "skill path {raw:?} (resolved to {}) does not exist or is not a directory",
                resolved.display()
            ),
            SkillError::BundledSkillInvalid { name, message } => write!(
                f,
                "bundled skill `{name}` is malformed: {message}"
            ),
        }
    }
}

impl std::error::Error for SkillError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SkillError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

// ─── Size limits ──────────────────────────────────────────────────

/// Per-skill soft limit. Loading a skill larger than this logs a
/// warning via `tracing::warn!` but does not fail.
pub const SOFT_SIZE_LIMIT_BYTES: usize = 4 * 1024;
/// Per-skill hard limit. Loading a skill larger than this returns
/// [`SkillError::SkillTooLarge`]. Forces authors to keep skills
/// tight and prevents accidental dump-the-whole-onboarding-doc.
pub const HARD_SIZE_LIMIT_BYTES: usize = 16 * 1024;
/// Total session limit across all resolved skills. Exceeding this
/// logs a warning at `Registry::finalise` time but does not drop
/// skills automatically — operators stay in control of which skills
/// they want loaded.
pub const SESSION_TOTAL_LIMIT_BYTES: usize = 64 * 1024;

// ─── Frontmatter parser ───────────────────────────────────────────

/// Split a SKILL.md file into its YAML frontmatter and markdown body.
///
/// Returns the frontmatter content (without the `---` delimiters) and
/// the body (everything after the closing `---`).
///
/// The frontmatter MUST start at byte 0 of the file with the opening
/// `---` on its own line, and MUST be terminated by a `---` on its
/// own line. This matches Jekyll / Hugo / Anthropic-skills convention.
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let trimmed = content.strip_prefix("---\n").or_else(|| {
        // Handle CRLF line endings.
        content.strip_prefix("---\r\n")
    })?;
    // Find the closing `---` on its own line.
    let mut search_start = 0;
    while let Some(idx) = trimmed[search_start..].find("---") {
        let abs = search_start + idx;
        // Must be at the start of a line.
        let at_line_start = abs == 0 || trimmed.as_bytes().get(abs - 1) == Some(&b'\n');
        // Must be followed by `\n`, `\r\n`, or end of file.
        let after = &trimmed[abs + 3..];
        let line_end_ok = after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n");
        if at_line_start && line_end_ok {
            let frontmatter = &trimmed[..abs];
            let body_start = if after.starts_with("\r\n") {
                abs + 3 + 2
            } else if after.starts_with('\n') {
                abs + 3 + 1
            } else {
                abs + 3
            };
            let body = &trimmed[body_start..];
            return Some((frontmatter, body));
        }
        search_start = abs + 3;
    }
    None
}

/// Parse a SKILL.md content blob into its frontmatter struct and
/// markdown body.
pub fn parse_skill(content: &str, path: &Path) -> Result<(SkillFrontmatter, String), SkillError> {
    let (frontmatter_str, body) =
        split_frontmatter(content).ok_or_else(|| SkillError::MissingFrontmatter {
            path: path.to_path_buf(),
        })?;

    let frontmatter: SkillFrontmatter =
        serde_yaml::from_str(frontmatter_str).map_err(|e| SkillError::InvalidFrontmatter {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

    if frontmatter.name.is_empty() {
        return Err(SkillError::MissingRequiredField {
            path: path.to_path_buf(),
            field: "name",
        });
    }
    if frontmatter.description.is_empty() {
        return Err(SkillError::MissingRequiredField {
            path: path.to_path_buf(),
            field: "description",
        });
    }

    Ok((frontmatter, body.to_string()))
}

// ─── Skill loaders ────────────────────────────────────────────────

/// Load a single skill from a file path.
pub fn load_skill_from_file(path: &Path, provenance: SkillProvenance) -> Result<Skill, SkillError> {
    let content = fs::read_to_string(path).map_err(|e| SkillError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    if content.len() > HARD_SIZE_LIMIT_BYTES {
        return Err(SkillError::SkillTooLarge {
            path: path.to_path_buf(),
            bytes: content.len(),
            limit: HARD_SIZE_LIMIT_BYTES,
        });
    }
    if content.len() > SOFT_SIZE_LIMIT_BYTES {
        tracing::warn!(
            path = %path.display(),
            bytes = content.len(),
            soft_limit = SOFT_SIZE_LIMIT_BYTES,
            "skill exceeds the soft size limit; consider splitting"
        );
    }

    let (frontmatter, body) = parse_skill(&content, path)?;
    Ok(Skill {
        frontmatter,
        body,
        provenance,
    })
}

/// A non-fatal warning emitted while loading skills — a single file
/// failed to parse, but the rest of the directory was loaded
/// successfully.
///
/// Surfaced on [`ResolvedRegistry::parse_warnings`] so downstream
/// binaries can render warnings in their boot summary. Operators
/// previously had to set up tracing-subscriber filters to see these;
/// the structured surface makes them visible without log plumbing.
///
/// Lands in 0.3.37 in response to an operator hitting an unquoted
/// colon in a description (`First clause: second clause`) — PyYAML
/// raised `mapping values are not allowed here` and the loader
/// silently skipped the file. 25-minute debug session later, the
/// operator switched to a folded scalar. The lesson: silent skip is
/// the worst failure mode for a new authoring surface.
#[derive(Debug, Clone)]
pub struct ParseWarning {
    /// The file that failed to load.
    pub path: PathBuf,
    /// Human-readable description of why it failed.
    pub error: String,
}

/// Walk a directory for `*.md` files, loading each as a skill.
///
/// Files that fail to parse are skipped (one malformed skill in a
/// domain pack shouldn't take down the rest) but their errors are
/// **both** logged via `tracing::warn!` AND collected for the caller
/// to surface via [`ResolvedRegistry::parse_warnings`]. Operators
/// using stdio transport — where tracing output may not be visible —
/// can still see the warnings through the structured channel.
pub fn load_skills_from_dir(
    dir: &Path,
    provenance: SkillProvenance,
) -> Result<(Vec<Skill>, Vec<ParseWarning>), SkillError> {
    if !dir.is_dir() {
        return Ok((Vec::new(), Vec::new()));
    }

    let entries = fs::read_dir(dir).map_err(|e| SkillError::Io {
        path: dir.to_path_buf(),
        source: e,
    })?;

    let mut skills = Vec::new();
    let mut warnings = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    dir = %dir.display(),
                    error = %e,
                    "failed to read directory entry; skipping"
                );
                warnings.push(ParseWarning {
                    path: dir.to_path_buf(),
                    error: format!("failed to read directory entry: {e}"),
                });
                continue;
            }
        };
        let path = entry.path();
        // Only `.md` files. Subdirectories and other extensions are
        // ignored (no recursion — keeps the model simple).
        if path.extension().map(|e| e == "md").unwrap_or(false) {
            match load_skill_from_file(&path, provenance.clone()) {
                Ok(skill) => skills.push(skill),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to load skill; skipping"
                    );
                    warnings.push(ParseWarning {
                        path: path.clone(),
                        error: e.to_string(),
                    });
                }
            }
        }
    }
    Ok((skills, warnings))
}

// ─── Path resolution ──────────────────────────────────────────────

/// Resolve a skill path declaration against the manifest's parent
/// directory, applying the same conventions used by other manifest
/// fields:
///
/// - `./foo` or `foo` → relative to the manifest's parent dir
/// - `~/foo` → home-relative (POSIX `$HOME` expansion)
/// - `/foo` or `C:\foo` → absolute
///
/// Public so downstream binaries can resolve paths consistently if
/// they need to.
pub fn resolve_skill_path(raw: &str, manifest_dir: &Path) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
        // No HOME — fall through to manifest-relative.
    }
    manifest_dir.join(raw)
}

/// Project layer path for a manifest: `<manifest_stem>.skills/` next
/// to the manifest YAML.
///
/// For a manifest at `mcp-servers/legal_mcp.yaml`, the project layer
/// lives at `mcp-servers/legal_mcp.skills/`.
pub fn project_skills_dir(yaml_path: &Path) -> PathBuf {
    let stem = yaml_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "manifest".to_string());
    let parent = yaml_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{stem}.skills"))
}

// ─── Library-bundled framework defaults ───────────────────────────

/// Return the framework's own bundled skills.
///
/// The five SKILL.md files are embedded at compile time via the
/// [`bundled_skills_index`](crate::server::bundled_skills_index)
/// submodule. Downstream binaries call this through
/// [`Registry::merge_framework_defaults`] when they want the
/// framework defaults at the bottom of their three-layer stack.
pub fn library_bundled_skills() -> Vec<BundledSkill> {
    crate::server::bundled_skills_index::library_bundled_skills()
}

// ─── Authoring template ───────────────────────────────────────────

/// Render a starter SKILL.md body as a string.
///
/// The returned text is a complete, parse-valid SKILL.md file with
/// the supplied `name` and `description` filled into the frontmatter
/// and the rest of the optional extension fields commented out.
/// The body follows the anatomy documented in
/// `docs/guides/writing-effective-skills.md` — Overview, Quick
/// Reference table, a placeholder major-topic section, Common
/// Pitfalls, and a "When wrong" section — all with `<TODO>`-style
/// placeholders the operator fills in.
///
/// Use [`write_skill_template`] for the on-disk version.
pub fn render_skill_template(name: &str, description: &str) -> String {
    format!(
        "---\n\
         name: {name}\n\
         description: {description}\n\
         # Optional mcp-methods extension fields (uncomment as needed):\n\
         # applies_to:\n\
         #   mcp_methods: \">=0.3.35\"\n\
         # references_tools:\n\
         #   - {name}\n\
         # references_arguments:\n\
         #   - {name}.<arg_name>\n\
         # auto_inject_hint: true\n\
         ---\n\
         \n\
         # `{name}` methodology\n\
         \n\
         ## Overview\n\
         \n\
         <TODO: 2–3 sentences. What this skill enables, when to reach for it,\n\
         what comes before and after it in the typical workflow.>\n\
         \n\
         ## Quick Reference\n\
         \n\
         | Task | Approach |\n\
         |---|---|\n\
         | <TODO: common task A> | <TODO: one-line pattern> |\n\
         | <TODO: common task B> | <TODO: one-line pattern> |\n\
         \n\
         ## <TODO: Major topic>\n\
         \n\
         <TODO: concrete prose, code blocks, examples.>\n\
         \n\
         ## Common Pitfalls\n\
         \n\
         ❌ <TODO: specific anti-pattern, framed as a behaviour to avoid>\n\
         \n\
         ✅ <TODO: positive guidance, often a heuristic>\n\
         \n\
         ## When `{name}` is the wrong tool\n\
         \n\
         - **<TODO: scenario>** — use <other tool> because <reason>.\n"
    )
}

/// Resolve where a template write should land and write it.
///
/// `dest` interpretation:
/// - If `dest` is an existing directory, the file is written to
///   `dest/<name>.md`.
/// - If `dest` ends in `.md`, it is used verbatim and its parent
///   must already exist.
/// - Otherwise `dest` is treated as a directory that should be
///   created (and its parents created with `create_dir_all`) before
///   writing `dest/<name>.md`.
///
/// Existing files are never overwritten — if the destination already
/// exists, returns a `SkillError::Io` wrapping `AlreadyExists`. The
/// caller should delete first if they really want to replace.
pub fn write_skill_template(
    dest: &Path,
    name: &str,
    description: &str,
) -> Result<PathBuf, SkillError> {
    let path = resolve_template_dest(dest, name);

    if path.exists() {
        return Err(SkillError::Io {
            path: path.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "destination already exists; delete it before re-running",
            ),
        });
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent).map_err(|e| SkillError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
    }

    let body = render_skill_template(name, description);
    fs::write(&path, body).map_err(|e| SkillError::Io {
        path: path.clone(),
        source: e,
    })?;
    Ok(path)
}

fn resolve_template_dest(dest: &Path, name: &str) -> PathBuf {
    if dest.is_dir() {
        return dest.join(format!("{name}.md"));
    }
    if dest
        .extension()
        .map(|e| e.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
    {
        return dest.to_path_buf();
    }
    dest.join(format!("{name}.md"))
}

// ─── Registry builder ─────────────────────────────────────────────

/// Builder for a skills [`ResolvedRegistry`]. Downstream binaries
/// (`kglite-mcp-server`, etc.) construct one of these in their
/// boot path, layer in their bundled + operator-declared skills,
/// then call [`Registry::finalise`] to get the resolved set
/// ready for MCP `prompts/list` + `prompts/get` wiring.
///
/// See the module docs for the canonical usage pattern.
#[derive(Default)]
pub struct Registry {
    bundled: Vec<BundledSkill>,
    /// Sources from the manifest's `skills:` list, in declaration
    /// order. Each entry contributes a layer; later entries within
    /// the root layer have lower priority than earlier ones.
    root_dirs: Vec<(PathBuf, String)>, // (resolved_path, raw_decl_string)
    root_includes_bundled: bool,
    /// Project layer — auto-detected `<basename>.skills/` adjacent
    /// to the manifest YAML. Set via `auto_detect_project_layer`.
    project_dir: Option<PathBuf>,
    /// Optional consumer-supplied evaluator for domain predicates
    /// (`graph_has_node_type`, `graph_has_property`). Wired in via
    /// [`Registry::with_predicate_evaluator`]; framework-internal
    /// predicates (`tool_registered`, `extension_enabled`) are
    /// dispatched without consulting the evaluator.
    evaluator: Option<Arc<dyn SkillPredicateEvaluator>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("bundled", &self.bundled)
            .field("root_dirs", &self.root_dirs)
            .field("root_includes_bundled", &self.root_includes_bundled)
            .field("project_dir", &self.project_dir)
            .field(
                "evaluator",
                &self
                    .evaluator
                    .as_ref()
                    .map(|_| "<dyn SkillPredicateEvaluator>"),
            )
            .finish()
    }
}

impl Registry {
    /// Construct an empty registry. Chain in `add_bundled`,
    /// `merge_framework_defaults`, `layer_dirs`,
    /// `auto_detect_project_layer`, and optionally
    /// `with_predicate_evaluator`, then call `finalise()`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a domain-specific predicate evaluator for the
    /// `applies_when:` machinery. The evaluator only sees domain
    /// predicates (`graph_has_node_type`, `graph_has_property`);
    /// framework-internal ones (`tool_registered`,
    /// `extension_enabled`) are dispatched against the
    /// [`McpServer`](crate::server::McpServer)'s runtime state at
    /// [`serve_prompts`](crate::server::serve_prompts) time.
    ///
    /// Without an evaluator, skills using domain predicates resolve
    /// to inactive (predicate `Unknown` → skill suppressed). This
    /// is the safe default: a typo'd predicate or a missing
    /// evaluator must not silently activate the wrong-domain skill.
    pub fn with_predicate_evaluator(
        mut self,
        evaluator: impl SkillPredicateEvaluator + 'static,
    ) -> Self {
        self.evaluator = Some(Arc::new(evaluator));
        self
    }

    /// Add a compile-time bundled skill. Typically called by
    /// downstream binaries with their own `include_str!`'d skills,
    /// once per custom tool.
    ///
    /// Bundled skills sit at the bottom of the three-layer
    /// composition; later layers override them when names collide.
    /// Within the bundled set, the downstream binary's skills win
    /// over framework defaults (the downstream calls `add_bundled`
    /// before or after `merge_framework_defaults` — order doesn't
    /// matter; resolution dedupes by name with downstream-first
    /// priority).
    ///
    /// Malformed bundled skills are reported at `finalise()` time
    /// via [`SkillError::BundledSkillInvalid`]. The framework's
    /// own bundled-skill CI test should catch this for the library
    /// defaults; downstream binaries should write equivalent tests
    /// for their own bundled set.
    pub fn add_bundled(mut self, skill: BundledSkill) -> Self {
        self.bundled.push(skill);
        self
    }

    /// Add a batch of compile-time bundled skills.
    pub fn add_bundled_many(mut self, skills: impl IntoIterator<Item = BundledSkill>) -> Self {
        self.bundled.extend(skills);
        self
    }

    /// Merge in the framework's own bundled defaults (returned by
    /// [`library_bundled_skills`]). Idempotent — calling twice is
    /// harmless (later calls add duplicates which the finalise
    /// deduper drops, downstream-first).
    pub fn merge_framework_defaults(self) -> Self {
        let defaults = library_bundled_skills();
        self.add_bundled_many(defaults)
    }

    /// Layer in skill directories declared in the manifest's
    /// `skills:` field, walked in declaration order. Each path
    /// becomes a domain-pack-layer source; the bundled marker
    /// `true` is acknowledged but its skills are already in the
    /// bundled layer via `add_bundled`/`merge_framework_defaults`.
    ///
    /// Path resolution uses the same conventions as the rest of the
    /// manifest (`./foo` relative to YAML dir, `~/foo` home-relative,
    /// `/foo` absolute). Non-existent paths are reported as
    /// [`SkillError::PathNotFound`] at this call site so operators
    /// see typos immediately.
    pub fn layer_dirs(
        mut self,
        source: &SkillsSource,
        yaml_path: &Path,
    ) -> Result<Self, SkillError> {
        let manifest_dir = yaml_path.parent().unwrap_or_else(|| Path::new("."));

        match source {
            SkillsSource::Disabled => {
                // Skills disabled entirely — return the registry
                // unchanged. Downstream may still have called
                // add_bundled, but those won't be reachable without
                // a layer telling us skills are enabled.
                self.root_includes_bundled = false;
            }
            SkillsSource::Sources(sources) => {
                for src in sources {
                    match src {
                        SkillSource::Bundled => {
                            self.root_includes_bundled = true;
                        }
                        SkillSource::Path(raw) => {
                            let resolved = resolve_skill_path(raw, manifest_dir);
                            if !resolved.is_dir() {
                                return Err(SkillError::PathNotFound {
                                    raw: raw.clone(),
                                    resolved,
                                });
                            }
                            self.root_dirs.push((resolved, raw.clone()));
                        }
                    }
                }
            }
        }

        Ok(self)
    }

    /// Auto-detect the project layer at `<basename>.skills/`
    /// adjacent to the manifest YAML. Always called; the directory
    /// is optional — if it doesn't exist, the project layer is
    /// simply empty.
    pub fn auto_detect_project_layer(mut self, yaml_path: &Path) -> Self {
        let candidate = project_skills_dir(yaml_path);
        if candidate.is_dir() {
            self.project_dir = Some(candidate);
        }
        self
    }

    /// Resolve all three layers and return the final registry.
    ///
    /// Resolution order per skill name: project > root layer
    /// (in declaration order) > bundled. The first source that
    /// contributes a skill with the given name wins; later sources
    /// are ignored for that name (no merging, no inheritance —
    /// full-file replacement).
    ///
    /// At this point the framework:
    /// - Parses all skill files (frontmatter validation)
    /// - Logs collision-resolution info via `tracing::info!` per skill
    /// - Enforces per-skill hard size limits ([`HARD_SIZE_LIMIT_BYTES`])
    /// - Warns on per-skill soft size limit ([`SOFT_SIZE_LIMIT_BYTES`])
    /// - Warns on session total exceeding [`SESSION_TOTAL_LIMIT_BYTES`]
    pub fn finalise(self) -> Result<ResolvedRegistry, SkillError> {
        let Self {
            bundled,
            root_dirs,
            root_includes_bundled,
            project_dir,
            evaluator,
        } = self;

        // Parse bundled skills first. These are the lowest-priority
        // layer; they get overridden by anything declared above.
        let mut bundled_skills: Vec<Skill> = Vec::with_capacity(bundled.len());
        if root_includes_bundled {
            for b in &bundled {
                let path = PathBuf::from(format!("<bundled:{}>", b.name));
                let (frontmatter, body) =
                    parse_skill(b.body, &path).map_err(|e| SkillError::BundledSkillInvalid {
                        name: b.name,
                        message: e.to_string(),
                    })?;
                if frontmatter.name != b.name {
                    return Err(SkillError::BundledSkillInvalid {
                        name: b.name,
                        message: format!(
                            "frontmatter name {:?} does not match the bundled key {:?}",
                            frontmatter.name, b.name
                        ),
                    });
                }
                bundled_skills.push(Skill {
                    frontmatter,
                    body,
                    provenance: SkillProvenance::Bundled,
                });
            }
        }

        // Root layer: walk each declared path; first wins per name.
        // Accumulate parse warnings across all layers so the resolved
        // registry can surface them to downstream binaries.
        let mut parse_warnings: Vec<ParseWarning> = Vec::new();
        let mut root_skills_per_dir: Vec<Vec<Skill>> = Vec::with_capacity(root_dirs.len());
        for (resolved, _raw) in &root_dirs {
            let provenance = SkillProvenance::DomainPack(resolved.clone());
            let (skills, warnings) = load_skills_from_dir(resolved, provenance)?;
            parse_warnings.extend(warnings);
            root_skills_per_dir.push(skills);
        }

        // Project layer: auto-detected adjacent dir.
        let project_skills: Vec<Skill> = match &project_dir {
            Some(dir) => {
                let (skills, warnings) = load_skills_from_dir(dir, SkillProvenance::Project)?;
                parse_warnings.extend(warnings);
                skills
            }
            None => Vec::new(),
        };

        // Resolve per skill name. Priority:
        //   1. Project layer
        //   2. Root layer entries in declaration order
        //   3. Bundled (downstream entries first, then framework)
        //
        // The bundled list is already in downstream-first order
        // because downstream binaries call `add_bundled` before
        // `merge_framework_defaults` by convention.

        let mut resolved: HashMap<String, Skill> = HashMap::new();
        let mut collisions: HashMap<String, Vec<SkillProvenance>> = HashMap::new();

        // Lowest priority first: bundled, then root in reverse
        // declaration order, then project. Later inserts overwrite.
        // We track collisions for the boot log.
        for skill in &bundled_skills {
            let name = skill.name().to_string();
            collisions
                .entry(name.clone())
                .or_default()
                .push(skill.provenance.clone());
            resolved.insert(name, skill.clone());
        }
        for skills in root_skills_per_dir.iter().rev() {
            for skill in skills {
                let name = skill.name().to_string();
                collisions
                    .entry(name.clone())
                    .or_default()
                    .push(skill.provenance.clone());
                resolved.insert(name, skill.clone());
            }
        }
        for skill in &project_skills {
            let name = skill.name().to_string();
            collisions
                .entry(name.clone())
                .or_default()
                .push(skill.provenance.clone());
            resolved.insert(name, skill.clone());
        }

        // Log collision resolution for skills with more than one
        // candidate. Single-candidate skills don't need a log line.
        for (name, candidates) in &collisions {
            if candidates.len() > 1 {
                let winner = resolved
                    .get(name)
                    .map(|s| format_provenance(&s.provenance))
                    .unwrap_or_else(|| "<none>".to_string());
                let all_candidates: Vec<String> =
                    candidates.iter().map(format_provenance).collect();
                tracing::info!(
                    skill = %name,
                    candidates = ?all_candidates,
                    winner = %winner,
                    "skill resolved across multiple layers"
                );
            }
        }

        // Check session-total size limit.
        let total_bytes: usize = resolved.values().map(|s| s.body.len()).sum();
        if total_bytes > SESSION_TOTAL_LIMIT_BYTES {
            tracing::warn!(
                total_bytes,
                limit = SESSION_TOTAL_LIMIT_BYTES,
                skill_count = resolved.len(),
                "total resolved skill body size exceeds session limit; \
                 consider trimming or splitting skills"
            );
        }

        Ok(ResolvedRegistry {
            skills: resolved,
            evaluator,
            parse_warnings,
        })
    }
}

fn format_provenance(p: &SkillProvenance) -> String {
    match p {
        SkillProvenance::Project => "project".to_string(),
        SkillProvenance::DomainPack(path) => format!("pack:{}", path.display()),
        SkillProvenance::Bundled => "bundled".to_string(),
    }
}

// ─── ResolvedRegistry ─────────────────────────────────────────────

/// The post-resolution skill set. Consumed by `serve_prompts`
/// (Phase 1c) to wire `prompts/list` and `prompts/get` on the
/// MCP server.
#[derive(Default)]
pub struct ResolvedRegistry {
    skills: HashMap<String, Skill>,
    /// Optional domain-predicate evaluator carried from
    /// [`Registry::with_predicate_evaluator`]. `serve_prompts`
    /// consults this when evaluating `applies_when:` blocks; absent
    /// means domain predicates resolve to `Unknown` → skill
    /// inactive.
    pub(crate) evaluator: Option<Arc<dyn SkillPredicateEvaluator>>,
    /// Non-fatal per-file load failures (silent skips). Empty in the
    /// happy path; populated when a SKILL.md fails to parse and the
    /// rest of the directory is loaded around it. Downstream binaries
    /// render these in their boot summary so operators see what was
    /// silently dropped without having to enable tracing.
    parse_warnings: Vec<ParseWarning>,
}

impl std::fmt::Debug for ResolvedRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedRegistry")
            .field("skills", &self.skills)
            .field(
                "evaluator",
                &self
                    .evaluator
                    .as_ref()
                    .map(|_| "<dyn SkillPredicateEvaluator>"),
            )
            .finish()
    }
}

impl ResolvedRegistry {
    /// All resolved skill names, sorted alphabetically for stable
    /// output in `prompts/list`.
    pub fn skill_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.skills.keys().cloned().collect();
        names.sort();
        names
    }

    /// Look up a skill by name. Used by `prompts/get` to fetch the
    /// full body when the agent requests it.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// Iterate all resolved skills. Order is unspecified — use
    /// `skill_names()` first if a deterministic iteration is needed.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Skill)> {
        self.skills.iter()
    }

    /// Number of resolved skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Whether the registry contains any skills.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Per-file load failures that were silently skipped. Empty in
    /// the happy path. Each entry names the path and the error so
    /// downstream binaries can render them in their boot summary —
    /// the durable channel for visibility into "this file was
    /// silently dropped" failures that previously cost a 25-minute
    /// debug session per operator.
    pub fn parse_warnings(&self) -> &[ParseWarning] {
        &self.parse_warnings
    }

    /// Evaluate the `applies_when:` block on `skill` against this
    /// registry's evaluator plus the supplied runtime state. Returns
    /// the per-clause outcomes plus whether the skill should be
    /// considered active.
    ///
    /// `registered_tools` and `extensions` carry the runtime state
    /// the framework-internal predicates check against.
    /// `serve_prompts` calls this for every skill at boot;
    /// `skills-list` calls it (with placeholder empty state) for
    /// the operator-facing summary.
    ///
    /// A skill without an `applies_when:` block is always active.
    pub fn activation_for(
        &self,
        skill: &Skill,
        registered_tools: &std::collections::HashSet<String>,
        extensions: &serde_json::Map<String, serde_json::Value>,
    ) -> SkillActivation {
        let Some(applies_when) = skill.frontmatter.applies_when.as_ref() else {
            return SkillActivation {
                active: true,
                clauses: Vec::new(),
            };
        };
        let mut clauses = Vec::new();
        let mut all_satisfied = true;

        if let Some(types) = applies_when.graph_has_node_type.as_ref() {
            let clause = PredicateClause::GraphHasNodeType(types);
            let outcome = self.dispatch_clause(&clause, registered_tools, extensions);
            if outcome != PredicateOutcome::Satisfied {
                all_satisfied = false;
            }
            clauses.push((format!("graph_has_node_type: {types:?}"), outcome));
        }
        if let Some(prop) = applies_when.graph_has_property.as_ref() {
            let clause = PredicateClause::GraphHasProperty {
                node_type: &prop.node_type,
                prop_name: &prop.prop_name,
            };
            let outcome = self.dispatch_clause(&clause, registered_tools, extensions);
            if outcome != PredicateOutcome::Satisfied {
                all_satisfied = false;
            }
            clauses.push((
                format!("graph_has_property: {}.{}", prop.node_type, prop.prop_name),
                outcome,
            ));
        }
        if let Some(tool) = applies_when.tool_registered.as_ref() {
            let clause = PredicateClause::ToolRegistered(tool);
            let outcome = self.dispatch_clause(&clause, registered_tools, extensions);
            if outcome != PredicateOutcome::Satisfied {
                all_satisfied = false;
            }
            clauses.push((format!("tool_registered: {tool}"), outcome));
        }
        if let Some(key) = applies_when.extension_enabled.as_ref() {
            let clause = PredicateClause::ExtensionEnabled(key);
            let outcome = self.dispatch_clause(&clause, registered_tools, extensions);
            if outcome != PredicateOutcome::Satisfied {
                all_satisfied = false;
            }
            clauses.push((format!("extension_enabled: {key}"), outcome));
        }

        SkillActivation {
            active: all_satisfied,
            clauses,
        }
    }

    fn dispatch_clause(
        &self,
        clause: &PredicateClause<'_>,
        registered_tools: &std::collections::HashSet<String>,
        extensions: &serde_json::Map<String, serde_json::Value>,
    ) -> PredicateOutcome {
        // Framework-internal predicates are dispatched in-framework
        // regardless of the evaluator's preference. This keeps
        // tool_registered + extension_enabled working even when no
        // evaluator is registered.
        match clause {
            PredicateClause::ToolRegistered(name) => {
                return if registered_tools.contains(*name) {
                    PredicateOutcome::Satisfied
                } else {
                    PredicateOutcome::Unsatisfied
                };
            }
            PredicateClause::ExtensionEnabled(key) => {
                let truthy = extensions
                    .get(*key)
                    .map(|v| !v.is_null() && v != &serde_json::Value::Bool(false))
                    .unwrap_or(false);
                return if truthy {
                    PredicateOutcome::Satisfied
                } else {
                    PredicateOutcome::Unsatisfied
                };
            }
            _ => {}
        }

        // Domain predicates: defer to the evaluator. Unknown when no
        // evaluator is registered or the evaluator returns None.
        match self.evaluator.as_ref().and_then(|e| e.evaluate(clause)) {
            Some(true) => PredicateOutcome::Satisfied,
            Some(false) => PredicateOutcome::Unsatisfied,
            None => PredicateOutcome::Unknown,
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_skill(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(format!("{name}.md"));
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    fn minimal_skill(name: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: A test skill named {name}.\n---\n\n# {name}\n\nBody.\n"
        )
    }

    // ─── Frontmatter parsing ──────────────────────────────────────

    #[test]
    fn parse_frontmatter_basic() {
        let content = "---\nname: foo\ndescription: A foo skill.\n---\n\nBody here.\n";
        let path = PathBuf::from("test.md");
        let (fm, body) = parse_skill(content, &path).unwrap();
        assert_eq!(fm.name, "foo");
        assert_eq!(fm.description, "A foo skill.");
        assert_eq!(body, "\nBody here.\n");
        assert!(fm.auto_inject_hint, "auto_inject_hint defaults to true");
    }

    #[test]
    fn parse_frontmatter_missing_delimiters_rejected() {
        let content = "name: foo\ndescription: bar\n";
        let path = PathBuf::from("test.md");
        let err = parse_skill(content, &path).unwrap_err();
        assert!(matches!(err, SkillError::MissingFrontmatter { .. }));
    }

    #[test]
    fn parse_frontmatter_invalid_yaml_rejected() {
        let content = "---\nname: foo\n  bad: yaml: nesting\n---\nbody\n";
        let path = PathBuf::from("test.md");
        let err = parse_skill(content, &path).unwrap_err();
        assert!(matches!(err, SkillError::InvalidFrontmatter { .. }));
    }

    #[test]
    fn parse_frontmatter_missing_name_rejected() {
        let content = "---\ndescription: bar\n---\nbody\n";
        let path = PathBuf::from("test.md");
        let err = parse_skill(content, &path).unwrap_err();
        assert!(matches!(
            err,
            SkillError::MissingRequiredField { field: "name", .. }
        ));
    }

    #[test]
    fn parse_frontmatter_missing_description_rejected() {
        let content = "---\nname: foo\n---\nbody\n";
        let path = PathBuf::from("test.md");
        let err = parse_skill(content, &path).unwrap_err();
        assert!(matches!(
            err,
            SkillError::MissingRequiredField {
                field: "description",
                ..
            }
        ));
    }

    #[test]
    fn parse_frontmatter_all_optional_fields() {
        let content = "---\n\
name: foo\n\
description: Full surface.\n\
references_tools: [grep, list_source]\n\
references_arguments: [grep.pattern]\n\
references_properties: [Function.module]\n\
auto_inject_hint: false\n\
applies_to:\n  mcp_methods: \">=0.3.35\"\n\
---\n\
Body.\n";
        let path = PathBuf::from("test.md");
        let (fm, _) = parse_skill(content, &path).unwrap();
        assert_eq!(fm.references_tools, vec!["grep", "list_source"]);
        assert_eq!(fm.references_arguments, vec!["grep.pattern"]);
        assert_eq!(fm.references_properties, vec!["Function.module"]);
        assert!(!fm.auto_inject_hint);
        assert_eq!(
            fm.applies_to.unwrap().get("mcp_methods"),
            Some(&">=0.3.35".to_string())
        );
    }

    // ─── Loading from files + dirs ────────────────────────────────

    #[test]
    fn load_skill_from_file_basic() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(dir.path(), "foo", &minimal_skill("foo"));
        let skill = load_skill_from_file(&path, SkillProvenance::Project).unwrap();
        assert_eq!(skill.name(), "foo");
        assert_eq!(skill.provenance, SkillProvenance::Project);
    }

    #[test]
    fn load_skill_too_large_rejected() {
        let dir = tempfile::tempdir().unwrap();
        // Build a body just over the hard limit.
        let big_body = "x".repeat(HARD_SIZE_LIMIT_BYTES + 100);
        let content = format!("---\nname: big\ndescription: too big.\n---\n{big_body}");
        let path = write_skill(dir.path(), "big", &content);
        let err = load_skill_from_file(&path, SkillProvenance::Project).unwrap_err();
        assert!(matches!(err, SkillError::SkillTooLarge { .. }));
    }

    #[test]
    fn load_skills_from_dir_walks_markdown_only() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "a", &minimal_skill("a"));
        write_skill(dir.path(), "b", &minimal_skill("b"));
        // Non-markdown file — ignored.
        fs::write(dir.path().join("readme.txt"), "not a skill").unwrap();
        // Subdirectory — ignored.
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        write_skill(&sub, "c", &minimal_skill("c"));

        let (skills, warnings) =
            load_skills_from_dir(dir.path(), SkillProvenance::Project).unwrap();
        assert_eq!(skills.len(), 2);
        assert!(warnings.is_empty());
        let mut names: Vec<&str> = skills.iter().map(|s| s.name()).collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn load_skills_from_dir_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let nonexistent = dir.path().join("does-not-exist");
        let (skills, warnings) =
            load_skills_from_dir(&nonexistent, SkillProvenance::Project).unwrap();
        assert!(skills.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn load_skills_from_dir_surfaces_yaml_parse_failure_as_warning() {
        // The exact scenario the operator hit: unquoted colon in
        // description value triggers PyYAML's "mapping values are
        // not allowed here" — except ours uses serde_yaml so the
        // failure mode is `InvalidFrontmatter`. Either way, the
        // file is skipped, the rest of the dir loads, and the
        // warning surfaces structurally rather than just via
        // tracing::warn!.
        let dir = tempfile::tempdir().unwrap();
        // Valid skill.
        write_skill(dir.path(), "good", &minimal_skill("good"));
        // Broken skill: unquoted colon inside description.
        write_skill(
            dir.path(),
            "broken",
            "---\nname: broken\ndescription: First clause: second clause\n---\n# body\n",
        );

        let (skills, warnings) =
            load_skills_from_dir(dir.path(), SkillProvenance::Project).unwrap();
        assert_eq!(skills.len(), 1, "the good skill should still load");
        assert_eq!(skills[0].name(), "good");
        assert_eq!(
            warnings.len(),
            1,
            "the broken file should surface as a warning"
        );
        assert!(warnings[0].path.ends_with("broken.md"));
        assert!(!warnings[0].error.is_empty());
    }

    #[test]
    fn resolved_registry_parse_warnings_propagated_from_project_layer() {
        // End-to-end through `Registry::finalise`: a broken file in
        // the project layer shows up on `ResolvedRegistry::parse_warnings`.
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("test_mcp.yaml");
        fs::write(&yaml, "name: t\nskills: true\n").unwrap();
        let skills_dir = dir.path().join("test_mcp.skills");
        fs::create_dir(&skills_dir).unwrap();
        // Valid skill.
        write_skill(&skills_dir, "good", &minimal_skill("good"));
        // Broken skill: missing closing `---`.
        write_skill(
            &skills_dir,
            "broken",
            "---\nname: broken\ndescription: bad\nstill in frontmatter\n",
        );

        let registry = Registry::new()
            .auto_detect_project_layer(&yaml)
            .finalise()
            .unwrap();

        assert_eq!(registry.len(), 1, "good skill resolved");
        assert!(registry.get("good").is_some());
        let warnings = registry.parse_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].path.ends_with("broken.md"));
    }

    // ─── Path resolution ──────────────────────────────────────────

    #[test]
    fn resolve_skill_path_relative() {
        let manifest_dir = Path::new("/a/b");
        assert_eq!(
            resolve_skill_path("./skills", manifest_dir),
            PathBuf::from("/a/b/./skills")
        );
        assert_eq!(
            resolve_skill_path("skills", manifest_dir),
            PathBuf::from("/a/b/skills")
        );
    }

    #[test]
    fn resolve_skill_path_absolute() {
        let manifest_dir = Path::new("/a/b");
        assert_eq!(
            resolve_skill_path("/abs/skills", manifest_dir),
            PathBuf::from("/abs/skills")
        );
    }

    #[test]
    fn resolve_skill_path_home_relative() {
        let manifest_dir = Path::new("/a/b");
        // Set HOME explicitly for the test.
        // SAFETY: tests run single-threaded for env mutation; this is
        // a known stylistic exception in Rust's 1.83+ unsafe-env API.
        unsafe {
            std::env::set_var("HOME", "/home/test");
        }
        assert_eq!(
            resolve_skill_path("~/skills", manifest_dir),
            PathBuf::from("/home/test/skills")
        );
    }

    #[test]
    fn project_skills_dir_naming() {
        assert_eq!(
            project_skills_dir(Path::new("/a/b/legal_mcp.yaml")),
            PathBuf::from("/a/b/legal_mcp.skills")
        );
        assert_eq!(
            project_skills_dir(Path::new("workspace_mcp.yaml")),
            PathBuf::from("workspace_mcp.skills")
        );
    }

    // ─── Registry builder ─────────────────────────────────────────

    #[test]
    fn registry_disabled_resolves_empty() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("test_mcp.yaml");
        fs::write(&yaml, "name: x\n").unwrap();

        let registry = Registry::new()
            .layer_dirs(&SkillsSource::Disabled, &yaml)
            .unwrap()
            .auto_detect_project_layer(&yaml)
            .finalise()
            .unwrap();
        assert!(registry.is_empty());
    }

    #[test]
    fn registry_add_bundled_only_visible_when_opted_in() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("test_mcp.yaml");
        fs::write(&yaml, "name: x\n").unwrap();

        let bundled = BundledSkill {
            name: "foo",
            // Static body for testing — needs to be 'static, which is
            // why BundledSkill uses &'static str. For the test we
            // leak. Production code uses include_str!.
            body: Box::leak(minimal_skill("foo").into_boxed_str()),
        };

        // Disabled → bundled is NOT visible, even if added.
        let registry = Registry::new()
            .add_bundled(bundled.clone())
            .layer_dirs(&SkillsSource::Disabled, &yaml)
            .unwrap()
            .finalise()
            .unwrap();
        assert!(registry.is_empty(), "disabled must short-circuit bundled");

        // skills: [true] → bundled IS visible.
        let registry = Registry::new()
            .add_bundled(bundled)
            .layer_dirs(&SkillsSource::Sources(vec![SkillSource::Bundled]), &yaml)
            .unwrap()
            .finalise()
            .unwrap();
        assert_eq!(registry.len(), 1);
        assert!(registry.get("foo").is_some());
        assert_eq!(
            registry.get("foo").unwrap().provenance,
            SkillProvenance::Bundled
        );
    }

    #[test]
    fn registry_three_layer_resolution_project_wins_over_bundled() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("test_mcp.yaml");
        fs::write(&yaml, "name: x\n").unwrap();

        // Bundled `foo`:
        let bundled = BundledSkill {
            name: "foo",
            body: "---\nname: foo\ndescription: from bundled.\n---\nbundled body\n",
        };

        // Project layer `foo`:
        let project_dir = dir.path().join("test_mcp.skills");
        fs::create_dir(&project_dir).unwrap();
        fs::write(
            project_dir.join("foo.md"),
            "---\nname: foo\ndescription: from project.\n---\nproject body\n",
        )
        .unwrap();

        let registry = Registry::new()
            .add_bundled(bundled)
            .layer_dirs(&SkillsSource::Sources(vec![SkillSource::Bundled]), &yaml)
            .unwrap()
            .auto_detect_project_layer(&yaml)
            .finalise()
            .unwrap();

        assert_eq!(registry.len(), 1);
        let skill = registry.get("foo").unwrap();
        assert_eq!(skill.description(), "from project.");
        assert_eq!(skill.provenance, SkillProvenance::Project);
    }

    #[test]
    fn registry_root_layer_first_declaration_wins() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("test_mcp.yaml");
        fs::write(&yaml, "name: x\n").unwrap();

        // First domain pack: foo (from "primary").
        let primary = dir.path().join("primary");
        fs::create_dir(&primary).unwrap();
        fs::write(
            primary.join("foo.md"),
            "---\nname: foo\ndescription: from primary.\n---\nprimary body\n",
        )
        .unwrap();

        // Second domain pack: foo (from "secondary") — should LOSE.
        let secondary = dir.path().join("secondary");
        fs::create_dir(&secondary).unwrap();
        fs::write(
            secondary.join("foo.md"),
            "---\nname: foo\ndescription: from secondary.\n---\nsecondary body\n",
        )
        .unwrap();

        let registry = Registry::new()
            .layer_dirs(
                &SkillsSource::Sources(vec![
                    SkillSource::Path("./primary".into()),
                    SkillSource::Path("./secondary".into()),
                ]),
                &yaml,
            )
            .unwrap()
            .finalise()
            .unwrap();

        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get("foo").unwrap().description(), "from primary.");
    }

    #[test]
    fn registry_root_layer_nonexistent_path_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("test_mcp.yaml");
        fs::write(&yaml, "name: x\n").unwrap();

        let err = Registry::new()
            .layer_dirs(
                &SkillsSource::Sources(vec![SkillSource::Path("./does-not-exist".into())]),
                &yaml,
            )
            .unwrap_err();
        assert!(matches!(err, SkillError::PathNotFound { .. }));
    }

    #[test]
    fn registry_empty_list_opts_in_without_root_sources() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("test_mcp.yaml");
        fs::write(&yaml, "name: x\n").unwrap();

        // No bundled, no paths — but project layer DOES exist.
        let project_dir = dir.path().join("test_mcp.skills");
        fs::create_dir(&project_dir).unwrap();
        fs::write(project_dir.join("only.md"), minimal_skill("only")).unwrap();

        let registry = Registry::new()
            .layer_dirs(&SkillsSource::Sources(vec![]), &yaml)
            .unwrap()
            .auto_detect_project_layer(&yaml)
            .finalise()
            .unwrap();

        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry.get("only").unwrap().provenance,
            SkillProvenance::Project
        );
    }

    #[test]
    fn registry_bundled_name_mismatch_rejected_at_finalise() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("test_mcp.yaml");
        fs::write(&yaml, "name: x\n").unwrap();

        // BundledSkill says name="foo" but the frontmatter says name="bar".
        let bundled = BundledSkill {
            name: "foo",
            body: Box::leak(
                "---\nname: bar\ndescription: mismatch.\n---\nbody\n"
                    .to_string()
                    .into_boxed_str(),
            ),
        };

        let err = Registry::new()
            .add_bundled(bundled)
            .layer_dirs(&SkillsSource::Sources(vec![SkillSource::Bundled]), &yaml)
            .unwrap()
            .finalise()
            .unwrap_err();
        assert!(matches!(err, SkillError::BundledSkillInvalid { .. }));
    }

    #[test]
    fn registry_library_bundled_skills_returns_vec() {
        // Five framework defaults ship from Phase 1d onward. The
        // exhaustive shape + uniqueness checks live in
        // `bundled_skills_index::tests`; here we just confirm the
        // re-export points downstream callers at the populated Vec.
        let skills = library_bundled_skills();
        assert!(
            !skills.is_empty(),
            "library_bundled_skills should return framework defaults from Phase 1d onward"
        );
    }

    #[test]
    fn registry_skill_names_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("test_mcp.yaml");
        fs::write(&yaml, "name: x\n").unwrap();

        let pack = dir.path().join("pack");
        fs::create_dir(&pack).unwrap();
        fs::write(pack.join("zeta.md"), minimal_skill("zeta")).unwrap();
        fs::write(pack.join("alpha.md"), minimal_skill("alpha")).unwrap();
        fs::write(pack.join("mu.md"), minimal_skill("mu")).unwrap();

        let registry = Registry::new()
            .layer_dirs(
                &SkillsSource::Sources(vec![SkillSource::Path("./pack".into())]),
                &yaml,
            )
            .unwrap()
            .finalise()
            .unwrap();

        assert_eq!(registry.skill_names(), vec!["alpha", "mu", "zeta"]);
    }

    // ─── Authoring template ───────────────────────────────────────

    #[test]
    fn render_skill_template_is_parse_valid() {
        // Round-trip: a freshly-rendered template must parse cleanly
        // through `parse_skill` so the operator's starting point is
        // never broken.
        let body = render_skill_template("custom_method", "A test description for the skill.");
        let (fm, _body) =
            parse_skill(&body, &PathBuf::from("test.md")).expect("rendered template must parse");
        assert_eq!(fm.name, "custom_method");
        assert_eq!(fm.description, "A test description for the skill.");
    }

    #[test]
    fn render_skill_template_substitutes_name_into_body_headings() {
        let body = render_skill_template("my_skill", "desc");
        assert!(body.contains("# `my_skill` methodology"));
        assert!(body.contains("## When `my_skill` is the wrong tool"));
    }

    #[test]
    fn write_skill_template_writes_into_directory() {
        let dir = tempfile::tempdir().unwrap();
        let dest = write_skill_template(dir.path(), "alpha", "First skill.").unwrap();
        assert_eq!(dest, dir.path().join("alpha.md"));
        let content = fs::read_to_string(&dest).unwrap();
        assert!(content.contains("name: alpha"));
    }

    #[test]
    fn write_skill_template_writes_to_explicit_md_path() {
        let dir = tempfile::tempdir().unwrap();
        let explicit = dir.path().join("renamed.md");
        let dest = write_skill_template(&explicit, "alpha", "First skill.").unwrap();
        assert_eq!(dest, explicit);
        assert!(explicit.is_file());
    }

    #[test]
    fn write_skill_template_creates_missing_parents() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        let dest = write_skill_template(&nested, "alpha", "First skill.").unwrap();
        assert_eq!(dest, nested.join("alpha.md"));
        assert!(dest.is_file());
    }

    #[test]
    fn write_skill_template_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alpha.md");
        fs::write(&path, "existing").unwrap();
        let err = write_skill_template(dir.path(), "alpha", "Replace me?").unwrap_err();
        assert!(matches!(err, SkillError::Io { .. }));
        // Original content preserved.
        assert_eq!(fs::read_to_string(&path).unwrap(), "existing");
    }

    #[test]
    fn write_skill_template_round_trips_through_registry() {
        // End-to-end: write a template, build a registry that
        // auto-detects it as a project skill, confirm it resolves.
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("test_mcp.yaml");
        fs::write(&yaml, "name: t\nskills: true\n").unwrap();
        let skills_dir = dir.path().join("test_mcp.skills");
        write_skill_template(&skills_dir, "custom_method", "Project-layer skill body.").unwrap();

        let registry = Registry::new()
            .auto_detect_project_layer(&yaml)
            .finalise()
            .unwrap();
        let skill = registry
            .get("custom_method")
            .expect("template should resolve");
        assert_eq!(skill.description(), "Project-layer skill body.");
    }

    // ─── applies_when predicates (Phase 3) ────────────────────────

    fn skill_with_applies_when(applies_when_yaml: &str) -> Skill {
        let body = format!(
            "---\nname: gated\ndescription: A gated skill.\n\
             applies_when:\n{applies_when_yaml}\n---\n\nBody.\n"
        );
        let (frontmatter, body) = parse_skill(&body, &PathBuf::from("gated.md")).unwrap();
        Skill {
            frontmatter,
            body,
            provenance: SkillProvenance::Bundled,
        }
    }

    #[test]
    fn applies_when_parses_map_shape() {
        let skill = skill_with_applies_when(
            "  graph_has_node_type: [Function, Class]\n\
             \x20 tool_registered: cypher_query\n\
             \x20 extension_enabled: csv_http_server\n\
             \x20 graph_has_property:\n\
             \x20   node_type: Function\n\
             \x20   prop_name: module",
        );
        let applies = skill.frontmatter.applies_when.unwrap();
        assert_eq!(
            applies.graph_has_node_type.as_deref(),
            Some(["Function".to_string(), "Class".to_string()].as_slice())
        );
        assert_eq!(applies.tool_registered.as_deref(), Some("cypher_query"));
        assert_eq!(
            applies.extension_enabled.as_deref(),
            Some("csv_http_server")
        );
        assert_eq!(
            applies.graph_has_property,
            Some(GraphPropertyCheck {
                node_type: "Function".to_string(),
                prop_name: "module".to_string(),
            })
        );
    }

    #[test]
    fn applies_when_absent_means_always_active() {
        let body = "---\nname: ungated\ndescription: An ungated skill.\n---\n\nBody.\n";
        let (frontmatter, body) = parse_skill(body, &PathBuf::from("ungated.md")).unwrap();
        let skill = Skill {
            frontmatter,
            body,
            provenance: SkillProvenance::Bundled,
        };
        let registry = ResolvedRegistry::default();
        let activation = registry.activation_for(
            &skill,
            &std::collections::HashSet::new(),
            &serde_json::Map::new(),
        );
        assert!(activation.active);
        assert!(activation.clauses.is_empty());
    }

    #[test]
    fn tool_registered_predicate_dispatches_in_framework() {
        let skill = skill_with_applies_when("  tool_registered: cypher_query");
        let registry = ResolvedRegistry::default();
        let mut tools = std::collections::HashSet::new();

        // Tool absent → unsatisfied.
        let inactive = registry.activation_for(&skill, &tools, &serde_json::Map::new());
        assert!(!inactive.active);
        assert_eq!(inactive.clauses[0].1, PredicateOutcome::Unsatisfied);

        // Tool present → satisfied.
        tools.insert("cypher_query".to_string());
        let active = registry.activation_for(&skill, &tools, &serde_json::Map::new());
        assert!(active.active);
        assert_eq!(active.clauses[0].1, PredicateOutcome::Satisfied);
    }

    #[test]
    fn extension_enabled_predicate_dispatches_in_framework() {
        let skill = skill_with_applies_when("  extension_enabled: csv_http_server");
        let registry = ResolvedRegistry::default();
        let tools = std::collections::HashSet::new();
        let mut extensions = serde_json::Map::new();

        // Key absent → unsatisfied.
        assert!(!registry.activation_for(&skill, &tools, &extensions).active);

        // Key with `false` → unsatisfied.
        extensions.insert("csv_http_server".to_string(), serde_json::json!(false));
        assert!(!registry.activation_for(&skill, &tools, &extensions).active);

        // Key with `null` → unsatisfied.
        extensions.insert("csv_http_server".to_string(), serde_json::Value::Null);
        assert!(!registry.activation_for(&skill, &tools, &extensions).active);

        // Key with truthy value → satisfied.
        extensions.insert("csv_http_server".to_string(), serde_json::json!(true));
        assert!(registry.activation_for(&skill, &tools, &extensions).active);

        // Key with a map → satisfied (truthy).
        extensions.insert(
            "csv_http_server".to_string(),
            serde_json::json!({"enabled": true}),
        );
        assert!(registry.activation_for(&skill, &tools, &extensions).active);
    }

    struct StubEvaluator {
        has_function: bool,
    }
    impl SkillPredicateEvaluator for StubEvaluator {
        fn evaluate(&self, clause: &PredicateClause<'_>) -> Option<bool> {
            match clause {
                PredicateClause::GraphHasNodeType(types) => {
                    Some(types.iter().any(|t| t == "Function") && self.has_function)
                }
                _ => None,
            }
        }
    }

    #[test]
    fn graph_predicate_dispatches_via_evaluator() {
        let skill = skill_with_applies_when("  graph_has_node_type: [Function, Class]");

        // With evaluator that says yes → active.
        let registry = Registry::new()
            .with_predicate_evaluator(StubEvaluator { has_function: true })
            .finalise()
            .unwrap();
        let active = registry.activation_for(
            &skill,
            &std::collections::HashSet::new(),
            &serde_json::Map::new(),
        );
        assert!(active.active);
        assert_eq!(active.clauses[0].1, PredicateOutcome::Satisfied);

        // With evaluator that says no → inactive.
        let registry = Registry::new()
            .with_predicate_evaluator(StubEvaluator {
                has_function: false,
            })
            .finalise()
            .unwrap();
        let inactive = registry.activation_for(
            &skill,
            &std::collections::HashSet::new(),
            &serde_json::Map::new(),
        );
        assert!(!inactive.active);
        assert_eq!(inactive.clauses[0].1, PredicateOutcome::Unsatisfied);
    }

    #[test]
    fn graph_predicate_unknown_without_evaluator_means_inactive() {
        let skill = skill_with_applies_when("  graph_has_node_type: [Function]");
        let registry = ResolvedRegistry::default();
        let activation = registry.activation_for(
            &skill,
            &std::collections::HashSet::new(),
            &serde_json::Map::new(),
        );
        assert!(!activation.active);
        assert_eq!(activation.clauses[0].1, PredicateOutcome::Unknown);
    }

    #[test]
    fn multiple_predicates_all_must_be_satisfied() {
        let skill = skill_with_applies_when(
            "  graph_has_node_type: [Function]\n\
             \x20 tool_registered: cypher_query",
        );
        let registry = Registry::new()
            .with_predicate_evaluator(StubEvaluator { has_function: true })
            .finalise()
            .unwrap();
        let mut tools = std::collections::HashSet::new();
        let extensions = serde_json::Map::new();

        // Graph satisfied but tool absent → inactive.
        assert!(!registry.activation_for(&skill, &tools, &extensions).active);

        // Both satisfied → active.
        tools.insert("cypher_query".to_string());
        assert!(registry.activation_for(&skill, &tools, &extensions).active);
    }
}
