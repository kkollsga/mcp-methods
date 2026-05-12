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
];
const ALLOWED_WORKSPACE_KEYS: &[&str] = &["kind", "root", "watch"];
const VALID_WORKSPACE_KIND: &[&str] = &["github", "local"];
const ALLOWED_TRUST_KEYS: &[&str] = &["allow_python_tools", "allow_embedder"];
const ALLOWED_TOOL_KEYS: &[&str] = &[
    "name",
    "description",
    "parameters",
    "cypher",
    "python",
    "function",
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
}

#[derive(Debug, Clone)]
pub enum ToolSpec {
    Cypher(CypherTool),
    Python(PythonTool),
}

impl ToolSpec {
    pub fn name(&self) -> &str {
        match self {
            ToolSpec::Cypher(t) => &t.name,
            ToolSpec::Python(t) => &t.name,
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

/// Auto-detect ``workspace_mcp.yaml`` inside a workspace directory.
pub fn find_workspace_manifest(workspace_dir: &Path) -> Option<PathBuf> {
    let candidate = workspace_dir.join("workspace_mcp.yaml");
    if candidate.is_file() {
        Some(candidate)
    } else {
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
    })
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
    Ok(Some(WorkspaceConfig { kind, root, watch }))
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

    let has_cypher = map.contains_key("cypher");
    let has_python = map.contains_key("python");
    let kinds_present: Vec<&str> = [("cypher", has_cypher), ("python", has_python)]
        .into_iter()
        .filter(|(_, p)| *p)
        .map(|(k, _)| k)
        .collect();
    if kinds_present.is_empty() {
        return Err(ManifestError::at(
            yaml_path,
            format!("tools[{idx}] ({name:?}) needs exactly one of: [\"cypher\", \"python\"]"),
        ));
    }
    if kinds_present.len() > 1 {
        return Err(ManifestError::at(
            yaml_path,
            format!("tools[{idx}] ({name:?}) has multiple kinds set ({kinds_present:?}); pick one"),
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
}
