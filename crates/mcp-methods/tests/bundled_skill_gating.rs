//! End-to-end gate on the two optional bundled skills.
//!
//! `github_issues` registers only when the manifest opts in with
//! `builtins.github: true` *and* a token is reachable; `repo_management`
//! only in workspace mode. Both bundled SKILL.md files declare
//! `applies_when: { tool_registered: <self> }`, so a server without the
//! tool must not advertise the methodology — neither on `prompts/list`
//! nor injected into some other tool's description.

#![cfg(feature = "server")]

use std::collections::HashSet;
use std::sync::Arc;

use mcp_methods::server::{serve_prompts, McpServer, ServerOptions, SkillRegistry};
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::model::{CallToolResult, ContentBlock, Tool};

/// Manifest that enables the bundled skill layer and nothing else.
fn bundled_skills_manifest() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let yaml = dir.path().join("test_mcp.yaml");
    std::fs::write(&yaml, "name: t\nskills:\n  - true\n").unwrap();
    (dir, yaml)
}

fn stub_tool(name: &'static str) -> ToolRoute<McpServer> {
    ToolRoute::new_dyn(
        Tool::new(
            name,
            "Stub tool for the gating test.",
            Arc::new(Default::default()),
        ),
        |_context| {
            Box::pin(async { Ok(CallToolResult::success(vec![ContentBlock::text("ok")]).into()) })
        },
    )
}

fn prompt_names(server: &mut McpServer) -> HashSet<String> {
    server
        .prompt_router_mut()
        .list_all()
        .into_iter()
        .map(|p| p.name.to_string())
        .collect()
}

fn tool_description(server: &mut McpServer, name: &str) -> String {
    server
        .tool_router_mut()
        .list_all()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("tool `{name}` not registered"))
        .description
        .map(|d| d.to_string())
        .unwrap_or_default()
}

#[test]
fn optional_bundled_skills_absent_when_their_tools_are_unregistered() {
    // `ServerOptions::default()` is the `--graph`-style deployment:
    // `builtins.github` off (so no github_issues even with a token in
    // the environment) and no workspace (so repo_management is gated
    // out of the router).
    let (_dir, yaml) = bundled_skills_manifest();
    let registry = SkillRegistry::from_manifest(&yaml, true).unwrap();
    let mut server = McpServer::new(ServerOptions::default());
    serve_prompts(&registry, &mut server);

    let prompts = prompt_names(&mut server);
    assert!(
        !prompts.contains("github_issues"),
        "github_issues skill must not surface without the tool; got: {prompts:?}"
    );
    assert!(
        !prompts.contains("repo_management"),
        "repo_management skill must not surface without the tool; got: {prompts:?}"
    );

    // The ungated bundled skills still ship — the gate is targeted,
    // not a blanket suppression of the bundled layer.
    for always_on in ["grep", "read_source", "list_source"] {
        assert!(
            prompts.contains(always_on),
            "bundled skill `{always_on}` should still register; got: {prompts:?}"
        );
    }
}

#[test]
fn suppressed_skill_is_not_injected_into_a_referenced_tool() {
    // The repo_management skill lists `set_root_dir` in
    // `references_tools`. A local-workspace session registers
    // set_root_dir but not repo_management — the suppressed skill must
    // not leak into set_root_dir's description either.
    let (_dir, yaml) = bundled_skills_manifest();
    let registry = SkillRegistry::from_manifest(&yaml, true).unwrap();
    let mut server = McpServer::new(ServerOptions::default());
    server
        .tool_router_mut()
        .add_route(stub_tool("set_root_dir"));
    serve_prompts(&registry, &mut server);

    let desc = tool_description(&mut server, "set_root_dir");
    assert!(
        !desc.contains("<!-- mcp-skill:repo_management -->"),
        "suppressed skill must not auto-inject into a referenced tool; got: {desc}"
    );
}

#[test]
fn optional_bundled_skills_register_when_their_tools_are_present() {
    // Same registry, but the session has the tools — both skills come
    // back, and each injects into its own tool description.
    for name in ["github_issues", "repo_management"] {
        let (_dir, yaml) = bundled_skills_manifest();
        let registry = SkillRegistry::from_manifest(&yaml, true).unwrap();
        let mut server = McpServer::new(ServerOptions::default());
        server.tool_router_mut().add_route(stub_tool(name));
        serve_prompts(&registry, &mut server);

        assert!(
            prompt_names(&mut server).contains(name),
            "bundled skill `{name}` must register when `{name}` is in the catalogue"
        );
        let desc = tool_description(&mut server, name);
        assert!(
            desc.contains(&format!("<!-- mcp-skill:{name} -->")),
            "active skill `{name}` should inject into its own tool; got: {desc}"
        );
    }
}
