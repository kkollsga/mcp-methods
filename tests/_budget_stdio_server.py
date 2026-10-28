"""stdio MCPServer for tests/test_response_budget.py: one big-output tool."""

import json

from mcp.server.mcpserver import MCPServer
from mcp_methods.fastmcp import install_response_budget

app = MCPServer("budget-stdio")
install_response_budget(app)
DATA = json.dumps([{"id": i, "text": "x" * 400} for i in range(120)])


@app.tool()
def big() -> str:
    return DATA


if __name__ == "__main__":
    app.run("stdio")
