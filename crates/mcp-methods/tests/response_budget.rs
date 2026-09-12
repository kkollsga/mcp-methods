//! Real JSON-RPC calls exercise discovery and the final dispatch boundary.
#![cfg(feature = "server")]
use mcp_methods::server::{McpServer, ServerOptions};
use rmcp::ServiceExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

struct Client {
    read: BufReader<ReadHalf<DuplexStream>>,
    write: WriteHalf<DuplexStream>,
    id: usize,
}
impl Client {
    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let message = json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params});
        self.write
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.read.read_line(&mut line),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(n > 0, "server disconnected");
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame["id"] == self.id {
                return frame;
            }
        }
    }
    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name":name,"arguments":arguments}))
            .await
    }
}

async fn boot(server: McpServer) -> Client {
    let (a, b) = tokio::io::duplex(1024 * 1024);
    tokio::spawn(async move {
        let service = server.serve(a).await.unwrap();
        let _ = service.waiting().await;
    });
    let (read, write) = tokio::io::split(b);
    let mut client = Client {
        read: BufReader::new(read),
        write,
        id: 0,
    };
    let init = client.request("initialize", json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"same-name","version":"1"}})).await;
    assert!(init.get("error").is_none(), "{init}");
    client
        .write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    client
}

fn preview(frame: &Value) -> Value {
    serde_json::from_str::<Value>(frame["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
        ["response_budget"]
        .clone()
}

#[derive(Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Args {
    value: String,
}

#[tokio::test]
async fn defaults_overrides_followups_and_session_isolation() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let data = json!((0..120)
        .map(|i| json!({"id":i,"target":"execute_mut","text":"界\\\"".repeat(500)}))
        .collect::<Vec<_>>())
    .to_string();
    let output = data.clone();
    let mut server = McpServer::new(ServerOptions::default());
    server.register_typed_tool("mutate", "Changes state once", move |_: Args| {
        counter.fetch_add(1, Ordering::SeqCst);
        output.clone()
    });
    let other = server.clone();
    let mut client = boot(server).await;
    let mut outsider = boot(other).await;
    let listing = client.request("tools/list", json!({})).await;
    let tool = listing["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "mutate")
        .unwrap();
    assert_eq!(
        tool["inputSchema"]["properties"]["_response"]["properties"]["mode"]["default"],
        "bounded"
    );
    let invalid = client
        .call("mutate", json!({"value":"x","_response":{"max_bytes":1}}))
        .await;
    assert!(invalid.get("error").is_some());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let result = client.call("mutate", json!({"value":"x"})).await;
    assert!(serde_json::to_vec(&result["result"]).unwrap().len() <= 16384);
    let p = preview(&result);
    assert_eq!(p["preview"]["total"], 120);
    let action = &p["next"]["full_result"];
    let denied = outsider
        .call(
            action["name"].as_str().unwrap(),
            action["arguments"].clone(),
        )
        .await;
    assert!(denied.get("error").is_some());
    let expanded = client
        .call(
            action["name"].as_str().unwrap(),
            action["arguments"].clone(),
        )
        .await;
    assert_eq!(expanded["result"]["content"][0]["text"], data);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let page = &p["next"]["page"];
    let next = client
        .call(page["name"].as_str().unwrap(), page["arguments"].clone())
        .await;
    assert!(preview(&next)["preview"]["offset"].as_u64().unwrap() > 0);
    let selected = client
        .call(
            "expand_response",
            json!({"result_id":p["result_id"],"path":"/0/id","response":{"mode":"full"}}),
        )
        .await;
    assert_eq!(selected["result"]["content"][0]["text"], "0");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let full = client
        .call("mutate", json!({"value":"x","_response":{"mode":"full"}}))
        .await;
    assert_eq!(full["result"]["content"][0]["text"], data);
    let larger = client
        .call(
            "mutate",
            json!({"value":"x","_response":{"max_bytes":32768}}),
        )
        .await;
    assert_eq!(preview(&larger)["max_bytes"], 32768);
    let again = client.call("mutate", json!({"value":"x"})).await;
    assert_eq!(preview(&again)["max_bytes"], 16384);
}

#[tokio::test]
async fn static_tools_errors_and_custom_routes_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("large.txt"), "a source line\n".repeat(4000)).unwrap();
    let mut server = McpServer::new(
        ServerOptions::default().with_static_source_roots(vec![dir
            .path()
            .to_str()
            .unwrap()
            .into()]),
    );
    server.register_typed_tool_fallible("fail", "fails", |_: Args| {
        Err("Error: failed\n".repeat(5000))
    });
    let custom = rmcp::model::Tool::new(
        "custom",
        "structured",
        Arc::new(json!({"type":"object"}).as_object().unwrap().clone()),
    );
    server.tool_router_mut().add_route(rmcp::handler::server::router::tool::ToolRoute::new_dyn(custom,
        |_| Box::pin(async { Ok(rmcp::model::CallToolResult::structured(json!({"rows": (0..2000).collect::<Vec<_>>(), "warning":"Read-side coverage unknown", "big":"z".repeat(50000)})).into()) })));
    let mut client = boot(server).await;
    for (name, args) in [
        ("read_source", json!({"file_path":"large.txt"})),
        ("fail", json!({"value":"x"})),
        ("custom", json!({})),
    ] {
        let result = client.call(name, args).await;
        assert!(result.get("error").is_none(), "{result}");
        assert!(serde_json::to_vec(&result["result"]).unwrap().len() <= 16384);
        assert_eq!(result["result"]["isError"], name == "fail");
        let p = preview(&result);
        let full = &p["next"]["full_result"];
        let recovered = client
            .call(full["name"].as_str().unwrap(), full["arguments"].clone())
            .await;
        if name == "custom" {
            assert_eq!(
                recovered["result"]["structuredContent"]["warning"],
                "Read-side coverage unknown"
            );
        }
    }
}

#[tokio::test]
async fn domain_guidance_and_colliding_names_remain_usable() {
    #[derive(Default, Deserialize, Serialize, schemars::JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Collision {
        #[serde(rename = "_response")]
        domain: String,
    }
    let mut server = McpServer::new(ServerOptions::default());
    server.register_typed_tool(
        "expand_response",
        "Existing application tool",
        |args: Collision| args.domain,
    );
    server.register_typed_tool("read_side", "Read-side evidence", |_: Args| {
        "read-side caller".to_string()
    });
    server.register_typed_tool("query", "Query", |_: Args| json!({"rows":(0..120).map(|i|json!({"id":i,"target":"execute_mut","body":"x".repeat(1000)})).collect::<Vec<_>>()}).to_string());
    let server = server.with_response_preview_hook(Arc::new(|name, _, _| {
        if name == "query" { json!({"summary":"Only execute_mut rows returned; a shared LIMIT excluded read-side coverage.",
            "next_query":{"name":"read_side","arguments":{"value":"execute_read"}}}) }
        else { Value::Null }
    }));
    let mut client = boot(server).await;
    let listing = client.request("tools/list", json!({})).await;
    let tool = listing["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "expand_response")
        .unwrap();
    assert!(tool["inputSchema"]["properties"]
        .get("_response_")
        .is_some());
    let normal = client
        .call(
            "expand_response",
            json!({"_response":"domain value","_response_":{"mode":"full"}}),
        )
        .await;
    assert_eq!(normal["result"]["content"][0]["text"], "domain value");
    let result = client.call("query", json!({"value":"both symbols"})).await;
    let p = preview(&result);
    let hint = &p["domain_guidance"]["value"];
    assert!(hint["summary"].as_str().unwrap().contains("LIMIT"));
    let next = &hint["next_query"];
    let evidence = client
        .call(next["name"].as_str().unwrap(), next["arguments"].clone())
        .await;
    assert_eq!(evidence["result"]["content"][0]["text"], "read-side caller");
    assert_eq!(p["next"]["full_result"]["name"], "expand_response_");
    let full = client
        .call(
            "expand_response_",
            p["next"]["full_result"]["arguments"].clone(),
        )
        .await;
    assert!(full["result"]["_meta"]["mcp_methods/preview"]["summary"]
        .as_str()
        .unwrap()
        .contains("LIMIT"));
    let mut huge_budget = p["next"]["full_result"]["arguments"].clone();
    huge_budget["response"] = json!({"max_bytes":usize::MAX});
    let expanded = client.call("expand_response_", huge_budget).await;
    assert!(expanded.get("error").is_none());
}
