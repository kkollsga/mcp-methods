//! Compile-time embedding of the framework's bundled skills.
//!
//! The five SKILL.md files in `./bundled_skills/` ship inside the
//! binary via `include_str!`. They form the bottom layer of the
//! three-layer composition (project → domain pack → bundled);
//! operator-authored skills always override them.
//!
//! Adding a new bundled skill: drop a SKILL.md into `bundled_skills/`,
//! add an entry to the `library_bundled_skills` Vec below, then let
//! the test `each_framework_bundled_skill_parses` confirm the
//! frontmatter is valid before the next release.

use crate::server::skills::BundledSkill;

/// The framework's bundled skills. Returned in declaration order; the
/// resolver doesn't care about order within the bundled layer
/// (collisions among bundled skills are themselves a framework-author
/// bug — every name in this Vec must be unique).
pub fn library_bundled_skills() -> Vec<BundledSkill> {
    vec![
        BundledSkill {
            name: "grep",
            body: include_str!("bundled_skills/grep.md"),
        },
        BundledSkill {
            name: "read_source",
            body: include_str!("bundled_skills/read_source.md"),
        },
        BundledSkill {
            name: "list_source",
            body: include_str!("bundled_skills/list_source.md"),
        },
        BundledSkill {
            name: "github_issues",
            body: include_str!("bundled_skills/github_discussions.md"),
        },
        BundledSkill {
            name: "repo_management",
            body: include_str!("bundled_skills/repo_management.md"),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::skills::parse_skill;
    use std::path::PathBuf;

    #[test]
    fn library_bundled_skills_returns_five_skills() {
        let skills = library_bundled_skills();
        assert_eq!(skills.len(), 5);
        let names: Vec<&str> = skills.iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![
                "grep",
                "read_source",
                "list_source",
                "github_issues",
                "repo_management",
            ]
        );
    }

    #[test]
    fn bundled_skill_names_are_unique() {
        let skills = library_bundled_skills();
        let mut names: Vec<&str> = skills.iter().map(|s| s.name).collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate bundled skill name");
    }

    #[test]
    fn each_framework_bundled_skill_parses() {
        // Every bundled skill must round-trip through `parse_skill` —
        // catches malformed frontmatter at CI time so operators never
        // see a `BundledSkillInvalid` at boot.
        for skill in library_bundled_skills() {
            let path = PathBuf::from(format!("bundled_skills/{}.md", skill.name));
            let (fm, body) = parse_skill(skill.body, &path).unwrap_or_else(|err| {
                panic!("bundled skill `{}` failed to parse: {err}", skill.name)
            });
            assert!(
                !fm.name.is_empty(),
                "bundled skill `{}` has empty frontmatter name",
                skill.name
            );
            assert!(
                !fm.description.is_empty(),
                "bundled skill `{}` has empty frontmatter description",
                skill.name
            );
            assert!(
                !body.trim().is_empty(),
                "bundled skill `{}` has empty body after frontmatter",
                skill.name
            );
        }
    }

    #[test]
    fn each_bundled_skill_frontmatter_name_matches_lookup_key() {
        // The Vec entry's `name` (the lookup key used by
        // `prompts/get`) must match the `name:` field inside the
        // skill's own frontmatter, or the agent would request `grep`
        // and receive a skill that introduces itself as something else.
        for skill in library_bundled_skills() {
            let path = PathBuf::from(format!("bundled_skills/{}.md", skill.name));
            let (fm, _body) = parse_skill(skill.body, &path).unwrap();
            assert_eq!(
                fm.name, skill.name,
                "lookup key `{}` does not match frontmatter name `{}`",
                skill.name, fm.name
            );
        }
    }

    #[test]
    fn bundled_skills_fit_under_size_limit() {
        // Hard limit is 16 KB per skill (matches `Registry::finalise`).
        // We check that here as a CI gate so a runaway bundled skill
        // never ships.
        const HARD_LIMIT: usize = 16 * 1024;
        for skill in library_bundled_skills() {
            let size = skill.body.len();
            assert!(
                size <= HARD_LIMIT,
                "bundled skill `{}` is {size} bytes, exceeds {HARD_LIMIT} byte hard limit",
                skill.name
            );
        }
    }
}
