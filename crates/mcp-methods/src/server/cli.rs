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

use std::path::PathBuf;

use crate::server::manifest::load as load_manifest;
use crate::server::skills::{
    load_skill_from_file, write_skill_template, Registry, ResolvedRegistry, Skill, SkillError,
    SkillProvenance,
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

/// Scaffold a starter SKILL.md at `dest` and return the resolved
/// path written. Thin wrapper around
/// [`write_skill_template`]
/// that bubbles errors as `String` for symmetric handling alongside
/// [`skills_list`] / [`skills_show`].
///
/// `description` is required — Anthropic's published guidance is that
/// skills with weak descriptions undertrigger badly, so the template
/// makes the operator commit to one rather than leaving a `<TODO>`
/// placeholder in the discovery-critical field.
pub fn skills_new(dest: &Path, name: &str, description: &str) -> Result<PathBuf, String> {
    if name.trim().is_empty() {
        return Err("skill name must not be empty".to_string());
    }
    if description.trim().is_empty() {
        return Err(
            "description must not be empty — it's the agent's only signal for triggering"
                .to_string(),
        );
    }
    write_skill_template(dest, name, description).map_err(|e| format!("template write failed: {e}"))
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
    // The CLI is the operator-facing surface — show predicate state
    // even when it's "always active". The split-view design (boot log
    // shows full state, agent prompts/list shows filtered) lets
    // operators debug "why isn't my skill firing?" by reading this
    // output. We pass empty tool / extension state since `skills-list`
    // runs without a live server, so every runtime-state predicate
    // (`tool_registered:`, `extension_enabled:`) necessarily evaluates
    // Unsatisfied here — that verdict is an artefact of the offline
    // evaluation, not a real suppression. Those clauses are therefore
    // reported as `[RUNTIME]` and the skill as `conditional`, never as
    // `[   FAIL]` / `inactive`. Only predicates the CLI can actually
    // decide from the manifest + skill files (the domain predicates)
    // are allowed to mark a skill `inactive`.
    let empty_tools = std::collections::HashSet::new();
    let empty_ext = serde_json::Map::new();

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<28}  {:<14}  {:<12}  description",
        "name", "provenance", "status"
    );
    let _ = writeln!(
        out,
        "{:<28}  {:<14}  {:<12}  {}",
        "-".repeat(28),
        "-".repeat(14),
        "-".repeat(12),
        "-".repeat(40)
    );
    for name in registry.skill_names() {
        let Some(skill) = registry.get(&name) else {
            continue;
        };
        let prov = provenance_label(&skill.provenance);
        let activation = registry.activation_for(skill, &empty_tools, &empty_ext);
        // Clauses this CLI run cannot decide offline, keyed by the exact
        // label `activation_for` renders for them.
        let runtime = runtime_clause_notes(skill);
        let runtime_note = |clause: &String| -> Option<&'static str> {
            runtime
                .iter()
                .find(|(label, _)| label == clause)
                .map(|(_, note)| *note)
        };
        // A skill is only truly suppressed when a predicate the CLI can
        // decide came out false; runtime-state clauses don't count.
        let decidable_failure = activation.clauses.iter().any(|(clause, outcome)| {
            *outcome != crate::server::skills::PredicateOutcome::Satisfied
                && runtime_note(clause).is_none()
        });
        let status = if activation.active {
            "active"
        } else if decidable_failure {
            "inactive"
        } else {
            "conditional"
        };
        let desc: String = skill.description().chars().take(60).collect();
        let _ = writeln!(
            out,
            "{:<28}  {:<14}  {status:<12}  {desc}",
            skill.name(),
            prov
        );
        // For skills that aren't unconditionally active, surface each
        // predicate so the operator can debug. Indented sub-lines keep
        // the table readable while still giving full attribution.
        if !activation.active {
            for (clause, outcome) in &activation.clauses {
                let unresolved_runtime =
                    *outcome != crate::server::skills::PredicateOutcome::Satisfied;
                match runtime_note(clause).filter(|_| unresolved_runtime) {
                    Some(note) => {
                        // "RUNTIME" is exactly the 7-wide mark column
                        // the other marks are right-aligned into.
                        let _ = writeln!(out, "    [RUNTIME]  {clause} — {note}");
                    }
                    None => {
                        let mark = match outcome {
                            crate::server::skills::PredicateOutcome::Satisfied => "ok",
                            crate::server::skills::PredicateOutcome::Unsatisfied => "FAIL",
                            crate::server::skills::PredicateOutcome::Unknown => "UNKNOWN",
                        };
                        let _ = writeln!(out, "    [{mark:>7}]  {clause}");
                    }
                }
            }
        }
    }
    out
}

/// The `activation.clauses` labels that come from *runtime-state*
/// predicates, each paired with the note explaining where the real
/// answer comes from.
///
/// `skills-list` has no live server, so these predicates cannot be
/// evaluated offline; rendering their forced `Unsatisfied` verdict as a
/// failure would tell operators a correctly-configured skill is
/// suppressed. The labels are rebuilt from the skill's typed
/// frontmatter using the same `format!` shapes
/// [`Registry::activation_for`](crate::server::skills::Registry::activation_for)
/// uses, so classification keys off the parsed predicate set rather
/// than sniffing the prefix of a display string.
fn runtime_clause_notes(skill: &Skill) -> Vec<(String, &'static str)> {
    let mut notes = Vec::new();
    let Some(applies_when) = skill.frontmatter.applies_when.as_ref() else {
        return notes;
    };
    if let Some(tool) = applies_when.tool_registered.as_ref() {
        notes.push((
            format!("tool_registered: {tool}"),
            "resolved against the live tool router at boot",
        ));
    }
    if let Some(key) = applies_when.extension_enabled.as_ref() {
        notes.push((
            format!("extension_enabled: {key}"),
            "resolved against the live manifest extensions at boot",
        ));
    }
    notes
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

    /// Write a skill whose frontmatter carries an `applies_when:`
    /// block. `applies_when` is the already-indented YAML body of the
    /// block (each line prefixed with two spaces, newline-terminated).
    fn write_gated_skill(dir: &Path, name: &str, applies_when: &str) {
        fs::write(
            dir.join(format!("{name}.md")),
            format!(
                "---\nname: {name}\ndescription: A {name} skill.\napplies_when:\n{applies_when}---\n\nBody {name}.\n"
            ),
        )
        .unwrap();
    }

    /// Answers the domain predicates with a hard "no" so the CLI has a
    /// clause it can actually decide offline.
    struct DenyGraphPredicates;

    impl crate::server::skills::SkillPredicateEvaluator for DenyGraphPredicates {
        fn evaluate(&self, clause: &crate::server::skills::PredicateClause<'_>) -> Option<bool> {
            use crate::server::skills::PredicateClause;
            match clause {
                PredicateClause::GraphHasNodeType(_) | PredicateClause::GraphHasProperty { .. } => {
                    Some(false)
                }
                _ => None,
            }
        }
    }

    /// Build a registry from the project layer next to `manifest`,
    /// with the domain-predicate evaluator attached — `skills_list`
    /// can't register one, and we need a decidable failing clause.
    fn project_registry_with_evaluator(manifest: &Path) -> ResolvedRegistry {
        Registry::new()
            .with_predicate_evaluator(DenyGraphPredicates)
            .auto_detect_project_layer(manifest)
            .finalise()
            .unwrap()
    }

    fn line_for<'a>(output: &'a str, name: &str) -> &'a str {
        output
            .lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("no row for {name} in:\n{output}"))
    }

    #[test]
    fn skills_list_reports_runtime_gated_skills_as_conditional() {
        // `skills-list` runs without a live server, so `tool_registered:`
        // is unknowable offline. It must not be rendered as a failure —
        // the bundled github/workspace skills are gated on their own
        // tool and would otherwise report as suppressed in *every*
        // deployment, including ones where the tool registers at boot.
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("test_mcp.yaml");
        fs::write(&manifest, "name: t\nskills: true\n").unwrap();
        let output = skills_list(&manifest, true).unwrap();

        for name in ["github_issues", "repo_management"] {
            let row = line_for(&output, name);
            assert!(
                row.contains("conditional"),
                "expected `{name}` to be conditional, got: {row}"
            );
            assert!(
                !row.contains("inactive"),
                "`{name}` must not be reported inactive offline, got: {row}"
            );
            assert!(
                output.contains(&format!(
                    "    [RUNTIME]  tool_registered: {name} — resolved against the live tool router at boot"
                )),
                "expected a RUNTIME clause line for `{name}` in:\n{output}"
            );
        }
        assert!(
            !output.contains("FAIL"),
            "no bundled skill may render as FAIL offline:\n{output}"
        );
    }

    #[test]
    fn skills_list_extension_gate_is_runtime_too() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("test_mcp.yaml");
        fs::write(&manifest, "name: t\nskills: true\n").unwrap();
        let skills_dir = dir.path().join("test_mcp.skills");
        fs::create_dir(&skills_dir).unwrap();
        write_gated_skill(
            &skills_dir,
            "gated",
            "  extension_enabled: csv_http_server\n",
        );
        let output = skills_list(&manifest, false).unwrap();
        assert!(line_for(&output, "gated").contains("conditional"));
        assert!(output.contains(
            "    [RUNTIME]  extension_enabled: csv_http_server — resolved against the live manifest extensions at boot"
        ), "got:\n{output}");
    }

    #[test]
    fn format_skill_list_keeps_decidable_failures_inactive() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("test_mcp.yaml");
        fs::write(&manifest, "name: t\nskills: true\n").unwrap();
        let skills_dir = dir.path().join("test_mcp.skills");
        fs::create_dir(&skills_dir).unwrap();
        write_gated_skill(&skills_dir, "domain", "  graph_has_node_type: [Function]\n");
        let output = format_skill_list(&project_registry_with_evaluator(&manifest));
        assert!(
            line_for(&output, "domain").contains("inactive"),
            "got:\n{output}"
        );
        assert!(
            output.contains("    [   FAIL]  graph_has_node_type: [\"Function\"]"),
            "got:\n{output}"
        );
    }

    #[test]
    fn format_skill_list_decidable_failure_wins_over_runtime_clause() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("test_mcp.yaml");
        fs::write(&manifest, "name: t\nskills: true\n").unwrap();
        let skills_dir = dir.path().join("test_mcp.skills");
        fs::create_dir(&skills_dir).unwrap();
        write_gated_skill(
            &skills_dir,
            "mixed",
            "  graph_has_node_type: [Function]\n  tool_registered: cypher_query\n",
        );
        let output = format_skill_list(&project_registry_with_evaluator(&manifest));
        // A clause the CLI *can* decide came out false — that's a real
        // suppression, so `inactive` wins over `conditional`.
        assert!(
            line_for(&output, "mixed").contains("inactive"),
            "got:\n{output}"
        );
        assert!(
            output.contains("    [   FAIL]  graph_has_node_type"),
            "got:\n{output}"
        );
        assert!(
            output.contains("    [RUNTIME]  tool_registered: cypher_query — resolved against the live tool router at boot"),
            "got:\n{output}"
        );
    }

    #[test]
    fn format_skill_list_ungated_skill_stays_active_without_clause_lines() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("test_mcp.yaml");
        fs::write(&manifest, "name: t\nskills: true\n").unwrap();
        let skills_dir = dir.path().join("test_mcp.skills");
        fs::create_dir(&skills_dir).unwrap();
        write_skill(&skills_dir, "plain", "Plain body.");
        let output = skills_list(&manifest, false).unwrap();
        let row = line_for(&output, "plain");
        assert!(row.contains("active"), "got: {row}");
        assert!(!row.contains("conditional") && !row.contains("inactive"));
        assert!(
            !output.contains("    ["),
            "no clause lines expected:\n{output}"
        );
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

    #[test]
    fn skills_new_scaffolds_into_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let dest = skills_new(dir.path(), "custom", "A short description.").unwrap();
        assert_eq!(dest, dir.path().join("custom.md"));
        let content = fs::read_to_string(&dest).unwrap();
        assert!(content.contains("name: custom"));
        assert!(content.contains("# `custom` methodology"));
    }

    #[test]
    fn skills_new_rejects_empty_name() {
        let dir = tempfile::tempdir().unwrap();
        let err = skills_new(dir.path(), "", "A description.").unwrap_err();
        assert!(err.contains("name must not be empty"));
    }

    #[test]
    fn skills_new_rejects_empty_description() {
        let dir = tempfile::tempdir().unwrap();
        let err = skills_new(dir.path(), "custom", "   ").unwrap_err();
        assert!(err.contains("description must not be empty"));
    }

    #[test]
    fn skills_new_bubbles_write_errors() {
        let dir = tempfile::tempdir().unwrap();
        // Pre-create the file to trigger the AlreadyExists branch.
        fs::write(dir.path().join("custom.md"), "x").unwrap();
        let err = skills_new(dir.path(), "custom", "description").unwrap_err();
        assert!(err.contains("template write failed"));
    }
}
