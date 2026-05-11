"""Console-script launcher for the bundled `mcp-server` binary.

`pip install mcp-methods` installs `mcp-server` on PATH as a Python
console-script entry point that resolves to `main()` below. We then
exec the native Rust binary shipped under `mcp_methods/_bin/`, so the
operator gets the same UX as a `cargo install`-built binary without
having to install Rust.

The native binary is built by maturin during the wheel build (via the
`[[bin]]` target in the root Cargo.toml, gated on the `server`
feature). The build workflow copies it into this `_bin/` directory
before maturin packages the Python tree.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

_BINARY_NAME = "mcp-server.exe" if sys.platform == "win32" else "mcp-server"


def _binary_path() -> Path:
    return Path(__file__).resolve().parent / "_bin" / _BINARY_NAME


def main() -> None:
    binary = _binary_path()
    if not binary.exists():
        sys.stderr.write(
            f"mcp-server: bundled binary not found at {binary}.\n"
            f"The wheel may have been built without the `server` feature, "
            f"or the build workflow may not have copied the binary into the "
            f"package. Build from source with `cargo install --path . "
            f"--features server` if you have a Rust toolchain.\n"
        )
        sys.exit(1)
    if sys.platform == "win32":
        # `os.execvp` doesn't replace the current process on Windows the
        # same way it does on POSIX. Spawn + wait + propagate exit code.
        import subprocess

        result = subprocess.run([str(binary), *sys.argv[1:]])
        sys.exit(result.returncode)
    os.execvp(str(binary), [str(binary), *sys.argv[1:]])


if __name__ == "__main__":  # pragma: no cover — invoked via entry point
    main()
