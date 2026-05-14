//! Reusable helpers for skills-related CLI subcommands.
//!
//! Downstream binaries (`mcp-server`, `kglite-mcp-server`, …) plug
//! these into their own `clap` setup to offer `skills-lint`,
//! `skills-list`, and `skills-show` without re-implementing the
//! load / resolve / format flow. Each helper returns a string ready
//! to print, plus an exit-code indicator where relevant.
//!
//! ```ignore
//! use mcp_methods::server::cli;
//! match cli::skills_lint(&dir) {
//!     Ok(report) => println!("{report}"),
//!     Err(e) => { eprintln!("{e}"); std::process::exit(2); }
//! }
//! ```

use std::fmt::Write as _;
use std::path::Path;

use crate::server::manifest::load as load_manifest;
use crate::server::skills::{
    load_skill_from_file, Registry, ResolvedRegistry, Skill, SkillError, SkillProvenance,
};

/// Result of [`skills_lint`] — a one-line report per file and a
/// boolean indicating whether any error was found.
#[derive(Debug)]
pub struct LintReport {
    /// Per-file lines, ordered by file path.
    pub lines: Vec<String>,
    /// True when at least one file in `dir` failed to parse or
    /// violated a hard constraint (size limit, missing required
    /// field).
    pub has_errors: bool,
}

impl LintReport {
    /// Render the report as a single string suitable for stdout.
    pub fn format(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            let _ = writeln!(out, "{line}");
        }
        let _ = writeln!(
            out,
            "\n{} file(s) checked; {}.",
            self.lines.len(),
            if self.has_errors {
                "errors found"
            } else {
                "clean"
            }
        );
        out
    }
}

/// Walk `dir` for `*.md` files, parse each as a skill, and report
/// per-file status. Soft warnings (size 4–16 KB) annotate the line
/// but don't flip `has_errors`. Hard failures (missing frontmatter,
/// missing required field, >16 KB) emit an `ERROR` line and flip
/// `has_errors` so operators can wire a non-zero exit on lint failure.
///
/// Errors at the directory level (path missing, not a directory)
/// surface as `Err`.
pub fn skills_lint(dir: &Path) -> Result<LintReport, SkillError> {
    use std::path::PathBuf;
    if !dir.exists() {
        return Err(SkillError::PathNotFound {
            raw: dir.display().to_string(),
            resolved: dir.to_path_buf(),
        });
    }
    if !dir.is_dir() {
        return Err(SkillError::PathNotFound {
            raw: dir.display().to_string(),
            resolved: dir.to_path_buf(),
        });
    }

    let entries = std::fs::read_dir(dir).map_err(|e| SkillError::Io {
        path: dir.to_path_buf(),
        source: e,
    })?;

    let mut lines: Vec<String> = Vec::new();
    let mut has_errors = false;
    let provenance = SkillProvenance::DomainPack(PathBuf::from("lint"));
    let mut any_md = false;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "md").unwrap_or(false) {
            any_md = true;
            match load_skill_from_file(&path, provenance.clone()) {
                Ok(skill) => {
                    let size = skill.body.len();
                    let warn = if size > 4096 {
                        format!(" [WARN: {size} bytes exceeds 4 KB soft limit]")
                    } else {
                        String::new()
                    };
                    lines.push(format!(
                        "  OK     {:<28}  {} bytes{warn}",
                        skill.name(),
                        size
                    ));
                }
                Err(e) => {
                    has_errors = true;
                    let basename = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    lines.push(format!("  ERROR  {basename:<28}  {e}"));
                }
            }
        }
    }
    if !any_md {
        lines.push("  (no SKILL.md files found)".to_string());
    }
    lines.sort();
    Ok(LintReport { lines, has_errors })
}

/// Build a registry from a manifest YAML and return a one-line-per-
/// skill summary suitable for stdout. Output columns: name, provenance,
/// description (truncated).
///
/// `include_bundled` controls whether the framework defaults are
/// merged before the operator-declared layers. Defaults to `true`
/// for CLI use.
pub fn skills_list(manifest_path: &Path, include_bundled: bool) -> Result<String, String> {
    let registry = build_registry(manifest_path, include_bundled)?;
    Ok(format_skill_list(&registry))
}

/// Look up a single skill by name and return its full body, prefixed
/// with a header line showing the name and provenance. Returns `Err`
/// if the skill is not present in the resolved set.
pub fn skills_show(
    manifest_path: &Path,
    name: &str,
    include_bundled: bool,
) -> Result<String, String> {
    let registry = build_registry(manifest_path, include_bundled)?;
    let skill = registry
        .get(name)
        .ok_or_else(|| format!("no skill named '{name}' resolved from {manifest_path:?}"))?;
    Ok(format_skill_body(skill))
}

fn build_registry(manifest_path: &Path, include_bundled: bool) -> Result<ResolvedRegistry, String> {
    let manifest =
        load_manifest(manifest_path).map_err(|e| format!("manifest load failed: {e}"))?;
    let mut builder = Registry::new();
    if include_bundled {
        builder = builder.merge_framework_defaults();
    }
    builder = builder.auto_detect_project_layer(manifest_path);
    builder = builder
        .layer_dirs(&manifest.skills, manifest_path)
        .map_err(|e| format!("skill layer load failed: {e}"))?;
    builder
        .finalise()
        .map_err(|e| format!("registry finalise failed: {e}"))
}

fn format_skill_list(registry: &ResolvedRegistry) -> String {
    if registry.is_empty() {
        return "(no skills resolved)\n".to_string();
    }
    let mut out = String::new();
    let _ = writeln!(out, "{:<28}  {:<14}  description", "name", "provenance");
    let _ = writeln!(
        out,
        "{:<28}  {:<14}  {}",
        "-".repeat(28),
        "-".repeat(14),
        "-".repeat(40)
    );
    for name in registry.skill_names() {
        let Some(skill) = registry.get(&name) else {
            continue;
        };
        let prov = provenance_label(&skill.provenance);
        let desc: String = skill.description().chars().take(60).collect();
        let _ = writeln!(out, "{:<28}  {:<14}  {desc}", skill.name(), prov);
    }
    out
}

fn format_skill_body(skill: &Skill) -> String {
    let prov = provenance_label(&skill.provenance);
    let mut out = String::new();
    let _ = writeln!(out, "# {} ({prov})", skill.name());
    let _ = writeln!(out, "{}", skill.description());
    let _ = writeln!(out);
    out.push_str(&skill.body);
    out
}

fn provenance_label(p: &SkillProvenance) -> String {
    match p {
        SkillProvenance::Project => "project".to_string(),
        SkillProvenance::DomainPack(_) => "domain_pack".to_string(),
        SkillProvenance::Bundled => "bundled".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_skill(dir: &Path, name: &str, body: &str) {
        fs::write(
            dir.join(format!("{name}.md")),
            format!("---\nname: {name}\ndescription: A {name} skill.\n---\n\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn skills_lint_reports_each_file() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "alpha", "Body alpha.");
        write_skill(dir.path(), "beta", "Body beta.");
        let report = skills_lint(dir.path()).unwrap();
        assert!(!report.has_errors);
        assert!(report.lines.iter().any(|l| l.contains("alpha")));
        assert!(report.lines.iter().any(|l| l.contains("beta")));
    }

    #[test]
    fn skills_lint_empty_dir_emits_friendly_line() {
        let dir = tempfile::tempdir().unwrap();
        let report = skills_lint(dir.path()).unwrap();
        assert!(!report.has_errors);
        assert!(report.lines.iter().any(|l| l.contains("no SKILL.md files")));
    }

    #[test]
    fn skills_lint_invalid_dir_errors() {
        let bogus = Path::new("/nonexistent/path/for/lint");
        let result = skills_lint(bogus);
        assert!(result.is_err());
    }

    #[test]
    fn skills_lint_size_warning_at_4kb() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(5_000);
        write_skill(dir.path(), "fat", &big);
        let report = skills_lint(dir.path()).unwrap();
        // Soft warn but not a hard error.
        assert!(!report.has_errors);
        assert!(report
            .lines
            .iter()
            .any(|l| l.contains("WARN") && l.contains("4 KB")));
    }

    #[test]
    fn skills_list_renders_table_for_resolved_set() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("test_mcp.yaml");
        fs::write(&manifest, "name: t\nskills: true\n").unwrap();
        let skills_dir = dir.path().join("test_mcp.skills");
        fs::create_dir(&skills_dir).unwrap();
        write_skill(&skills_dir, "custom", "Custom body.");
        let output = skills_list(&manifest, true).unwrap();
        assert!(output.contains("custom"));
        assert!(output.contains("grep"), "expected bundled grep in output");
        assert!(output.contains("project"));
        assert!(output.contains("bundled"));
    }

    #[test]
    fn skills_list_without_bundled() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("test_mcp.yaml");
        fs::write(&manifest, "name: t\nskills: true\n").unwrap();
        let skills_dir = dir.path().join("test_mcp.skills");
        fs::create_dir(&skills_dir).unwrap();
        write_skill(&skills_dir, "custom", "Custom body.");
        let output = skills_list(&manifest, false).unwrap();
        assert!(output.contains("custom"));
        assert!(
            !output.contains("\ngrep "),
            "bundled grep should be excluded"
        );
    }

    #[test]
    fn skills_show_returns_body_with_header() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("test_mcp.yaml");
        fs::write(&manifest, "name: t\nskills: true\n").unwrap();
        let skills_dir = dir.path().join("test_mcp.skills");
        fs::create_dir(&skills_dir).unwrap();
        write_skill(&skills_dir, "alpha", "ALPHA-BODY-MARKER");
        let output = skills_show(&manifest, "alpha", false).unwrap();
        assert!(output.starts_with("# alpha"));
        assert!(output.contains("ALPHA-BODY-MARKER"));
        assert!(output.contains("project"));
    }

    #[test]
    fn skills_show_missing_skill_errors() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("test_mcp.yaml");
        fs::write(&manifest, "name: t\n").unwrap();
        let err = skills_show(&manifest, "nonexistent", false).unwrap_err();
        assert!(err.contains("no skill named"));
    }

    #[test]
    fn skills_list_no_skills_declared_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("test_mcp.yaml");
        fs::write(&manifest, "name: t\n").unwrap();
        let output = skills_list(&manifest, false).unwrap();
        assert!(output.contains("no skills resolved"));
    }
}
