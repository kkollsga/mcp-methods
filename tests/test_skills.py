"""Tests for the Python `SkillRegistry` / `Skill` pyclasses.

Exercises the load-from-manifest path against a temp YAML + a project
skills directory, plus the FastMCP `register_skills_as_prompts`
helper against a tiny in-process app stub.
"""

from __future__ import annotations

import textwrap
from pathlib import Path

import pytest
from mcp_methods import Skill, SkillRegistry


def _write_manifest(dir: Path, *, with_skills: bool = True) -> Path:
    """Write a minimal `*_mcp.yaml` and (optionally) one project skill."""
    manifest = dir / "test_mcp.yaml"
    body = "name: test\n"
    if with_skills:
        body += "skills: true\n"
    manifest.write_text(body)
    if with_skills:
        skills_dir = dir / "test_mcp.skills"
        skills_dir.mkdir()
        (skills_dir / "custom_method.md").write_text(
            textwrap.dedent(
                """\
                ---
                name: custom_method
                description: A project-layer skill that overrides bundled defaults.
                auto_inject_hint: true
                ---

                # Custom methodology

                Body content.
                """
            )
        )
    return manifest


def test_load_registry_with_bundled_defaults(tmp_path: Path) -> None:
    manifest = _write_manifest(tmp_path)
    reg = SkillRegistry.from_manifest(str(manifest))
    names = reg.skill_names()
    # Five framework defaults plus the project skill = 6.
    assert "custom_method" in names
    assert "grep" in names
    assert "read_source" in names
    assert len(reg) >= 6
    assert "custom_method" in reg


def test_load_registry_without_bundled(tmp_path: Path) -> None:
    manifest = _write_manifest(tmp_path)
    reg = SkillRegistry.from_manifest(str(manifest), include_bundled=False)
    names = reg.skill_names()
    # Only the project skill — no framework defaults.
    assert names == ["custom_method"]
    assert len(reg) == 1
    assert "grep" not in reg


def test_skill_metadata_and_body(tmp_path: Path) -> None:
    manifest = _write_manifest(tmp_path)
    reg = SkillRegistry.from_manifest(str(manifest), include_bundled=False)
    skill = reg.get("custom_method")
    assert isinstance(skill, Skill)
    assert skill.name == "custom_method"
    assert "overrides bundled defaults" in skill.description
    assert "Custom methodology" in skill.body
    assert skill.provenance == "project"
    assert skill.auto_inject_hint is True


def test_project_layer_overrides_bundled(tmp_path: Path) -> None:
    # A project skill named `grep` should mask the bundled `grep`.
    manifest = tmp_path / "test_mcp.yaml"
    manifest.write_text("name: test\nskills: true\n")
    skills_dir = tmp_path / "test_mcp.skills"
    skills_dir.mkdir()
    (skills_dir / "grep.md").write_text(
        "---\nname: grep\ndescription: Project grep override.\n---\nOverride body.\n"
    )
    reg = SkillRegistry.from_manifest(str(manifest))
    skill = reg.get("grep")
    assert skill is not None
    assert skill.provenance == "project"
    assert "Override body." in skill.body


def test_get_missing_returns_none(tmp_path: Path) -> None:
    manifest = _write_manifest(tmp_path, with_skills=False)
    reg = SkillRegistry.from_manifest(str(manifest), include_bundled=False)
    assert reg.get("nonexistent") is None


def test_no_skills_declared_empty_registry(tmp_path: Path) -> None:
    manifest = _write_manifest(tmp_path, with_skills=False)
    reg = SkillRegistry.from_manifest(str(manifest), include_bundled=False)
    assert reg.skill_names() == []
    assert len(reg) == 0


def test_find_sibling_resolves_from_graph_path(tmp_path: Path) -> None:
    # find_sibling expects a graph/data path like `<stem>.kdb`; it
    # returns the sibling `<stem>_mcp.yaml` if present.
    manifest = _write_manifest(tmp_path, with_skills=False)
    graph_path = tmp_path / "test.kdb"
    graph_path.touch()
    found = SkillRegistry.find_sibling(str(graph_path))
    assert Path(found) == manifest


def test_find_sibling_missing_raises(tmp_path: Path) -> None:
    graph_path = tmp_path / "nothing.kdb"
    graph_path.touch()
    with pytest.raises(ValueError, match="no sibling"):
        SkillRegistry.find_sibling(str(graph_path))


def test_invalid_manifest_path_raises(tmp_path: Path) -> None:
    with pytest.raises(ValueError, match="manifest load failed"):
        SkillRegistry.from_manifest(str(tmp_path / "nonexistent.yaml"))


# ─── FastMCP helper ──────────────────────────────────────────────


class _FakeFastMCP:
    """Captures `@app.prompt(...)` decorator usage."""

    def __init__(self) -> None:
        self.prompts: dict[str, callable] = {}
        self.descriptions: dict[str, str] = {}

    def prompt(self, *, name: str, description: str):
        def decorator(fn):
            self.prompts[name] = fn
            self.descriptions[name] = description
            return fn

        return decorator


def test_register_skills_as_prompts(tmp_path: Path) -> None:
    from mcp_methods.fastmcp import register_skills_as_prompts

    manifest = _write_manifest(tmp_path)
    reg = SkillRegistry.from_manifest(str(manifest), include_bundled=False)
    app = _FakeFastMCP()
    count = register_skills_as_prompts(app, reg)
    assert count == 1
    assert "custom_method" in app.prompts
    assert "overrides bundled defaults" in app.descriptions["custom_method"]
    # Invoke the handler — it should return the skill body.
    body = app.prompts["custom_method"]()
    assert "Custom methodology" in body


def test_register_skills_per_handler_captures_correct_body(tmp_path: Path) -> None:
    # Regression guard: a naive `for` loop binds the loop variable late,
    # so every prompt handler would return the last skill's body. The
    # helper isolates each registration in a function.
    from mcp_methods.fastmcp import register_skills_as_prompts

    manifest = tmp_path / "test_mcp.yaml"
    manifest.write_text("name: test\nskills: true\n")
    skills_dir = tmp_path / "test_mcp.skills"
    skills_dir.mkdir()
    for n in ("alpha", "beta", "gamma"):
        (skills_dir / f"{n}.md").write_text(
            f"---\nname: {n}\ndescription: skill {n}.\n---\nbody-of-{n}\n"
        )
    reg = SkillRegistry.from_manifest(str(manifest), include_bundled=False)
    app = _FakeFastMCP()
    register_skills_as_prompts(app, reg)
    assert app.prompts["alpha"]().strip() == "body-of-alpha"
    assert app.prompts["beta"]().strip() == "body-of-beta"
    assert app.prompts["gamma"]().strip() == "body-of-gamma"


def test_register_skills_empty_registry_is_noop(tmp_path: Path) -> None:
    from mcp_methods.fastmcp import register_skills_as_prompts

    manifest = _write_manifest(tmp_path, with_skills=False)
    reg = SkillRegistry.from_manifest(str(manifest), include_bundled=False)
    app = _FakeFastMCP()
    count = register_skills_as_prompts(app, reg)
    assert count == 0
    assert app.prompts == {}


# ─── Skill template ────────────────────────────────────────────────


def test_render_skill_template_returns_parse_valid_body() -> None:
    from mcp_methods import render_skill_template

    body = render_skill_template("custom", "A short description.")
    assert "name: custom" in body
    assert "description: A short description." in body
    assert "# `custom` methodology" in body
    assert "## Quick Reference" in body
    assert "## Common Pitfalls" in body


def test_write_skill_template_writes_into_directory(tmp_path: Path) -> None:
    from mcp_methods import write_skill_template

    dest = write_skill_template(tmp_path, "custom", "A description.")
    assert Path(dest) == tmp_path / "custom.md"
    content = (tmp_path / "custom.md").read_text()
    assert "name: custom" in content


def test_write_skill_template_round_trips_through_registry(tmp_path: Path) -> None:
    from mcp_methods import write_skill_template

    manifest = tmp_path / "test_mcp.yaml"
    manifest.write_text("name: t\nskills: true\n")
    skills_dir = tmp_path / "test_mcp.skills"
    write_skill_template(skills_dir, "custom_method", "Project-layer skill body.")

    reg = SkillRegistry.from_manifest(str(manifest), include_bundled=False)
    skill = reg.get("custom_method")
    assert skill is not None
    assert skill.description == "Project-layer skill body."
    assert skill.provenance == "project"


def test_write_skill_template_rejects_empty_name(tmp_path: Path) -> None:
    from mcp_methods import write_skill_template

    with pytest.raises(ValueError, match="name must not be empty"):
        write_skill_template(tmp_path, "", "A description.")


def test_write_skill_template_rejects_empty_description(tmp_path: Path) -> None:
    from mcp_methods import write_skill_template

    with pytest.raises(ValueError, match="description must not be empty"):
        write_skill_template(tmp_path, "custom", "   ")


def test_write_skill_template_refuses_to_overwrite(tmp_path: Path) -> None:
    from mcp_methods import write_skill_template

    (tmp_path / "custom.md").write_text("existing content")
    with pytest.raises(ValueError, match="already exists"):
        write_skill_template(tmp_path, "custom", "Description.")
    # Original content preserved.
    assert (tmp_path / "custom.md").read_text() == "existing content"
