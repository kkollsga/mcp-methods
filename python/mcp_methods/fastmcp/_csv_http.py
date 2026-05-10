"""Tiny CORS-enabled HTTP server for serving CSV exports to the agent's
browser-side context.

Used in tandem with `register_cypher_query(..., csv_dir=...)`: the cypher
tool drops a CSV into `csv_dir`, this server serves it to anyone who can
reach `bind:port` with `Access-Control-Allow-Origin: *` so a Claude.ai
browser tab can load the file directly via fetch().
"""

from __future__ import annotations

import threading
from http.server import HTTPServer, SimpleHTTPRequestHandler
from pathlib import Path


class _CorsHandler(SimpleHTTPRequestHandler):
    """SimpleHTTPRequestHandler with CORS headers added to every response."""

    def end_headers(self) -> None:
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "GET, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "*")
        super().end_headers()

    def do_OPTIONS(self) -> None:  # noqa: N802 — http.server naming convention
        self.send_response(204)
        self.end_headers()

    def log_message(self, fmt: str, *args) -> None:
        # Quiet — leave stdout for the MCP server's transport.
        pass


def serve_csv_via_http(
    directory: str | Path, *, port: int = 0, bind: str = "127.0.0.1"
) -> tuple[HTTPServer, str]:
    """Start a background HTTP server in a daemon thread.

    Returns `(server, base_url)` so the caller can compose URLs like
    `f"{base_url}/{filename}"`. The server runs forever in a daemon
    thread; call `server.shutdown()` to stop it explicitly.
    """
    directory = Path(directory).resolve()
    directory.mkdir(parents=True, exist_ok=True)

    def handler_factory(*args, **kwargs):
        # SimpleHTTPRequestHandler needs `directory=` to chroot.
        return _CorsHandler(*args, directory=str(directory), **kwargs)

    server = HTTPServer((bind, port), handler_factory)
    actual_port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    base_url = f"http://{bind}:{actual_port}"
    return server, base_url
