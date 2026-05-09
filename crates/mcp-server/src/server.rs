//! Minimal MCP `ServerHandler` implementation.
//!
//! Phase 1 surface: server identity, instructions, capabilities, and a
//! single ``ping`` tool that confirms the framework is alive. Real tools
//! (cypher_query, read_source, github_issues, …) get added in later
//! phases as their owning crates / modules land.

// `tool_router` field is consumed by rmcp's macro-generated dispatch;
// the compiler doesn't see the read.
#![allow(dead_code)]

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::{Deserialize, Serialize};

use crate::manifest::Manifest;

/// Per-server runtime state shared by every tool dispatch.
#[derive(Debug, Clone, Default)]
pub struct ServerOptions {
    /// Server display name surfaced via initialize.
    pub name: Option<String>,
    /// Free-form text shown to the agent at session start.
    pub instructions: Option<String>,
}

impl ServerOptions {
    pub fn from_manifest(manifest: Option<&Manifest>, fallback_name: &str) -> Self {
        Self {
            name: manifest
                .and_then(|m| m.name.clone())
                .or_else(|| Some(fallback_name.to_string())),
            instructions: manifest.and_then(|m| m.instructions.clone()),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct PingArgs {
    /// Optional message to echo back. Defaults to "pong".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// MCP server backed by the rmcp framework.
///
/// The struct is cloned per request by rmcp's handler dispatch; keep it
/// cheap to clone (Arc anything heavy).
#[derive(Clone)]
pub struct McpServer {
    options: ServerOptions,
    tool_router: ToolRouter<McpServer>,
}

#[tool_router]
impl McpServer {
    pub fn new(options: ServerOptions) -> Self {
        Self {
            options,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Liveness probe — returns 'pong' (or echoes `message` if supplied). \
                          Use to confirm the server framework is wired correctly before \
                          relying on graph- or source-aware tools."
    )]
    async fn ping(
        &self,
        Parameters(args): Parameters<PingArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = args.message.unwrap_or_else(|| "pong".to_string());
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }
}

#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        let name = self
            .options
            .name
            .clone()
            .unwrap_or_else(|| "MCP Server".to_string());
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(name, env!("CARGO_PKG_VERSION")))
            .with_protocol_version(ProtocolVersion::V_2024_11_05);
        if let Some(text) = &self.options.instructions {
            info = info.with_instructions(text.clone());
        }
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_from_manifest_uses_name_when_set() {
        let opts = ServerOptions::from_manifest(None, "Fallback");
        assert_eq!(opts.name.as_deref(), Some("Fallback"));
    }

    #[test]
    fn server_constructs() {
        let _server = McpServer::new(ServerOptions::default());
    }
}
