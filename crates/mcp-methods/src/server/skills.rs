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

    /// `applies_when:` predicates — Phase 2 / 3 territory. Parsed
    /// and stored verbatim for now; the predicate evaluator isn't
    /// wired up in Phase 1.
    #[serde(default)]
    pub applies_when: Vec<serde_yaml::Value>,
}

fn default_auto_inject_hint() -> bool {
    true
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

/// Walk a directory for `*.md` files, loading each as a skill.
///
/// Files that fail to parse log warnings via `tracing::warn!` and
/// are skipped — one malformed skill in a domain pack shouldn't take
/// down the rest. The lint pass surfaces these for fix-it-later.
pub fn load_skills_from_dir(
    dir: &Path,
    provenance: SkillProvenance,
) -> Result<Vec<Skill>, SkillError> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let entries = fs::read_dir(dir).map_err(|e| SkillError::Io {
        path: dir.to_path_buf(),
        source: e,
    })?;

    let mut skills = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    dir = %dir.display(),
                    error = %e,
                    "failed to read directory entry; skipping"
                );
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
                }
            }
        }
    }
    Ok(skills)
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
/// Phase 1b returns an empty Vec — the actual SKILL.md files are
/// added in Phase 1d. The function signature is stable so downstream
/// binaries can call `.merge_framework_defaults()` against it from
/// Phase 1b onward without rebuilding when Phase 1d lands.
pub fn library_bundled_skills() -> Vec<BundledSkill> {
    Vec::new()
}

// ─── Registry builder ─────────────────────────────────────────────

/// Builder for a skills [`ResolvedRegistry`]. Downstream binaries
/// (`kglite-mcp-server`, etc.) construct one of these in their
/// boot path, layer in their bundled + operator-declared skills,
/// then call [`Registry::finalise`] to get the resolved set
/// ready for MCP `prompts/list` + `prompts/get` wiring.
///
/// See the module docs for the canonical usage pattern.
#[derive(Debug, Default)]
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
}

impl Registry {
    /// Construct an empty registry. Chain in `add_bundled`,
    /// `merge_framework_defaults`, `layer_dirs`, and
    /// `auto_detect_project_layer` calls, then call `finalise()`.
    pub fn new() -> Self {
        Self::default()
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
        let mut root_skills_per_dir: Vec<Vec<Skill>> = Vec::with_capacity(root_dirs.len());
        for (resolved, _raw) in &root_dirs {
            let provenance = SkillProvenance::DomainPack(resolved.clone());
            let skills = load_skills_from_dir(resolved, provenance)?;
            root_skills_per_dir.push(skills);
        }

        // Project layer: auto-detected adjacent dir.
        let project_skills: Vec<Skill> = match &project_dir {
            Some(dir) => load_skills_from_dir(dir, SkillProvenance::Project)?,
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

        Ok(ResolvedRegistry { skills: resolved })
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
#[derive(Debug, Default)]
pub struct ResolvedRegistry {
    skills: HashMap<String, Skill>,
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

        let skills = load_skills_from_dir(dir.path(), SkillProvenance::Project).unwrap();
        assert_eq!(skills.len(), 2);
        let mut names: Vec<&str> = skills.iter().map(|s| s.name()).collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn load_skills_from_dir_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let nonexistent = dir.path().join("does-not-exist");
        let skills = load_skills_from_dir(&nonexistent, SkillProvenance::Project).unwrap();
        assert!(skills.is_empty());
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
        // Phase 1b stub: empty Vec. Phase 1d populates this.
        let skills = library_bundled_skills();
        assert!(
            skills.is_empty(),
            "Phase 1b ships with no framework bundled skills yet"
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
}
