"""Shared helpers for the mcp-methods test suite.

`workspace_binary` is the single place a test resolves a built Cargo
artifact, and it deliberately does **not** prefer a build profile.
"Release if present, else debug" is a stale-artifact bug wearing a
default: `make bundle-bin` (and therefore `make dev-with-bin`) runs
`cargo build --release -p mcp-server`, so once a release artifact exists
it permanently shadows every later `cargo build -p mcp-server`, and the
suite then exercises old code while reporting green.

Two properties replace the preference:

* **newest-of-profile** — whichever artifact was built last wins,
  regardless of profile;
* **staleness skip** — an artifact older than the workspace manifest is
  not silently tested. The suite skips with the rebuild command in the
  reason, so "tested stale code" renders as *not attempted* rather than
  as *green*.
"""

from __future__ import annotations

from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

#: Profiles searched, in no order of preference — mtime decides.
BUILD_PROFILES = ("release", "debug")


def workspace_binary(
    name: str,
    *,
    repo_root: Path | None = None,
    profiles: tuple[str, ...] = BUILD_PROFILES,
    rebuild_cmd: str | None = None,
) -> tuple[Path | None, str | None]:
    """Resolve a Cargo-built binary as ``(path, skip_reason)``.

    ``skip_reason`` is ``None`` when the returned path is safe to run.
    It is a ready-to-use pytest skip reason — including the rebuild
    command — when nothing is built, or when the newest artifact predates
    the root ``Cargo.toml`` and would therefore test stale code.
    """
    root = REPO_ROOT if repo_root is None else repo_root
    manifest = root / "Cargo.toml"
    cmd = rebuild_cmd or f"cargo build -p {name}"

    candidates = [root / "target" / profile / name for profile in profiles]
    built = [p for p in candidates if p.exists()]
    if not built:
        locations = " or ".join(str(p) for p in candidates)
        return None, f"{name} binary not built (missing at {locations}). Build with `{cmd}`."

    chosen = max(built, key=lambda p: p.stat().st_mtime)
    if manifest.is_file() and chosen.stat().st_mtime < manifest.stat().st_mtime:
        return chosen, (
            f"{name} binary at {chosen} predates {manifest} and would test stale code. "
            f"Rebuild with `{cmd}`."
        )
    return chosen, None
