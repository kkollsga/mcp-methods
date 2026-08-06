//! Compile-level contract for downstream crates that use the public rmcp
//! escape hatch instead of `register_typed_tool`.

use std::sync::Arc;

use mcp_methods::server::{McpServer, ServerOptions};
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::model::{CallToolResult, ContentBlock, Tool};

#[test]
fn downstream_rmcp_route_is_accepted_by_the_framework_router() {
    let mut server = McpServer::new(ServerOptions::default());
    let route = ToolRoute::new_dyn(
        Tool::new(
            "downstream_raw",
            "Compile the public rmcp route boundary.",
            Arc::new(Default::default()),
        ),
        |_context| {
            Box::pin(async { Ok(CallToolResult::success(vec![ContentBlock::text("ok")]).into()) })
        },
    );

    server.tool_router_mut().add_route(route);
    assert!(server.tool_router_mut().has_route("downstream_raw"));
}
