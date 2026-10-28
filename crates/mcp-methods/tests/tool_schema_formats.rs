//! Every schema a client can fetch through `tools/list` uses only the
//! `format` values JSON Schema 2020-12 defines. schemars labels Rust integer
//! types `uint`, `uint64`, `int64` and so on, which no validator knows:
//! Claude Code logs `unknown format "uint" ignored` for each, and a strict
//! Ajv rejects the schema outright.
#![cfg(feature = "server")]

use std::time::Duration;

use mcp_methods::server::stdio::StdioTransport;
use mcp_methods::server::{McpServer, ServerOptions};
use rmcp::ServiceExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The `format` values of the JSON Schema 2020-12 format vocabulary.
const STANDARD_FORMATS: &[&str] = &[
    "date-time",
    "date",
    "time",
    "duration",
    "email",
    "idn-email",
    "hostname",
    "idn-hostname",
    "ipv4",
    "ipv6",
    "uri",
    "uri-reference",
    "iri",
    "iri-reference",
    "uuid",
    "uri-template",
    "json-pointer",
    "relative-json-pointer",
    "regex",
];

#[derive(Default, Deserialize, schemars::JsonSchema)]
#[allow(dead_code)] // fields exist for their schema only
struct Numbers {
    /// unsigned, signed and narrow integers, a float and a date
    a: u64,
    b: i64,
    c: u8,
    d: Option<usize>,
    e: f64,
    f: Vec<u32>,
}

/// Every `format` string that is not standard, with its JSON path.
fn offenders(value: &Value, path: &str, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                match (key.as_str(), child) {
                    ("format", Value::String(format))
                        if !STANDARD_FORMATS.contains(&format.as_str()) =>
                    {
                        out.push(format!("{path}/format = {format}"));
                    }
                    _ => offenders(child, &format!("{path}/{key}"), out),
                }
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                offenders(child, &format!("{path}/{i}"), out);
            }
        }
        _ => {}
    }
}

async fn list_tools(server: McpServer) -> Vec<Value> {
    let (a, b) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        let (read, write) = tokio::io::split(a);
        let service = server
            .serve(StdioTransport::from_io(read, write))
            .await
            .unwrap();
        let _ = service.waiting().await;
    });
    let (read, mut write) = tokio::io::split(b);
    let mut read = BufReader::new(read);
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta": meta}});
    write
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    loop {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(10), read.read_line(&mut line))
            .await
            .expect("tools/list answered")
            .unwrap();
        assert!(n > 0, "server closed");
        let frame: Value = serde_json::from_str(&line).unwrap();
        if frame["id"] == 1 {
            return frame["result"]["tools"].as_array().unwrap().clone();
        }
    }
}

#[tokio::test]
async fn tool_schemas_carry_only_standard_formats() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = McpServer::new(
        ServerOptions::default()
            .with_static_source_roots(vec![dir.path().to_string_lossy().into_owned()]),
    );
    server.register_typed_tool::<Numbers, _>("numbers", "integers", |_| String::new());
    // A consumer's own route with a schemars-generated output schema, the
    // shape kglite's `cypher_query` has.
    let output = serde_json::to_value(schemars::schema_for!(Numbers))
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
    let raw = rmcp::model::Tool::new(
        "raw_numbers",
        "raw",
        std::sync::Arc::new(Default::default()),
    )
    .with_raw_output_schema(std::sync::Arc::new(output));
    server
        .tool_router_mut()
        .add_route(rmcp::handler::server::router::tool::ToolRoute::new_dyn(
            raw,
            |_ctx| Box::pin(async { Ok(rmcp::model::CallToolResult::success(vec![]).into()) }),
        ));
    let tools = list_tools(server).await;
    assert!(
        tools.iter().any(|t| t["name"] == "numbers"),
        "typed tool listed"
    );
    assert!(
        tools.iter().any(|t| t["name"] == "read_source"),
        "builtins listed: {:?}",
        tools.iter().map(|t| &t["name"]).collect::<Vec<_>>()
    );
    let mut bad = Vec::new();
    for tool in &tools {
        for key in ["inputSchema", "outputSchema"] {
            offenders(
                &tool[key],
                &format!("{}/{key}", tool["name"].as_str().unwrap()),
                &mut bad,
            );
        }
    }
    assert!(bad.is_empty(), "non-standard formats:\n{}", bad.join("\n"));
    let raw = tools.iter().find(|t| t["name"] == "raw_numbers").unwrap();
    assert!(raw["outputSchema"].is_object(), "output schema kept: {raw}");
    // The integer stays an integer, and an unsigned one stays non-negative.
    let numbers = tools.iter().find(|t| t["name"] == "numbers").unwrap();
    let a = &numbers["inputSchema"]["properties"]["a"];
    assert_eq!(a["type"], "integer", "{a}");
    assert_eq!(a["minimum"], 0, "{a}");
}
