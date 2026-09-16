//! Skills-aware MCP — runtime types, frontmatter parsing, layered
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
//! # Layer composition
//!
//! 1. **Project layer (top priority).** Auto-detected from
//!    `<manifest_basename>.skills/` adjacent to the YAML. Files there
//!    override every other layer per skill name. This is the operator's
//!    per-deployment tweak zone.
//! 2. **Root layer.** Each entry in the manifest's `skills:`
//!    list, walked in declaration order. First-match-per-name wins.
//!    This is where operator-curated domain skill-packs sit
//!    (`kglite-skills-legal/`, etc.).
//! 3. **Inline layer.** Mapping entries in the manifest's `skills:`
//!    list — a whole skill written out in the YAML. One fixed layer
//!    however the entries are interleaved with paths: position in the
//!    list never changes precedence.
//! 4. **Owned layer.** Runtime-supplied [`OwnedSkill`] bodies handed to
//!    [`Registry::add_layer`] by the host binary — e.g. skills carried
//!    inside the artefact the server serves. Overrides bundled, is
//!    overridden by inline entries, operator-declared dirs and the
//!    project layer.
//! 5. **Bundled layer (bottom).** Compile-time defaults shipped with
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

use serde::{Deserialize, Serialize};

use super::manifest::{load as load_manifest, InlineSkill, SkillSource, SkillsSource};

// ─── Public types ─────────────────────────────────────────────────

/// A compile-time bundled skill, embedded into the binary via
/// `include_str!`. Downstream binaries (e.g. `kglite-mcp-server`)
/// construct these for their custom tools; the framework constructs
/// them for its own (`grep`, `read_source`, etc.).
///
/// Bundled skills sit at the bottom of the layer composition —
/// owned, root-layer and project entries override them when names
/// collide.
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

/// A runtime-supplied skill with an **owned** body, handed to
/// [`Registry::add_layer`]. Unlike [`BundledSkill`] — whose body must
/// be `&'static str` and therefore compile-time — an `OwnedSkill` can
/// be assembled at boot from whatever the host binary reads: a graph
/// file, a database row, a downloaded pack.
///
/// Owned skills sit directly above the bundled layer and below the
/// operator's declared `skills:` directories, so an operator can
/// always override one with a file on disk. Malformed entries are
/// [`ParseWarning`]s, never errors — a bad row in an artefact must not
/// take the server down the way a malformed compile-time skill does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedSkill {
    /// Skill name. Must match the `name` field in `body`'s
    /// frontmatter; a mismatch is reported as a [`ParseWarning`] and
    /// the entry is dropped.
    pub name: String,
    /// The full SKILL.md content — frontmatter + body, the same shape
    /// as [`BundledSkill::body`].
    pub body: String,
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

    /// Tools this skill teaches or references in prose. When
    /// `auto_inject_hint` is set, the skill's routing + methodology is
    /// injected into the description of every tool listed here, in
    /// addition to its name-match tool — the only way to express a
    /// cross-tool skill. Also used for staleness detection (Phase 1f).
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

    /// When `true` (the default), the framework injects the skill's
    /// `description` (under `## When to use`) and `body` (under
    /// `## Methodology`) into the live tool-description channel — for
    /// the tool whose name matches the skill plus every tool in
    /// `references_tools`. Set `false` to keep the skill on the
    /// `prompts/*` plane only (no tool-description injection).
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

    /// `delivery:` — which tier the framework injects this skill on.
    ///
    /// [`Delivery::Lazy`] (the default) puts the description in every
    /// target tool's description plus one routing line pointing at the
    /// `skill(name)` tool; the body travels only when the agent asks
    /// for it. [`Delivery::Eager`] embeds the whole body in every
    /// target tool's description, as every skill did before 0.4.11.
    ///
    /// Any other value is a parse error — a warning for file, owned
    /// and inline layers, fatal for a bundled skill.
    #[serde(default)]
    pub delivery: Delivery,
}

/// Which tier [`serve_prompts`](crate::server::serve_prompts) injects
/// a skill on.
///
/// The choice is a budget decision, not a capability one: both tiers
/// put the skill's `description` — the TRIGGER/SKIP routing — into
/// every target tool's description, and both leave the full body
/// reachable. They differ in when the body is paid for.
///
/// Reach for [`Eager`](Delivery::Eager) only when the body shapes the
/// *first* call's parameters (a query language the agent has to write
/// correctly before it has any result to learn from). Everything else
/// is [`Lazy`](Delivery::Lazy): the agent reads the routing line, calls
/// `skill(name)` when the routing matches, and the tool list stops
/// scaling with skills × referenced tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    /// Body embedded in every target tool's description at
    /// `tools/list` time.
    Eager,
    /// Body withheld; the target tool descriptions carry the routing
    /// plus a line naming the `skill(name)` tool that fetches it.
    /// The default for any skill that does not say otherwise.
    #[default]
    Lazy,
}

impl std::fmt::Display for Delivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Delivery::Eager => "eager",
            Delivery::Lazy => "lazy",
        })
    }
}

fn default_auto_inject_hint() -> bool {
    true
}

/// The parsed shape of a SKILL.md's `applies_when:` block. Each field
/// is one predicate; `None` means "this predicate is not applied".
/// All populated fields are ANDed.
///
/// Adding a new predicate requires extending this struct and the
/// matching arm in `ResolvedRegistry::dispatch_clause`, reached via
/// [`ResolvedRegistry::activation_for`]. The bounded-set
/// design is intentional — operators get type-checked semantics
/// instead of an open-ended DSL.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
/// resolution log, the `skills-list` / `skills-show` CLI columns, and
/// the `provenance` string on the pyo3 `Skill` wrapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillProvenance {
    /// Auto-detected from `<basename>.skills/` adjacent to the
    /// manifest YAML — top-priority operator overrides.
    Project,
    /// Loaded from an operator-declared path in the manifest's
    /// `skills:` list (a domain skill-pack or shared library).
    DomainPack(PathBuf),
    /// Supplied at runtime as an owned body via
    /// [`Registry::add_layer`]. The `String` is the caller's label for
    /// the layer — free-form, domain-specific (a host serving a graph
    /// that carries its own skills passes `"graph"`), and used only
    /// for reporting: the collision log, the CLI provenance column and
    /// the warning paths of malformed entries. It does not affect
    /// resolution order.
    Owned(String),
    /// Declared as a mapping entry in the manifest's `skills:` list
    /// (an [`InlineSkill`]) — the
    /// body lives in the YAML, not in a file. No payload: every inline
    /// skill comes from the one manifest the server was booted with,
    /// so there is nothing to distinguish them by.
    Inline,
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

    /// Which tier the auto-inject pass delivers this skill on.
    pub fn delivery(&self) -> Delivery {
        self.frontmatter.delivery
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
    /// Manifest YAML at `path` failed to load while resolving skills
    /// from a manifest (e.g. via [`Registry::from_manifest`]).
    Manifest { path: PathBuf, message: String },
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
            SkillError::Manifest { path, message } => write!(
                f,
                "manifest load failed at {}: {message}",
                path.display()
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
///
/// **Counted at resolve time, every skill, both delivery tiers.**
/// [`Delivery::Lazy`] changes *when* a body reaches the agent, not
/// whether it can: one session may call `skill(name)` for every lazy
/// skill in the registry, so the worst case this limit bounds is the
/// same as it was when every body shipped in the tool list. Charging
/// lazy bodies only at `skill()` time would make the number depend on
/// agent behaviour and would let a registry that cannot fit a session
/// resolve without a word. See
/// [`ResolvedRegistry::total_body_bytes`].
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
/// framework defaults at the bottom of their layer stack.
pub fn library_bundled_skills() -> Vec<BundledSkill> {
    crate::server::bundled_skills_index::library_bundled_skills()
}

// ─── Inline skills ────────────────────────────────────────────────

/// Render a manifest-declared [`InlineSkill`] back into SKILL.md
/// text — YAML frontmatter, `---`, then the body.
///
/// The round trip through text is deliberate: it puts inline skills
/// through [`parse_skill`] and the size limits on exactly the same
/// path as a file on disk, so there is one definition of what a valid
/// skill is rather than two that can drift.
///
/// This is not [`render_skill_template`]'s job — that renders a
/// starter file full of `<TODO>` placeholders for a human to fill in,
/// with the extension keys commented out. Here every key the operator
/// actually set has to come out live.
///
/// Frontmatter values go through `serde_yaml` rather than string
/// interpolation, so a description with a colon, a body-looking
/// `---`, or any other YAML metacharacter survives the trip.
fn render_inline_skill(inline: &InlineSkill) -> String {
    let mut front = serde_yaml::Mapping::new();
    let mut put = |key: &str, value: serde_yaml::Value| {
        front.insert(serde_yaml::Value::String(key.to_string()), value);
    };
    put("name", serde_yaml::Value::String(inline.name.clone()));
    put(
        "description",
        serde_yaml::Value::String(inline.description.clone()),
    );
    if !inline.references_tools.is_empty() {
        put(
            "references_tools",
            serde_yaml::Value::Sequence(
                inline
                    .references_tools
                    .iter()
                    .map(|t| serde_yaml::Value::String(t.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(delivery) = &inline.delivery {
        put("delivery", serde_yaml::Value::String(delivery.clone()));
    }
    if let Some(applies_when) = &inline.applies_when {
        // `AppliesWhen` serialises back to the same mapping it was
        // parsed from; an unset predicate is skipped, not emitted as
        // null, so the re-parse sees what the operator wrote.
        match serde_yaml::to_value(applies_when) {
            Ok(value) => put("applies_when", value),
            Err(e) => {
                // Unreachable for the bounded predicate set, but a
                // serialisation failure must not silently drop the
                // gate and let a suppressed skill surface.
                tracing::warn!(
                    skill = %inline.name,
                    error = %e,
                    "inline skill `applies_when` failed to serialise; emitting it \
                     unparseable so the entry is rejected rather than ungated"
                );
                put(
                    "applies_when",
                    serde_yaml::Value::String(format!("<unserialisable: {e}>")),
                );
            }
        }
    }

    // Same rule as the `applies_when` arm: on the unreachable failure,
    // emit frontmatter `parse_skill` rejects (no `description`) so the
    // entry becomes a warning instead of a skill with a lost gate.
    let frontmatter = serde_yaml::to_string(&serde_yaml::Value::Mapping(front))
        .unwrap_or_else(|e| format!("name: <unserialisable: {e}>\n"));
    let body = &inline.body;
    let separator = if body.ends_with('\n') { "" } else { "\n" };
    format!("---\n{frontmatter}---\n{body}{separator}")
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
    /// Whether the manifest's `skills:` list carries the `true`
    /// marker. That marker is the operator's switch for every layer
    /// the *binary* supplies rather than the operator: compile-time
    /// [`BundledSkill`]s and runtime [`OwnedSkill`] layers alike. With
    /// `skills: false` — or a list of paths that never says `true` —
    /// neither surfaces, however many the host binary added.
    binary_layers_enabled: bool,
    /// Owned layers from [`Registry::add_layer`], in call order.
    /// Later calls have higher priority than earlier ones, and the
    /// whole set sits above bundled and below `inline_skills`.
    owned_layers: Vec<(Vec<OwnedSkill>, SkillProvenance)>,
    /// Inline skills declared as mapping entries in the manifest's
    /// `skills:` list, collected by `layer_dirs` in declaration order.
    /// They form one layer between `owned_layers` and `root_dirs`
    /// whatever their position in the list; within the layer, a later
    /// entry of a duplicated name wins.
    inline_skills: Vec<InlineSkill>,
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
            .field("binary_layers_enabled", &self.binary_layers_enabled)
            .field("owned_layers", &self.owned_layers)
            .field("inline_skills", &self.inline_skills)
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

    /// One-shot resolution of a [`ResolvedRegistry`] from a manifest
    /// YAML — loads the manifest, merges framework defaults (when
    /// `include_bundled` is true), auto-detects the project layer at
    /// `<basename>.skills/`, layers in operator-declared `skills:`
    /// paths, and finalises in a single call.
    ///
    /// This is the canonical shape consumed by pyo3 wrappers
    /// (`mcp-methods-py::SkillRegistry.from_manifest` and downstream
    /// equivalents like kglite's). Owning it here keeps the
    /// orchestration single-sourced so layering tweaks don't need to
    /// be replicated in every wrapper.
    ///
    /// For bespoke layering — e.g. supplying a custom predicate
    /// evaluator via [`Registry::with_predicate_evaluator`], or
    /// `include_str!`'d downstream bundled skills via
    /// [`Registry::add_bundled`] — drive the builder directly
    /// instead of calling this.
    ///
    /// Pass `include_bundled=false` to skip framework defaults; useful
    /// for tests or downstream binaries supplying their own bundled
    /// layer.
    pub fn from_manifest(
        manifest_path: &Path,
        include_bundled: bool,
    ) -> Result<ResolvedRegistry, SkillError> {
        let manifest = load_manifest(manifest_path).map_err(|e| SkillError::Manifest {
            path: manifest_path.to_path_buf(),
            message: e.message,
        })?;
        let mut builder = Registry::new();
        if include_bundled {
            builder = builder.merge_framework_defaults();
        }
        builder = builder.auto_detect_project_layer(manifest_path);
        builder = builder.layer_dirs(&manifest.skills, manifest_path)?;
        builder.finalise()
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
    /// Bundled skills sit at the bottom of the layer
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

    /// Add a layer of runtime-supplied [`OwnedSkill`] bodies, labelled
    /// by `provenance`.
    ///
    /// The layer sits **above** the bundled layer and **below** the
    /// operator's declared `skills:` directories and the project
    /// layer, so a host binary can ship skills alongside the artefact
    /// it serves while leaving the operator the final word.
    ///
    /// Call it as many times as there are sources; **later calls
    /// override earlier ones** for the same skill name, the same way
    /// the project layer overrides a domain pack. Within one call the
    /// later entry of a duplicated name wins.
    ///
    /// Owned skills are only reachable when the manifest's `skills:`
    /// list carries the `true` marker — the same switch that gates the
    /// bundled layer. With `skills: false` nothing here surfaces.
    ///
    /// Unlike bundled skills, a malformed body — bad frontmatter, a
    /// `name` that disagrees with [`OwnedSkill::name`], or a body over
    /// [`HARD_SIZE_LIMIT_BYTES`] — is a [`ParseWarning`] on the
    /// resolved registry and the entry is dropped; the rest of the
    /// layer still loads. Bodies assembled at runtime are data, not
    /// code, and a single bad row must not deny service.
    ///
    /// `provenance` is expected to be [`SkillProvenance::Owned`] with
    /// the caller's label for this layer (`"graph"`, `"tenant-pack"`,
    /// …); that is the variant this layer exists for. Any other
    /// variant is accepted and attached to the resolved skills
    /// verbatim — it only changes how the skills are *reported* (the
    /// collision log, the CLI provenance column), never where the
    /// layer sits in the resolution order. The parameter is the whole
    /// provenance rather than a bare label so that a caller
    /// re-materialising skills it had previously read from disk can
    /// keep the original attribution.
    pub fn add_layer(
        mut self,
        skills: impl IntoIterator<Item = OwnedSkill>,
        provenance: SkillProvenance,
    ) -> Self {
        self.owned_layers
            .push((skills.into_iter().collect(), provenance));
        self
    }

    /// Layer in the sources declared in the manifest's `skills:`
    /// field, walked in declaration order. Each path becomes a
    /// domain-pack-layer source; the `true` marker adds no source of
    /// its own — it switches on the layers the binary already holds,
    /// from `add_bundled`/`merge_framework_defaults` and from
    /// [`Registry::add_layer`]. Each mapping entry is an inline skill,
    /// collected into the single inline layer that sits between the
    /// owned layers and the declared directories.
    ///
    /// Inline entries are the operator's own declaration, like the
    /// paths beside them, so they surface whether or not the list also
    /// carries `true`; only `skills: false` (which declares no entries
    /// at all) hides them.
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
                // add_bundled or add_layer, but those won't be
                // reachable without a layer telling us skills are
                // enabled.
                self.binary_layers_enabled = false;
            }
            SkillsSource::Sources(sources) => {
                for src in sources {
                    match src {
                        SkillSource::Bundled => {
                            self.binary_layers_enabled = true;
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
                        SkillSource::Inline(inline) => {
                            self.inline_skills.push(inline.clone());
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

    /// Resolve every layer and return the final registry.
    ///
    /// Resolution order per skill name: project > root layer
    /// (in declaration order) > inline manifest entries > owned layers
    /// (later [`add_layer`](Registry::add_layer) calls first) >
    /// bundled. The
    /// first source that contributes a skill with the given name
    /// wins; later sources are ignored for that name (no merging, no
    /// inheritance — full-file replacement).
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
            binary_layers_enabled,
            owned_layers,
            inline_skills,
            project_dir,
            evaluator,
        } = self;

        // Parse bundled skills first. These are the lowest-priority
        // layer; they get overridden by anything declared above.
        let mut bundled_skills: Vec<Skill> = Vec::with_capacity(bundled.len());
        if binary_layers_enabled {
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

        // Accumulate parse warnings across all layers so the resolved
        // registry can surface them to downstream binaries.
        let mut parse_warnings: Vec<ParseWarning> = Vec::new();

        // Owned layers: runtime-supplied bodies, gated by the same
        // `skills: [true]` marker as the bundled layer. Every failure
        // here is a warning, not an error — the bodies are data the
        // host assembled at boot, so one bad entry drops itself and
        // leaves the rest of the layer standing.
        let mut owned_skills_per_layer: Vec<Vec<Skill>> = Vec::with_capacity(owned_layers.len());
        if binary_layers_enabled {
            for (entries, provenance) in &owned_layers {
                let label = owned_layer_label(provenance);
                let mut layer: Vec<Skill> = Vec::with_capacity(entries.len());
                for entry in entries {
                    let path = PathBuf::from(format!("<owned:{label}:{}>", entry.name));
                    if entry.body.len() > HARD_SIZE_LIMIT_BYTES {
                        let error = SkillError::SkillTooLarge {
                            path: path.clone(),
                            bytes: entry.body.len(),
                            limit: HARD_SIZE_LIMIT_BYTES,
                        };
                        tracing::warn!(
                            path = %path.display(),
                            error = %error,
                            "owned skill exceeds the hard size limit; skipping"
                        );
                        parse_warnings.push(ParseWarning {
                            path,
                            error: error.to_string(),
                        });
                        continue;
                    }
                    if entry.body.len() > SOFT_SIZE_LIMIT_BYTES {
                        tracing::warn!(
                            path = %path.display(),
                            bytes = entry.body.len(),
                            soft_limit = SOFT_SIZE_LIMIT_BYTES,
                            "owned skill exceeds the soft size limit; consider splitting"
                        );
                    }
                    let (frontmatter, body) = match parse_skill(&entry.body, &path) {
                        Ok(parsed) => parsed,
                        Err(e) => {
                            tracing::warn!(
                                path = %path.display(),
                                error = %e,
                                "failed to parse owned skill; skipping"
                            );
                            parse_warnings.push(ParseWarning {
                                path,
                                error: e.to_string(),
                            });
                            continue;
                        }
                    };
                    if frontmatter.name != entry.name {
                        let error = format!(
                            "frontmatter name {:?} does not match the owned key {:?}",
                            frontmatter.name, entry.name
                        );
                        tracing::warn!(
                            path = %path.display(),
                            error = %error,
                            "owned skill name mismatch; skipping"
                        );
                        parse_warnings.push(ParseWarning { path, error });
                        continue;
                    }
                    layer.push(Skill {
                        frontmatter,
                        body,
                        provenance: provenance.clone(),
                    });
                }
                owned_skills_per_layer.push(layer);
            }
        }

        // Inline layer: skills written out in the manifest's `skills:`
        // list. Rendered back to SKILL.md text and parsed on the same
        // path as a file, so the same validation and size limits
        // apply. Not gated by `binary_layers_enabled`: an inline entry
        // is the operator's own declaration, exactly like a path
        // beside it, so it does not wait on the `- true` marker that
        // switches on the layers the *binary* supplies.
        let mut inline_skill_layer: Vec<Skill> = Vec::with_capacity(inline_skills.len());
        for inline in &inline_skills {
            let path = PathBuf::from(format!("<inline:{}>", inline.name));
            let rendered = render_inline_skill(inline);
            if rendered.len() > HARD_SIZE_LIMIT_BYTES {
                let error = SkillError::SkillTooLarge {
                    path: path.clone(),
                    bytes: rendered.len(),
                    limit: HARD_SIZE_LIMIT_BYTES,
                };
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "inline skill exceeds the hard size limit; skipping"
                );
                parse_warnings.push(ParseWarning {
                    path,
                    error: error.to_string(),
                });
                continue;
            }
            if rendered.len() > SOFT_SIZE_LIMIT_BYTES {
                tracing::warn!(
                    path = %path.display(),
                    bytes = rendered.len(),
                    soft_limit = SOFT_SIZE_LIMIT_BYTES,
                    "inline skill exceeds the soft size limit; consider splitting"
                );
            }
            match parse_skill(&rendered, &path) {
                Ok((frontmatter, body)) => inline_skill_layer.push(Skill {
                    frontmatter,
                    body,
                    provenance: SkillProvenance::Inline,
                }),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to parse inline skill; skipping"
                    );
                    parse_warnings.push(ParseWarning {
                        path,
                        error: e.to_string(),
                    });
                }
            }
        }

        // Root layer: walk each declared path; first wins per name.
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
        //   3. Inline entries from the manifest's `skills:` list
        //   4. Owned layers (later `add_layer` calls win)
        //   5. Bundled (downstream entries first, then framework)
        //
        // The bundled list is already in downstream-first order
        // because downstream binaries call `add_bundled` before
        // `merge_framework_defaults` by convention.

        let mut resolved: HashMap<String, Skill> = HashMap::new();
        let mut collisions: HashMap<String, Vec<SkillProvenance>> = HashMap::new();

        // Lowest priority first: bundled, then owned layers in call
        // order, then the inline layer, then root in reverse
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
        for skills in &owned_skills_per_layer {
            for skill in skills {
                let name = skill.name().to_string();
                collisions
                    .entry(name.clone())
                    .or_default()
                    .push(skill.provenance.clone());
                resolved.insert(name, skill.clone());
            }
        }
        for skill in &inline_skill_layer {
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

        // Check session-total size limit. Every resolved skill counts,
        // whatever its delivery tier — see the constant's doc comment.
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
        SkillProvenance::Owned(label) => format!("owned:{label}"),
        SkillProvenance::Inline => "inline".to_string(),
        SkillProvenance::Bundled => "bundled".to_string(),
    }
}

/// The label an owned layer's synthetic warning paths carry
/// (`<owned:LABEL:NAME>`). For the expected
/// [`SkillProvenance::Owned`] that is the caller's label verbatim;
/// any other variant falls back to its collision-log rendering so the
/// path still points at a recognisable source.
fn owned_layer_label(p: &SkillProvenance) -> String {
    match p {
        SkillProvenance::Owned(label) => label.clone(),
        other => format_provenance(other),
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

    /// Summed body size of every resolved skill, in bytes — the
    /// number checked against [`SESSION_TOTAL_LIMIT_BYTES`] at
    /// [`Registry::finalise`] time.
    ///
    /// Delivery tier does not enter into it: a [`Delivery::Lazy`]
    /// body is one `skill(name)` call away, so it is part of what a
    /// single session can pull.
    pub fn total_body_bytes(&self) -> usize {
        self.skills.values().map(|s| s.body.len()).sum()
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
    fn unknown_applies_when_predicates_are_rejected_before_activation() {
        for applies_when in [
            "  graph_has_node_typo: [Function]",
            "  graph_has_node_type: [Function]\n  graph_has_node_typo: [Function]",
            "  graph_has_property:\n    node_type: Function\n    prop_name: module\n    property_typo: ignored",
        ] {
            let content = format!(
                "---\nname: typo_gate\ndescription: Must fail closed.\napplies_when:\n{applies_when}\n---\nBody.\n"
            );
            let error = parse_skill(&content, Path::new("typo_gate.md")).unwrap_err();
            let message = error.to_string();
            assert!(
                matches!(error, SkillError::InvalidFrontmatter { .. }),
                "{message}"
            );
            assert!(message.contains("unknown field"), "{message}");
            assert!(message.contains("typo"), "{message}");
        }
    }

    #[test]
    fn unknown_predicate_file_is_skipped_with_a_visible_diagnostic() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "good", &minimal_skill("good"));
        write_skill(
            dir.path(),
            "typo_gate",
            "---\nname: typo_gate\ndescription: Must fail closed.\napplies_when:\n  graph_has_node_typo: [Function]\n---\nBody.\n",
        );

        let (skills, warnings) =
            load_skills_from_dir(dir.path(), SkillProvenance::Project).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name(), "good");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].path.ends_with("typo_gate.md"));
        assert!(warnings[0].error.contains("unknown field"));
        assert!(warnings[0].error.contains("graph_has_node_typo"));
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
    fn hard_limit_counts_the_complete_file_and_accepts_exactly_16_kib() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = "---\nname: boundary\ndescription: Boundary skill.\n---\n";
        let content = format!(
            "{prefix}{}",
            "x".repeat(HARD_SIZE_LIMIT_BYTES - prefix.len())
        );
        assert_eq!(content.len(), 16_384);
        let path = write_skill(dir.path(), "boundary", &content);

        let skill = load_skill_from_file(&path, SkillProvenance::Project)
            .expect("a complete 16 KiB file is within the inclusive limit");
        assert_eq!(skill.name(), "boundary");

        fs::write(&path, format!("{content}x")).unwrap();
        let error = load_skill_from_file(&path, SkillProvenance::Project)
            .expect_err("one byte above the complete-file limit must fail");
        assert!(matches!(
            error,
            SkillError::SkillTooLarge {
                bytes: 16_385,
                limit: HARD_SIZE_LIMIT_BYTES,
                ..
            }
        ));
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
    fn registry_layer_resolution_project_wins_over_bundled() {
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

    // ─── Delivery tier ────────────────────────────────────────────

    #[test]
    fn delivery_defaults_to_lazy_when_absent() {
        // Mutation: flip `#[default]` on `Delivery` to `Eager`.
        let content = "---\nname: foo\ndescription: A foo skill.\n---\n\nBody.\n";
        let (frontmatter, _) = parse_skill(content, Path::new("t.md")).unwrap();
        assert_eq!(frontmatter.delivery, Delivery::Lazy);
    }

    #[test]
    fn delivery_parses_both_tiers_lowercase() {
        for (yaml, expected) in [("eager", Delivery::Eager), ("lazy", Delivery::Lazy)] {
            let content =
                format!("---\nname: foo\ndescription: d.\ndelivery: {yaml}\n---\n\nBody.\n");
            let (frontmatter, _) = parse_skill(&content, Path::new("t.md")).unwrap();
            assert_eq!(frontmatter.delivery, expected);
        }
    }

    #[test]
    fn delivery_rejects_any_other_value() {
        // Mutation: make `delivery` an `Option<String>` again — the
        // parse succeeds and the bogus tier survives unchecked.
        let content = "---\nname: foo\ndescription: d.\ndelivery: bogus\n---\n\nBody.\n";
        let err = parse_skill(content, Path::new("t.md")).unwrap_err();
        let message = err.to_string();
        assert!(
            matches!(err, SkillError::InvalidFrontmatter { .. }),
            "expected a frontmatter error, got {message}"
        );
        assert!(
            message.contains("bogus") && message.contains("eager"),
            "the error should name the bad value and the valid set: {message}"
        );
    }

    #[test]
    fn bad_delivery_on_disk_is_a_parse_warning_not_a_failure() {
        // Mutation: propagate the parse error with `?` in
        // `load_skills_from_dir` — the good skill disappears too.
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("test_mcp.skills");
        fs::create_dir(&skills_dir).unwrap();
        write_skill(&skills_dir, "good", &minimal_skill("good"));
        write_skill(
            &skills_dir,
            "bad",
            "---\nname: bad\ndescription: d.\ndelivery: bogus\n---\n\nBody.\n",
        );
        let yaml = yaml_in(dir.path());

        let registry = Registry::new()
            .auto_detect_project_layer(&yaml)
            .finalise()
            .unwrap();

        assert_eq!(registry.skill_names(), vec!["good".to_string()]);
        let warnings = registry.parse_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].error.contains("bogus"),
            "warning should name the rejected tier: {}",
            warnings[0].error
        );
    }

    #[test]
    fn bad_delivery_on_a_bundled_skill_is_fatal() {
        // Mutation: downgrade the bundled parse arm in `finalise` to a
        // warning — the registry resolves and the server boots with a
        // skill nobody validated.
        let dir = tempfile::tempdir().unwrap();
        let yaml = yaml_in(dir.path());
        let err = Registry::new()
            .add_bundled(BundledSkill {
                name: "foo",
                body: "---\nname: foo\ndescription: d.\ndelivery: bogus\n---\nbody\n",
            })
            .layer_dirs(&SkillsSource::Sources(vec![SkillSource::Bundled]), &yaml)
            .unwrap()
            .finalise()
            .unwrap_err();
        assert!(
            matches!(err, SkillError::BundledSkillInvalid { name: "foo", .. }),
            "expected BundledSkillInvalid, got {err}"
        );
    }

    #[test]
    fn total_body_bytes_counts_lazy_bodies_too() {
        // The session-size rule: a lazy body is one `skill()` call
        // away, so it is charged at resolve time exactly like an eager
        // one. Mutation: filter `total_body_bytes` to eager skills —
        // the lazy body stops counting and the assertion fails.
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("test_mcp.skills");
        fs::create_dir(&skills_dir).unwrap();
        write_skill(
            &skills_dir,
            "lazy_one",
            "---\nname: lazy_one\ndescription: d.\ndelivery: lazy\n---\nLAZYBODY\n",
        );
        write_skill(
            &skills_dir,
            "eager_one",
            "---\nname: eager_one\ndescription: d.\ndelivery: eager\n---\nEAGERBODY\n",
        );
        let yaml = yaml_in(dir.path());
        let registry = Registry::new()
            .auto_detect_project_layer(&yaml)
            .finalise()
            .unwrap();

        let expected: usize = registry.iter().map(|(_, s)| s.body.len()).sum();
        assert_eq!(registry.total_body_bytes(), expected);
        assert_eq!(
            registry.total_body_bytes(),
            registry.get("lazy_one").unwrap().body.len()
                + registry.get("eager_one").unwrap().body.len()
        );
    }

    // ─── Owned layer (`add_layer`) ────────────────────────────────

    /// A well-formed owned entry whose frontmatter matches `name`.
    fn owned(name: &str, description: &str) -> OwnedSkill {
        OwnedSkill {
            name: name.to_string(),
            body: format!(
                "---\nname: {name}\ndescription: {description}\n---\n{description} body\n"
            ),
        }
    }

    fn yaml_in(dir: &Path) -> PathBuf {
        let yaml = dir.join("test_mcp.yaml");
        fs::write(&yaml, "name: x\n").unwrap();
        yaml
    }

    #[test]
    fn owned_layer_overrides_bundled() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = yaml_in(dir.path());

        let registry = Registry::new()
            .add_bundled(BundledSkill {
                name: "foo",
                body: "---\nname: foo\ndescription: from bundled.\n---\nbundled body\n",
            })
            .add_layer(
                [owned("foo", "from owned.")],
                SkillProvenance::Owned("graph".to_string()),
            )
            .layer_dirs(&SkillsSource::Sources(vec![SkillSource::Bundled]), &yaml)
            .unwrap()
            .finalise()
            .unwrap();

        assert_eq!(registry.len(), 1);
        let skill = registry.get("foo").unwrap();
        assert_eq!(skill.description(), "from owned.");
        assert_eq!(
            skill.provenance,
            SkillProvenance::Owned("graph".to_string())
        );
    }

    #[test]
    fn owned_layer_loses_to_declared_root_dir() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = yaml_in(dir.path());

        let pack = dir.path().join("pack");
        fs::create_dir(&pack).unwrap();
        fs::write(
            pack.join("foo.md"),
            "---\nname: foo\ndescription: from pack.\n---\npack body\n",
        )
        .unwrap();

        let registry = Registry::new()
            .add_layer(
                [owned("foo", "from owned.")],
                SkillProvenance::Owned("graph".to_string()),
            )
            .layer_dirs(
                &SkillsSource::Sources(vec![
                    SkillSource::Bundled,
                    SkillSource::Path("./pack".into()),
                ]),
                &yaml,
            )
            .unwrap()
            .finalise()
            .unwrap();

        assert_eq!(registry.len(), 1);
        let skill = registry.get("foo").unwrap();
        assert_eq!(skill.description(), "from pack.");
        assert!(matches!(skill.provenance, SkillProvenance::DomainPack(_)));
    }

    #[test]
    fn owned_layer_later_call_overrides_earlier() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = yaml_in(dir.path());

        let registry = Registry::new()
            .add_layer(
                [owned("foo", "from first.")],
                SkillProvenance::Owned("first".to_string()),
            )
            .add_layer(
                [owned("foo", "from second.")],
                SkillProvenance::Owned("second".to_string()),
            )
            .layer_dirs(&SkillsSource::Sources(vec![SkillSource::Bundled]), &yaml)
            .unwrap()
            .finalise()
            .unwrap();

        assert_eq!(registry.len(), 1);
        let skill = registry.get("foo").unwrap();
        assert_eq!(skill.description(), "from second.");
        assert_eq!(
            skill.provenance,
            SkillProvenance::Owned("second".to_string())
        );
    }

    #[test]
    fn owned_layer_malformed_entry_warns_and_rest_loads() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = yaml_in(dir.path());

        let registry = Registry::new()
            .add_layer(
                [
                    OwnedSkill {
                        name: "broken".to_string(),
                        // No frontmatter delimiters at all.
                        body: "just a body, no frontmatter\n".to_string(),
                    },
                    owned("intact", "from owned."),
                ],
                SkillProvenance::Owned("graph".to_string()),
            )
            .layer_dirs(&SkillsSource::Sources(vec![SkillSource::Bundled]), &yaml)
            .unwrap()
            .finalise()
            .unwrap();

        assert!(registry.get("broken").is_none());
        assert_eq!(registry.get("intact").unwrap().description(), "from owned.");
        let warnings = registry.parse_warnings();
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0].path,
            PathBuf::from("<owned:graph:broken>"),
            "the warning must name the layer label and the entry"
        );
    }

    #[test]
    fn owned_layer_name_mismatch_warns() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = yaml_in(dir.path());

        let registry = Registry::new()
            .add_layer(
                [OwnedSkill {
                    name: "declared".to_string(),
                    body: "---\nname: actual\ndescription: mismatched.\n---\nbody\n".to_string(),
                }],
                SkillProvenance::Owned("graph".to_string()),
            )
            .layer_dirs(&SkillsSource::Sources(vec![SkillSource::Bundled]), &yaml)
            .unwrap()
            .finalise()
            .unwrap();

        assert!(registry.is_empty(), "neither key may resolve");
        assert!(registry.get("actual").is_none());
        let warnings = registry.parse_warnings();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].path, PathBuf::from("<owned:graph:declared>"));
        assert!(
            warnings[0].error.contains("does not match"),
            "warning should explain the mismatch: {}",
            warnings[0].error
        );
    }

    #[test]
    fn owned_layer_oversized_entry_warns() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = yaml_in(dir.path());

        let huge = format!(
            "---\nname: huge\ndescription: too big.\n---\n{}\n",
            "x".repeat(HARD_SIZE_LIMIT_BYTES)
        );
        let registry = Registry::new()
            .add_layer(
                [OwnedSkill {
                    name: "huge".to_string(),
                    body: huge,
                }],
                SkillProvenance::Owned("graph".to_string()),
            )
            .layer_dirs(&SkillsSource::Sources(vec![SkillSource::Bundled]), &yaml)
            .unwrap()
            .finalise()
            .unwrap();

        assert!(registry.is_empty());
        assert_eq!(registry.parse_warnings().len(), 1);
        assert!(registry.parse_warnings()[0].error.contains("hard limit"));
    }

    #[test]
    fn owned_layer_hidden_when_skills_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = yaml_in(dir.path());

        let registry = Registry::new()
            .add_layer(
                [owned("foo", "from owned.")],
                SkillProvenance::Owned("graph".to_string()),
            )
            .layer_dirs(&SkillsSource::Disabled, &yaml)
            .unwrap()
            .finalise()
            .unwrap();

        assert!(
            registry.is_empty(),
            "skills: false must hide owned entries the way it hides bundled"
        );
        assert!(
            registry.parse_warnings().is_empty(),
            "a disabled layer is not parsed at all"
        );
    }

    #[test]
    fn owned_provenance_renders_with_its_label() {
        // The collision log, the `skills-list` provenance column and
        // the pyo3 `Skill.provenance` string all render provenance
        // through a match; this is the framework-side rendering.
        assert_eq!(
            format_provenance(&SkillProvenance::Owned("graph".to_string())),
            "owned:graph"
        );
        assert_eq!(
            owned_layer_label(&SkillProvenance::Owned("graph".to_string())),
            "graph"
        );
        // A non-owned provenance passed to `add_layer` still labels
        // its warning paths recognisably.
        assert_eq!(owned_layer_label(&SkillProvenance::Bundled), "bundled");
    }

    #[test]
    fn owned_layer_accepts_a_non_owned_provenance_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = yaml_in(dir.path());

        let registry = Registry::new()
            .add_bundled(BundledSkill {
                name: "foo",
                body: "---\nname: foo\ndescription: from bundled.\n---\nbundled body\n",
            })
            .add_layer([owned("foo", "from owned.")], SkillProvenance::Project)
            .layer_dirs(&SkillsSource::Sources(vec![SkillSource::Bundled]), &yaml)
            .unwrap()
            .finalise()
            .unwrap();

        let skill = registry.get("foo").unwrap();
        assert_eq!(skill.description(), "from owned.");
        assert_eq!(
            skill.provenance,
            SkillProvenance::Project,
            "the label is reported verbatim; it does not move the layer"
        );
    }

    // ─── Inline layer (`skills:` mapping entries) ─────────────────

    /// A minimal well-formed inline entry.
    fn inline(name: &str, description: &str) -> InlineSkill {
        InlineSkill {
            name: name.to_string(),
            description: description.to_string(),
            body: format!("{description} body\n"),
            references_tools: Vec::new(),
            delivery: None,
            applies_when: None,
        }
    }

    #[test]
    fn inline_layer_overrides_owned() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = yaml_in(dir.path());

        let registry = Registry::new()
            .add_layer(
                [owned("foo", "from owned.")],
                SkillProvenance::Owned("graph".to_string()),
            )
            .layer_dirs(
                &SkillsSource::Sources(vec![
                    SkillSource::Bundled,
                    SkillSource::Inline(inline("foo", "from inline.")),
                ]),
                &yaml,
            )
            .unwrap()
            .finalise()
            .unwrap();

        assert_eq!(registry.len(), 1);
        let skill = registry.get("foo").unwrap();
        assert_eq!(skill.description(), "from inline.");
        assert_eq!(skill.provenance, SkillProvenance::Inline);
    }

    #[test]
    fn inline_layer_loses_to_declared_root_dir() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = yaml_in(dir.path());

        let pack = dir.path().join("pack");
        fs::create_dir(&pack).unwrap();
        fs::write(
            pack.join("foo.md"),
            "---\nname: foo\ndescription: from pack.\n---\npack body\n",
        )
        .unwrap();

        let registry = Registry::new()
            .layer_dirs(
                &SkillsSource::Sources(vec![
                    SkillSource::Inline(inline("foo", "from inline.")),
                    SkillSource::Path("./pack".into()),
                ]),
                &yaml,
            )
            .unwrap()
            .finalise()
            .unwrap();

        assert_eq!(registry.len(), 1);
        let skill = registry.get("foo").unwrap();
        assert_eq!(skill.description(), "from pack.");
        assert!(matches!(skill.provenance, SkillProvenance::DomainPack(_)));
    }

    #[test]
    fn inline_layer_precedence_ignores_list_position() {
        // The inline layer is fixed between owned and the declared
        // dirs. Writing the mapping *after* the path must not promote
        // it above the path, and writing it before must not demote it
        // below the owned layer. Both orders resolve identically.
        let dir = tempfile::tempdir().unwrap();
        let yaml = yaml_in(dir.path());

        let pack = dir.path().join("pack");
        fs::create_dir(&pack).unwrap();
        fs::write(
            pack.join("dir_wins.md"),
            "---\nname: dir_wins\ndescription: from pack.\n---\npack body\n",
        )
        .unwrap();

        let path_entry = SkillSource::Path("./pack".into());
        let dir_contested = SkillSource::Inline(inline("dir_wins", "from inline."));
        let owned_contested = SkillSource::Inline(inline("inline_wins", "from inline."));

        for (label, sources) in [
            (
                "inline first",
                vec![
                    SkillSource::Bundled,
                    dir_contested.clone(),
                    owned_contested.clone(),
                    path_entry.clone(),
                ],
            ),
            (
                "inline last",
                vec![
                    SkillSource::Bundled,
                    path_entry.clone(),
                    dir_contested.clone(),
                    owned_contested.clone(),
                ],
            ),
        ] {
            let registry = Registry::new()
                .add_layer(
                    [owned("inline_wins", "from owned.")],
                    SkillProvenance::Owned("graph".to_string()),
                )
                .layer_dirs(&SkillsSource::Sources(sources), &yaml)
                .unwrap()
                .finalise()
                .unwrap();

            assert_eq!(
                registry.get("dir_wins").unwrap().description(),
                "from pack.",
                "a declared dir outranks inline regardless of order ({label})"
            );
            assert_eq!(
                registry.get("inline_wins").unwrap().description(),
                "from inline.",
                "inline outranks owned regardless of order ({label})"
            );
        }
    }

    #[test]
    fn inline_layer_surfaces_without_the_bundled_marker() {
        // Inline entries are the operator's own declaration, like the
        // paths beside them — they must not wait on the `- true`
        // marker that switches on the binary-supplied layers.
        let dir = tempfile::tempdir().unwrap();
        let yaml = yaml_in(dir.path());

        let registry = Registry::new()
            .add_bundled(BundledSkill {
                name: "framework",
                body: "---\nname: framework\ndescription: from bundled.\n---\nbundled body\n",
            })
            .add_layer(
                [owned("carried", "from owned.")],
                SkillProvenance::Owned("graph".to_string()),
            )
            .layer_dirs(
                &SkillsSource::Sources(vec![SkillSource::Inline(inline("foo", "from inline."))]),
                &yaml,
            )
            .unwrap()
            .finalise()
            .unwrap();

        assert_eq!(
            registry.skill_names(),
            vec!["foo".to_string()],
            "the inline entry surfaces; the binary-supplied layers stay gated"
        );
        assert_eq!(registry.get("foo").unwrap().description(), "from inline.");
    }

    #[test]
    fn inline_entry_carries_its_optional_frontmatter_keys() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = yaml_in(dir.path());

        let registry = Registry::new()
            .layer_dirs(
                &SkillsSource::Sources(vec![SkillSource::Inline(InlineSkill {
                    name: "recipes".to_string(),
                    // A colon in the description is the classic YAML
                    // trap that string-interpolated frontmatter loses.
                    description: "House recipes: start here.".to_string(),
                    body: "# Recipes\n\nProject explicit columns.\n".to_string(),
                    references_tools: vec!["cypher_query".to_string()],
                    delivery: Some("lazy".to_string()),
                    applies_when: Some(AppliesWhen {
                        graph_has_node_type: Some(vec!["Function".to_string()]),
                        ..Default::default()
                    }),
                })]),
                &yaml,
            )
            .unwrap()
            .finalise()
            .unwrap();

        let skill = registry.get("recipes").unwrap();
        assert_eq!(skill.description(), "House recipes: start here.");
        assert_eq!(skill.body, "# Recipes\n\nProject explicit columns.\n");
        assert_eq!(skill.frontmatter.references_tools, ["cypher_query"]);
        assert_eq!(skill.frontmatter.delivery, Delivery::Lazy);
        assert_eq!(
            skill.frontmatter.applies_when,
            Some(AppliesWhen {
                graph_has_node_type: Some(vec!["Function".to_string()]),
                ..Default::default()
            })
        );
    }

    #[test]
    fn inline_layer_oversized_entry_warns_and_rest_loads() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = yaml_in(dir.path());

        let mut huge = inline("huge", "too big.");
        huge.body = "x".repeat(HARD_SIZE_LIMIT_BYTES);

        let registry = Registry::new()
            .layer_dirs(
                &SkillsSource::Sources(vec![
                    SkillSource::Inline(huge),
                    SkillSource::Inline(inline("intact", "from inline.")),
                ]),
                &yaml,
            )
            .unwrap()
            .finalise()
            .unwrap();

        assert!(registry.get("huge").is_none());
        assert_eq!(
            registry.get("intact").unwrap().description(),
            "from inline."
        );
        let warnings = registry.parse_warnings();
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0].path,
            PathBuf::from("<inline:huge>"),
            "the warning must name the inline entry"
        );
        assert!(warnings[0].error.contains("hard limit"));
    }

    #[test]
    fn inline_entry_rejected_by_parse_skill_warns_rather_than_failing() {
        // The manifest parser guarantees non-empty name/description/
        // body, but `InlineSkill`'s fields are public, so a Rust
        // caller can hand `layer_dirs` an entry `parse_skill` refuses.
        // That is data, like a bad file in a pack: warn and carry on.
        let dir = tempfile::tempdir().unwrap();
        let yaml = yaml_in(dir.path());

        let mut nameless = inline("nameless", "from inline.");
        nameless.name = String::new();

        let registry = Registry::new()
            .layer_dirs(
                &SkillsSource::Sources(vec![
                    SkillSource::Inline(nameless),
                    SkillSource::Inline(inline("intact", "from inline.")),
                ]),
                &yaml,
            )
            .unwrap()
            .finalise()
            .unwrap();

        assert_eq!(registry.skill_names(), vec!["intact".to_string()]);
        let warnings = registry.parse_warnings();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].path, PathBuf::from("<inline:>"));
        assert!(
            warnings[0].error.contains("name"),
            "warning should name the missing field: {}",
            warnings[0].error
        );
    }

    #[test]
    fn inline_provenance_renders_as_inline() {
        assert_eq!(format_provenance(&SkillProvenance::Inline), "inline");
    }

    #[test]
    fn rendered_inline_skill_round_trips_through_parse_skill() {
        // The render is only correct if `parse_skill` gives back what
        // went in — the layer relies on that, and a body that happens
        // to contain a `---` line is the case a naive renderer loses.
        let entry = InlineSkill {
            name: "tricky".to_string(),
            description: "Body: with a rule.".to_string(),
            body: "intro\n\n---\n\noutro\n".to_string(),
            references_tools: vec!["a".to_string()],
            delivery: Some("eager".to_string()),
            applies_when: None,
        };
        let rendered = render_inline_skill(&entry);
        let (frontmatter, body) = parse_skill(&rendered, Path::new("<inline:tricky>")).unwrap();
        assert_eq!(frontmatter.name, "tricky");
        assert_eq!(frontmatter.description, "Body: with a rule.");
        assert_eq!(frontmatter.references_tools, ["a"]);
        assert_eq!(frontmatter.delivery, Delivery::Eager);
        assert_eq!(body, entry.body);
    }

    #[test]
    fn from_manifest_resolves_full_stack() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("test_mcp.yaml");
        fs::write(&yaml, "name: x\nskills:\n  - true\n  - ./domain-pack\n").unwrap();

        let project_dir = dir.path().join("test_mcp.skills");
        fs::create_dir(&project_dir).unwrap();
        fs::write(project_dir.join("a.md"), minimal_skill("a")).unwrap();

        let pack_dir = dir.path().join("domain-pack");
        fs::create_dir(&pack_dir).unwrap();
        fs::write(pack_dir.join("b.md"), minimal_skill("b")).unwrap();

        let registry = Registry::from_manifest(&yaml, false).unwrap();
        let names = registry.skill_names();
        assert!(names.contains(&"a".to_string()));
        assert!(names.contains(&"b".to_string()));
        assert_eq!(
            registry.get("a").unwrap().provenance,
            SkillProvenance::Project
        );
    }

    #[test]
    fn from_manifest_surfaces_manifest_load_error() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("broken_mcp.yaml");
        fs::write(&yaml, "this: is: not: valid yaml\n").unwrap();

        let err = Registry::from_manifest(&yaml, false).unwrap_err();
        assert!(matches!(err, SkillError::Manifest { .. }));
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

    // ─── Bundled-skill gating on their own tool ───────────────────

    fn bundled_skill(name: &str) -> Skill {
        let bundled = library_bundled_skills()
            .into_iter()
            .find(|b| b.name == name)
            .unwrap_or_else(|| panic!("bundled skill `{name}` not in the index"));
        let path = PathBuf::from(format!("<bundled:{name}>"));
        let (frontmatter, body) = parse_skill(bundled.body, &path).unwrap();
        Skill {
            frontmatter,
            body,
            provenance: SkillProvenance::Bundled,
        }
    }

    #[test]
    fn optional_bundled_skills_declare_their_tool_registered_gate() {
        // `github_issues` registers only under `builtins.github: true`
        // with a reachable token; `repo_management` only in workspace
        // mode (`kind: github`). Both gate on the registration
        // *outcome*, so one predicate covers both origins. Without the
        // gate they advertised methodology for tools absent from the
        // session (observed downstream in `--graph` mode).
        for name in ["github_issues", "repo_management"] {
            let skill = bundled_skill(name);
            let applies =
                skill.frontmatter.applies_when.as_ref().unwrap_or_else(|| {
                    panic!("bundled skill `{name}` lost its applies_when block")
                });
            assert_eq!(
                applies.tool_registered.as_deref(),
                Some(name),
                "bundled skill `{name}` must gate on its own tool"
            );
        }
    }

    #[test]
    fn always_registered_bundled_skills_stay_ungated() {
        // The source-tool skills ship with every deployment — gating
        // them would suppress the framework's baseline methodology.
        for name in ["grep", "read_source", "list_source"] {
            let skill = bundled_skill(name);
            assert!(
                skill.frontmatter.applies_when.is_none(),
                "bundled skill `{name}` should not be predicate-gated"
            );
        }
    }

    #[test]
    fn optional_bundled_skills_activate_only_with_their_tool() {
        let registry = ResolvedRegistry::default();
        let extensions = serde_json::Map::new();
        for name in ["github_issues", "repo_management"] {
            let skill = bundled_skill(name);

            // Tool unregistered (github builtins off / non-workspace
            // mode) → suppressed from prompts/list and auto-inject.
            let inactive =
                registry.activation_for(&skill, &std::collections::HashSet::new(), &extensions);
            assert!(
                !inactive.active,
                "bundled skill `{name}` must be inactive when `{name}` is unregistered"
            );
            assert_eq!(
                inactive.clauses,
                vec![(
                    format!("tool_registered: {name}"),
                    PredicateOutcome::Unsatisfied
                )]
            );

            // Tool registered → active.
            let mut tools = std::collections::HashSet::new();
            tools.insert(name.to_string());
            let active = registry.activation_for(&skill, &tools, &extensions);
            assert!(
                active.active,
                "bundled skill `{name}` must be active when `{name}` is registered"
            );
            assert_eq!(active.clauses[0].1, PredicateOutcome::Satisfied);
        }
    }

    #[test]
    fn unrelated_registered_tool_does_not_activate_optional_bundled_skills() {
        // A session with the source tools but no github tools (the
        // `--graph` deployment) must not surface either skill.
        let registry = ResolvedRegistry::default();
        let tools: std::collections::HashSet<String> =
            ["grep", "read_source", "list_source", "set_root_dir"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        for name in ["github_issues", "repo_management"] {
            let skill = bundled_skill(name);
            assert!(
                !registry
                    .activation_for(&skill, &tools, &serde_json::Map::new())
                    .active,
                "bundled skill `{name}` leaked into a session without `{name}`"
            );
        }
    }
}
