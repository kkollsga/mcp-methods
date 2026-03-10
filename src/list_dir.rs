use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use pyo3::prelude::*;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const DEFAULT_SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "__pycache__",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    "dist",
    "build",
    ".eggs",
    "venv",
    ".venv",
    "target",
    ".cargo",
    ".ruff_cache",
];

static DEFAULT_SKIP_SET: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| DEFAULT_SKIP_DIRS.iter().copied().collect());

/// An entry in the directory tree.
struct Entry {
    name: String,
    full_path: PathBuf,
    is_dir: bool,
    size: u64,
    children: BTreeMap<String, Entry>,
}

impl Entry {
    fn new_dir(name: String, full_path: PathBuf) -> Self {
        Self {
            name,
            full_path,
            is_dir: true,
            size: 0,
            children: BTreeMap::new(),
        }
    }

    fn new_file(name: String, full_path: PathBuf, size: u64) -> Self {
        Self {
            name,
            full_path,
            is_dir: false,
            size,
            children: BTreeMap::new(),
        }
    }
}

/// List directory contents with tree-formatted output.
#[pyfunction]
#[pyo3(signature = (
    path,
    *,
    depth = 1,
    glob = None,
    dirs_only = false,
    relative_to = None,
    respect_gitignore = true,
    skip_dirs = None,
    include_size = false,
))]
#[allow(clippy::too_many_arguments)]
pub fn list_dir(
    path: &str,
    depth: usize,
    glob: Option<&str>,
    dirs_only: bool,
    relative_to: Option<&str>,
    respect_gitignore: bool,
    skip_dirs: Option<Vec<String>>,
    include_size: bool,
) -> PyResult<String> {
    let root = PathBuf::from(path).canonicalize().map_err(|e| {
        pyo3::exceptions::PyValueError::new_err(format!("Cannot resolve '{}': {}", path, e))
    })?;
    if !root.is_dir() {
        return Ok(format!("Error: '{}' is not a directory.", path));
    }

    let custom_skip: Option<HashSet<String>> = skip_dirs.map(|dirs| dirs.iter().cloned().collect());

    let mut tree = Entry::new_dir(dir_display_name(&root, relative_to), root.clone());
    let mut leaf_counts: BTreeMap<PathBuf, (usize, usize)> = BTreeMap::new();

    // Single walk at depth+1: build tree for entries <= depth,
    // count children for entries at exactly depth+1.
    {
        let mut builder = WalkBuilder::new(&root);
        builder.max_depth(Some(depth + 1));
        builder.hidden(false);
        builder.git_ignore(respect_gitignore);
        builder.git_global(respect_gitignore);
        builder.git_exclude(respect_gitignore);

        if let Some(glob_pat) = glob {
            let mut overrides = OverrideBuilder::new(&root);
            overrides
                .add("*/")
                .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("{}", e)))?;
            overrides
                .add(glob_pat)
                .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("{}", e)))?;
            let built = overrides
                .build()
                .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("{}", e)))?;
            builder.overrides(built);
        }

        builder.filter_entry(move |entry| {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(name) = entry.file_name().to_str() {
                    return match &custom_skip {
                        Some(set) => !set.contains(name),
                        None => !DEFAULT_SKIP_SET.contains(name),
                    };
                }
            }
            true
        });

        for entry in builder.build().flatten() {
            let entry_path = entry.path().to_path_buf();
            if entry_path == root {
                continue;
            }
            let rel = match entry_path.strip_prefix(&root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let comp_count = rel.components().count();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

            if comp_count <= depth {
                // Build tree entry
                if dirs_only && !is_dir {
                    continue;
                }
                let components: Vec<String> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().to_string())
                    .collect();
                let size = if include_size && !is_dir {
                    entry.metadata().map(|m| m.len()).unwrap_or(0)
                } else {
                    0
                };
                insert_entry(&mut tree, &components, is_dir, size, &entry_path);
            } else {
                // depth+1: count for leaf directory summaries
                if let Some(parent) = entry_path.parent() {
                    let counter = leaf_counts.entry(parent.to_path_buf()).or_insert((0, 0));
                    if is_dir {
                        counter.0 += 1;
                    } else {
                        counter.1 += 1;
                    }
                }
            }
        }
    }

    // Prune empty dirs when glob is active
    if glob.is_some() && !dirs_only {
        prune_empty_dirs(&mut tree);
    }

    if tree.children.is_empty() {
        return Ok(format!("{}/ (empty)", tree.name));
    }

    let mut output = Vec::new();
    output.push(format!("{}/", tree.name));
    render_tree(&tree, "", &mut output, include_size, &leaf_counts);
    Ok(output.join("\n"))
}

fn insert_entry(
    tree: &mut Entry,
    components: &[String],
    is_dir: bool,
    size: u64,
    full_path: &Path,
) {
    let mut node = tree;
    for (i, comp) in components.iter().enumerate() {
        if i == components.len() - 1 {
            node.children.entry(comp.clone()).or_insert_with(|| {
                if is_dir {
                    Entry::new_dir(comp.clone(), full_path.to_path_buf())
                } else {
                    Entry::new_file(comp.clone(), full_path.to_path_buf(), size)
                }
            });
        } else {
            // Intermediate directory — construct its path
            let intermediate_path: PathBuf = full_path
                .components()
                .take(full_path.components().count() - (components.len() - 1 - i))
                .collect();
            node = node
                .children
                .entry(comp.clone())
                .or_insert_with(|| Entry::new_dir(comp.clone(), intermediate_path));
        }
    }
}

fn prune_empty_dirs(entry: &mut Entry) -> bool {
    if !entry.is_dir {
        return true;
    }
    entry.children.retain(|_, child| prune_empty_dirs(child));
    !entry.children.is_empty()
}

fn render_tree(
    entry: &Entry,
    prefix: &str,
    output: &mut Vec<String>,
    include_size: bool,
    leaf_counts: &BTreeMap<PathBuf, (usize, usize)>,
) {
    let len = entry.children.len();

    for (i, child) in entry.children.values().enumerate() {
        let is_last = i == len - 1;
        let connector = if is_last { "└── " } else { "├── " };
        let child_prefix = if is_last { "    " } else { "│   " };

        if child.is_dir {
            let summary = if child.children.is_empty() {
                leaf_counts
                    .get(&child.full_path)
                    .map(|&(d, f)| format_summary(d, f))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            output.push(format!("{}{}{}/{}", prefix, connector, child.name, summary));
            if !child.children.is_empty() {
                render_tree(
                    child,
                    &format!("{}{}", prefix, child_prefix),
                    output,
                    include_size,
                    leaf_counts,
                );
            }
        } else {
            let size_str = if include_size {
                format!("  ({})", format_size(child.size))
            } else {
                String::new()
            };
            output.push(format!("{}{}{}{}", prefix, connector, child.name, size_str));
        }
    }
}

fn format_summary(dirs: usize, files: usize) -> String {
    match (dirs, files) {
        (0, 0) => String::new(),
        (0, f) => format!("           [{} file{}]", f, if f == 1 { "" } else { "s" }),
        (d, 0) => format!("           [{} dir{}]", d, if d == 1 { "" } else { "s" }),
        (d, f) => format!(
            "           [{} dir{}, {} file{}]",
            d,
            if d == 1 { "" } else { "s" },
            f,
            if f == 1 { "" } else { "s" }
        ),
    }
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn dir_display_name(path: &Path, relative_to: Option<&str>) -> String {
    if let Some(base) = relative_to {
        let base_path = PathBuf::from(base);
        if let Ok(rel) = path.strip_prefix(&base_path) {
            let s = rel.to_string_lossy().to_string();
            if s.is_empty() {
                return path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| ".".to_string());
            }
            return s;
        }
    }
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string())
}
