"""End-to-end smoke tests for the `mcp-server` binary.

Drives the framework binary over JSON-RPC stdio (the way Claude Desktop
/ Cursor do) and exercises every tool the framework registers — source
navigation, GitHub access, workspace modes, .env discovery, bare boot.
Catches boot failures, missing tools, and per-tool argument-shape
regressions before users do.

Tests are skipped when the binary isn't built. Build it with::

    cargo build -p mcp-server --release      # or --no-default-features

Mirrored from kglite's `test_mcp_server_smoke.py` with kglite/Cypher/
graph-specific classes (TestGraphMode, TestReadCodeSource, TestYamlManifest)
dropped — those live in the kglite repo and exercise kglite's shim.
What's kept covers pure framework behaviour: source tools, GitHub
tools, env-file walk-up, local-workspace mode, bare boot.

GitHub-token-gated tests are exercised when ``GITHUB_TOKEN`` is set,
OR when a sibling ``.env`` exists with one (the same walk-up the
binary does at boot).
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import threading
import time
from pathlib import Path
from typing import Any

import pytest

from tests.conftest import REPO_ROOT, workspace_binary

# Newest-of-profile, never "prefer release" — a stale `target/release/
# mcp-server` from `make bundle-bin` would otherwise shadow a fresh
# `cargo build -p mcp-server` and this suite would test old code while
# reporting green. See `tests/conftest.py::workspace_binary`.
BINARY, _SKIP_REASON = workspace_binary(
    "mcp-server", rebuild_cmd="cargo build -p mcp-server --release"
)


pytestmark = pytest.mark.skipif(_SKIP_REASON is not None, reason=_SKIP_REASON or "")


def _discover_github_token() -> str | None:
    """Look for a GitHub token in env, then fall back to a `.env` in the
    repo root (same walk-up the binary itself does at boot)."""
    for var in ("GITHUB_TOKEN", "GH_TOKEN"):
        v = os.environ.get(var)
        if v:
            return v
    env_path = REPO_ROOT / ".env"
    if env_path.is_file():
        for line in env_path.read_text().splitlines():
            line = line.strip()
            if line.startswith("#") or "=" not in line:
                continue
            key, _, value = line.partition("=")
            if key.strip() in ("GITHUB_TOKEN", "GH_TOKEN"):
                return value.strip().strip("\"'")
    return None


GITHUB_TOKEN = _discover_github_token()


# ── JSON-RPC stdio client ─────────────────────────────────────────────────


class McpClient:
    """Minimal JSON-RPC 2.0 / NDJSON client for an MCP stdio server."""

    def __init__(self, proc: subprocess.Popen[bytes]) -> None:
        self.proc = proc
        self._next_id = 0
        # Drain stderr in the background so the subprocess buffer doesn't fill
        # up if the server logs verbosely. We don't assert against stderr — just
        # collect it for diagnostics on failure.
        self._stderr_lines: list[str] = []
        self._stderr_thread = threading.Thread(target=self._drain_stderr, daemon=True)
        self._stderr_thread.start()

    def _drain_stderr(self) -> None:
        assert self.proc.stderr is not None
        for line in iter(self.proc.stderr.readline, b""):
            self._stderr_lines.append(line.decode("utf-8", errors="replace").rstrip())

    def _allocate_id(self) -> int:
        self._next_id += 1
        return self._next_id

    def _send(self, payload: dict[str, Any]) -> None:
        line = (json.dumps(payload) + "\n").encode("utf-8")
        assert self.proc.stdin is not None
        self.proc.stdin.write(line)
        self.proc.stdin.flush()

    def _recv(self, expected_id: int, timeout_s: float = 30.0) -> dict[str, Any]:
        """Read NDJSON responses from stdout until one matching `expected_id`
        comes back. Notifications and other ids are buffered/ignored."""
        deadline = time.monotonic() + timeout_s
        assert self.proc.stdout is not None
        while time.monotonic() < deadline:
            line = self.proc.stdout.readline()
            if not line:
                stderr_tail = "\n".join(self._stderr_lines[-20:])
                raise RuntimeError(f"Server exited unexpectedly. Last stderr:\n{stderr_tail}")
            try:
                msg = json.loads(line.decode("utf-8"))
            except json.JSONDecodeError:
                continue
            if msg.get("id") == expected_id:
                return msg
        raise TimeoutError(f"Timed out waiting for response id={expected_id}")

    def initialize(self) -> dict[str, Any]:
        rid = self._allocate_id()
        self._send(
            {
                "jsonrpc": "2.0",
                "id": rid,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "mcp-server-smoke-test", "version": "0"},
                },
            }
        )
        resp = self._recv(rid)
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        return resp

    def list_tools(self) -> list[dict[str, Any]]:
        rid = self._allocate_id()
        self._send({"jsonrpc": "2.0", "id": rid, "method": "tools/list"})
        resp = self._recv(rid)
        if "error" in resp:
            raise RuntimeError(f"tools/list errored: {resp['error']}")
        return resp["result"]["tools"]

    def call_tool(self, name: str, arguments: dict[str, Any] | None = None) -> dict[str, Any]:
        rid = self._allocate_id()
        self._send(
            {
                "jsonrpc": "2.0",
                "id": rid,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments or {}},
            }
        )
        resp = self._recv(rid)
        if "error" in resp:
            raise RuntimeError(f"tools/call({name}) errored: {resp['error']}")
        return resp["result"]

    def shutdown(self) -> None:
        try:
            assert self.proc.stdin is not None
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=5)


def _spawn(
    args: list[str],
    cwd: Path | None = None,
    env_extra: dict[str, str] | None = None,
    env_remove: list[str] | None = None,
) -> McpClient:
    env = os.environ.copy()
    if env_remove:
        for key in env_remove:
            env.pop(key, None)
    if env_extra:
        env.update(env_extra)
    proc = subprocess.Popen(
        [str(BINARY), *args],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=str(cwd) if cwd else None,
        env=env,
    )
    client = McpClient(proc)
    client.initialize()
    return client


def _text_content(result: dict[str, Any]) -> str:
    """Extract the joined text from a tools/call result envelope."""
    parts = result.get("content", [])
    text_parts = [p["text"] for p in parts if p.get("type") == "text"]
    return "\n".join(text_parts)


# ── Source-root mode ─────────────────────────────────────────────────────


class TestSourceRootMode:
    """`--source-root <dir>` binds the source tools to a fixed directory."""

    @pytest.fixture
    def source_dir(self, tmp_path: Path) -> Path:
        d = tmp_path / "src"
        d.mkdir()
        (d / "hello.py").write_text(
            "def greet(name):\n"
            "    return f'Hello, {name}'\n"
            "\n"
            "def shout(name):\n"
            "    return greet(name).upper()\n"
        )
        (d / "README.md").write_text("# Sample\n\nA tiny demo.\n")
        sub = d / "sub"
        sub.mkdir()
        (sub / "nested.txt").write_text("nested file content\n")
        return d

    def test_lists_source_tools(self, source_dir: Path):
        client = _spawn(["--source-root", str(source_dir)])
        try:
            names = {t["name"] for t in client.list_tools()}
            assert "ping" in names
            assert "read_source" in names
            assert "grep" in names
            assert "list_source" in names
        finally:
            client.shutdown()

    def test_ping(self, source_dir: Path):
        client = _spawn(["--source-root", str(source_dir)])
        try:
            r = client.call_tool("ping")
            assert "pong" in _text_content(r).lower()
        finally:
            client.shutdown()

    def test_read_source(self, source_dir: Path):
        client = _spawn(["--source-root", str(source_dir)])
        try:
            r = client.call_tool("read_source", {"file_path": "hello.py"})
            text = _text_content(r)
            assert "def greet" in text and "def shout" in text
        finally:
            client.shutdown()

    def test_read_source_line_range(self, source_dir: Path):
        client = _spawn(["--source-root", str(source_dir)])
        try:
            r = client.call_tool(
                "read_source", {"file_path": "hello.py", "start_line": 1, "end_line": 2}
            )
            text = _text_content(r)
            assert "def greet" in text
            assert "def shout" not in text
        finally:
            client.shutdown()

    def test_read_source_grep(self, source_dir: Path):
        client = _spawn(["--source-root", str(source_dir)])
        try:
            r = client.call_tool("read_source", {"file_path": "hello.py", "grep": r"def\s+\w+"})
            text = _text_content(r)
            assert "def greet" in text and "def shout" in text
        finally:
            client.shutdown()

    def test_grep_across_files(self, source_dir: Path):
        client = _spawn(["--source-root", str(source_dir)])
        try:
            r = client.call_tool("grep", {"pattern": "Hello"})
            text = _text_content(r)
            assert "hello.py" in text
        finally:
            client.shutdown()

    def test_grep_glob_filter(self, source_dir: Path):
        client = _spawn(["--source-root", str(source_dir)])
        try:
            r = client.call_tool("grep", {"pattern": "demo", "glob": "*.md"})
            text = _text_content(r)
            assert "README.md" in text
            assert "hello.py" not in text
        finally:
            client.shutdown()

    def test_list_source(self, source_dir: Path):
        client = _spawn(["--source-root", str(source_dir)])
        try:
            r = client.call_tool("list_source", {"path": ".", "depth": 2})
            text = _text_content(r)
            assert "hello.py" in text
            assert "README.md" in text
            assert "sub" in text
        finally:
            client.shutdown()


# ── GitHub-token-gated tools ─────────────────────────────────────────────


class TestGithubTools:
    """`github_issues` / `github_api` register at boot only when
    `GITHUB_TOKEN` is reachable. The .env walk-up may leak a token from a
    parent directory; unauthorized tests use an isolated cwd above which
    no `.env` lives."""

    def test_unauthorized_hides_github_tools(self, tmp_path: Path):
        isolated_cwd = tmp_path / "no_env_here"
        isolated_cwd.mkdir()
        src = tmp_path / "src"
        src.mkdir()
        client = _spawn(
            ["--source-root", str(src)],
            cwd=isolated_cwd,
            env_remove=["GITHUB_TOKEN", "GH_TOKEN"],
        )
        try:
            names = {t["name"] for t in client.list_tools()}
            assert "github_issues" not in names, (
                "github_issues registered without a token — the .env walk-up "
                "may have found one in an unexpected location."
            )
            assert "github_api" not in names
        finally:
            client.shutdown()

    @pytest.mark.skipif(
        GITHUB_TOKEN is None,
        reason="No GITHUB_TOKEN reachable (env or sibling .env).",
    )
    def test_authorized_lists_github_tools(self, tmp_path: Path):
        src = tmp_path / "src"
        src.mkdir()
        client = _spawn(
            ["--source-root", str(src)],
            env_extra={"GITHUB_TOKEN": GITHUB_TOKEN or ""},
        )
        try:
            names = {t["name"] for t in client.list_tools()}
            assert "github_issues" in names
            assert "github_api" in names
        finally:
            client.shutdown()

    @pytest.mark.skipif(
        GITHUB_TOKEN is None,
        reason="No GITHUB_TOKEN reachable.",
    )
    def test_github_api_call(self, tmp_path: Path):
        """Live GitHub call against a stable public endpoint."""
        src = tmp_path / "src"
        src.mkdir()
        client = _spawn(
            ["--source-root", str(src)],
            env_extra={"GITHUB_TOKEN": GITHUB_TOKEN or ""},
        )
        try:
            r = client.call_tool("github_api", {"path": "users/octocat"})
            text = _text_content(r)
            assert "octocat" in text.lower()
        finally:
            client.shutdown()

    @pytest.mark.skipif(
        GITHUB_TOKEN is None,
        reason="No GITHUB_TOKEN reachable.",
    )
    def test_github_issues_search(self, tmp_path: Path):
        src = tmp_path / "src"
        src.mkdir()
        client = _spawn(
            ["--source-root", str(src)],
            env_extra={"GITHUB_TOKEN": GITHUB_TOKEN or ""},
        )
        try:
            r = client.call_tool(
                "github_issues",
                {"query": "bug", "repo_name": "rust-lang/rust", "limit": 3},
            )
            text = _text_content(r)
            assert text.strip(), "github_issues returned empty body"
            assert "error" not in text.lower()[:80]
        finally:
            client.shutdown()


# ── .env discovery ────────────────────────────────────────────────────────


class TestEnvFileLoading:
    """`load_env_for_mode` walks up from the mode dir looking for `.env`.
    Explicit `env_file:` YAML key overrides walk-up. Both paths must work."""

    def test_walk_up_from_source_root_finds_env(self, tmp_path: Path):
        outer = tmp_path / "outer"
        outer.mkdir()
        (outer / ".env").write_text("GITHUB_TOKEN=ghp_walkup_test_token_not_real\n")
        src = outer / "src"
        src.mkdir()
        client = _spawn(
            ["--source-root", str(src)],
            cwd=tmp_path,
            env_remove=["GITHUB_TOKEN", "GH_TOKEN"],
        )
        try:
            names = {t["name"] for t in client.list_tools()}
            assert "github_issues" in names, (
                "github_issues missing — .env walk-up from --source-root parent "
                "didn't fire. Tools listed: " + str(sorted(names))
            )
        finally:
            client.shutdown()

    def test_explicit_env_file_yaml_key(self, tmp_path: Path):
        env_dir = tmp_path / "stash"
        env_dir.mkdir()
        (env_dir / "my.env").write_text("GITHUB_TOKEN=ghp_explicit_test_token_not_real\n")
        manifest = tmp_path / "explicit_mcp.yaml"
        manifest.write_text("name: Explicit Env Test\nenv_file: stash/my.env\n")
        client = _spawn(
            ["--mcp-config", str(manifest)],
            cwd=tmp_path,
            env_remove=["GITHUB_TOKEN", "GH_TOKEN"],
        )
        try:
            names = {t["name"] for t in client.list_tools()}
            assert "github_issues" in names, (
                "explicit env_file: didn't load the token. Tools listed: " + str(sorted(names))
            )
        finally:
            client.shutdown()

    def test_existing_env_var_not_overwritten(self, tmp_path: Path):
        """`apply_env_file` must not overwrite an already-set env var."""
        src = tmp_path / "src_no_env"
        src.mkdir()
        client = _spawn(
            ["--source-root", str(src)],
            cwd=src,
            env_extra={"GITHUB_TOKEN": "ghp_via_env_not_real"},
        )
        try:
            names = {t["name"] for t in client.list_tools()}
            assert "github_issues" in names
        finally:
            client.shutdown()


# ── Local-workspace mode ─────────────────────────────────────────────────


class TestLocalWorkspace:
    """`workspace.kind: local` registers `set_root_dir` and binds the
    declared root as the source-root provider."""

    @pytest.fixture
    def local_workspace(self, tmp_path: Path) -> tuple[Path, Path]:
        ws = tmp_path / "workspace"
        ws.mkdir()
        (ws / "demo.py").write_text("print('hello')\n")
        manifest = tmp_path / "ws_mcp.yaml"
        manifest.write_text(f"name: Local WS Test\nworkspace:\n  kind: local\n  root: {ws}\n")
        return manifest, ws

    def test_set_root_dir_registered(self, local_workspace):
        manifest, _ws = local_workspace
        client = _spawn(["--mcp-config", str(manifest)])
        try:
            names = {t["name"] for t in client.list_tools()}
            assert "set_root_dir" in names
            assert "read_source" in names
        finally:
            client.shutdown()

    def test_read_source_via_workspace(self, local_workspace):
        manifest, _ws = local_workspace
        client = _spawn(["--mcp-config", str(manifest)])
        try:
            r = client.call_tool("read_source", {"file_path": "demo.py"})
            text = _text_content(r)
            assert "hello" in text
        finally:
            client.shutdown()


# ── Bare boot ────────────────────────────────────────────────────────────


class TestBareBoot:
    """No flags → still boots → `ping` is registered. Source / GitHub tools
    are conditional on configuration; bare mode has neither."""

    def test_boots_without_flags(self, tmp_path: Path):
        client = _spawn([], cwd=tmp_path, env_remove=["GITHUB_TOKEN", "GH_TOKEN"])
        try:
            names = {t["name"] for t in client.list_tools()}
            assert "ping" in names
            r = client.call_tool("ping")
            assert "pong" in _text_content(r).lower()
        finally:
            client.shutdown()

    def test_boots_with_minimal_manifest(self, tmp_path: Path):
        manifest = tmp_path / "bare_mcp.yaml"
        manifest.write_text("name: Bare Smoke Test\n")
        client = _spawn(
            ["--mcp-config", str(manifest)], cwd=tmp_path, env_remove=["GITHUB_TOKEN", "GH_TOKEN"]
        )
        try:
            names = {t["name"] for t in client.list_tools()}
            assert "ping" in names
        finally:
            client.shutdown()


# ── Cleanup safety: ensure no orphaned binaries ───────────────────────────


def teardown_module(_module):
    if shutil.which("pkill"):
        subprocess.run(
            ["pkill", "-f", str(BINARY)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
