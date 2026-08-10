"""Tests for `tests/conftest.py::workspace_binary`.

These live outside `test_mcp_server_smoke.py` on purpose: that module is
skipped wholesale when no binary is built, and a resolver test that can
be skipped by the very condition it is checking is not a check.

The first test is the regression the resolver exists for. Against the
previous rule — ``BINARY = _RELEASE_BIN if _RELEASE_BIN.exists() else
_DEBUG_BIN`` — it fails: a stale `target/release/mcp-server` left behind
by `make bundle-bin` shadows a freshly built `target/debug/mcp-server`
for the whole smoke suite.
"""

from __future__ import annotations

import os
from pathlib import Path

from tests.conftest import workspace_binary

STALE = 1_754_000_000.0  # older
MANIFEST_TIME = 1_754_500_000.0
FRESH = 1_755_000_000.0  # newer


def _make_tree(root: Path, *, release: float | None, debug: float | None) -> None:
    (root / "Cargo.toml").write_text("[workspace.package]\n")
    os.utime(root / "Cargo.toml", (MANIFEST_TIME, MANIFEST_TIME))
    for profile, mtime in (("release", release), ("debug", debug)):
        if mtime is None:
            continue
        d = root / "target" / profile
        d.mkdir(parents=True)
        binary = d / "mcp-server"
        binary.write_bytes(b"")
        os.utime(binary, (mtime, mtime))


def test_newer_debug_wins_over_stale_release(tmp_path: Path) -> None:
    """The defect: newest-of-profile, never 'prefer release'."""
    _make_tree(tmp_path, release=STALE, debug=FRESH)
    chosen, skip = workspace_binary("mcp-server", repo_root=tmp_path)
    assert chosen == tmp_path / "target" / "debug" / "mcp-server"
    assert skip is None


def test_newer_release_wins_over_stale_debug(tmp_path: Path) -> None:
    """Symmetric: the rule is mtime, not the other profile name."""
    _make_tree(tmp_path, release=FRESH, debug=STALE)
    chosen, skip = workspace_binary("mcp-server", repo_root=tmp_path)
    assert chosen == tmp_path / "target" / "release" / "mcp-server"
    assert skip is None


def test_only_profile_present_is_used(tmp_path: Path) -> None:
    _make_tree(tmp_path, release=None, debug=FRESH)
    chosen, skip = workspace_binary("mcp-server", repo_root=tmp_path)
    assert chosen == tmp_path / "target" / "debug" / "mcp-server"
    assert skip is None


def test_artifact_older_than_manifest_skips_with_rebuild_command(tmp_path: Path) -> None:
    """Stale code is skipped visibly, not tested silently (R10 corollary)."""
    _make_tree(tmp_path, release=STALE, debug=STALE)
    chosen, skip = workspace_binary(
        "mcp-server", repo_root=tmp_path, rebuild_cmd="cargo build -p mcp-server"
    )
    assert chosen is not None
    assert skip is not None
    assert "predates" in skip
    assert "cargo build -p mcp-server" in skip


def test_nothing_built_skips_with_both_locations(tmp_path: Path) -> None:
    _make_tree(tmp_path, release=None, debug=None)
    chosen, skip = workspace_binary("mcp-server", repo_root=tmp_path)
    assert chosen is None
    assert skip is not None
    assert "target/release/mcp-server" in skip
    assert "target/debug/mcp-server" in skip
