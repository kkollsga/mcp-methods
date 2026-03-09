"""Tests for mcp_methods._utils."""

import os
import tempfile

from mcp_methods._utils import load_env, timed


def test_timed_appends_timing():
    @timed
    def greet(name: str) -> str:
        return f"hello {name}"

    result = greet("world")
    assert result.startswith("hello world")
    assert "⏱" in result
    assert "ms" in result


def test_timed_preserves_function_name():
    @timed
    def my_func() -> str:
        return "ok"

    assert my_func.__name__ == "my_func"


def test_load_env_sets_variables():
    with tempfile.NamedTemporaryFile(mode="w", suffix=".env", delete=False) as f:
        f.write("TEST_MCP_KEY=test_value\n")
        f.write("# comment line\n")
        f.write("\n")
        f.write("TEST_MCP_QUOTED='quoted_value'\n")
        f.flush()
        path = f.name

    try:
        # Clean up env first
        os.environ.pop("TEST_MCP_KEY", None)
        os.environ.pop("TEST_MCP_QUOTED", None)

        load_env(path)

        assert os.environ["TEST_MCP_KEY"] == "test_value"
        assert os.environ["TEST_MCP_QUOTED"] == "quoted_value"
    finally:
        os.unlink(path)
        os.environ.pop("TEST_MCP_KEY", None)
        os.environ.pop("TEST_MCP_QUOTED", None)


def test_load_env_does_not_overwrite():
    with tempfile.NamedTemporaryFile(mode="w", suffix=".env", delete=False) as f:
        f.write("TEST_MCP_EXISTING=new_value\n")
        f.flush()
        path = f.name

    try:
        os.environ["TEST_MCP_EXISTING"] = "original"
        load_env(path)
        assert os.environ["TEST_MCP_EXISTING"] == "original"
    finally:
        os.unlink(path)
        os.environ.pop("TEST_MCP_EXISTING", None)


def test_load_env_missing_file():
    load_env("/nonexistent/path/.env")  # should not raise
