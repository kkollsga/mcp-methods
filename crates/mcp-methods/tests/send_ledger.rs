//! Send ledger: records match what the client received on the wire.
#![cfg(feature = "server")]
use mcp_methods::server::{McpServer, ServerOptions};
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

struct Client {
    read: BufReader<ReadHalf<DuplexStream>>,
    write: WriteHalf<DuplexStream>,
    id: usize,
    last_result_bytes: usize,
    init_bytes: usize,
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
                self.last_result_bytes = serde_json::to_vec(&frame["result"]).unwrap().len();
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
        last_result_bytes: 0,
        init_bytes: 0,
    };
    let init = client.request("initialize", json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"same-name","version":"1"}})).await;
    assert!(init.get("error").is_none(), "{init}");
    client.init_bytes = client.last_result_bytes;
    client
        .write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    client
}

fn ledger_lines(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn server_with(path: &std::path::Path) -> McpServer {
    let mut server = McpServer::new(ServerOptions {
        instructions: Some("be brief".into()),
        ..ServerOptions::default()
    })
    .with_send_ledger(path);
    server.register_typed_tool("big", "Returns a lot", |a: Args| "x".repeat(a.n));
    server
}

#[derive(Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Args {
    n: usize,
}

#[tokio::test]
async fn records_match_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.jsonl");
    let mut client = boot(server_with(&path)).await;
    let listing = client.request("tools/list", json!({})).await;
    let list_bytes = client.last_result_bytes;
    let small = client.call("big", json!({"n":10})).await;
    let small_bytes = client.last_result_bytes;
    let large = client.call("big", json!({"n":40000})).await;
    let large_bytes = client.last_result_bytes;
    let preview: Value =
        serde_json::from_str(large["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let expand_args = json!({"result_id": preview["response_budget"]["result_id"]});
    let expanded = client.call("expand_response", expand_args.clone()).await;
    assert!(expanded.get("error").is_none(), "{expanded}");
    let expand_bytes = client.last_result_bytes;
    assert_eq!(small["result"]["content"][0]["text"], "x".repeat(10));
    assert!(
        large["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .len()
            < 40000
    );

    let lines = ledger_lines(&path);
    let init = lines.iter().find(|l| l["method"] == "initialize").unwrap();
    assert_eq!(init["instructions_bytes"], 8);
    assert_eq!(init["total_bytes"], client.init_bytes);
    let list = lines.iter().find(|l| l["method"] == "tools/list").unwrap();
    assert_eq!(list["total_bytes"], list_bytes);
    let listed = listing["result"]["tools"].as_array().unwrap();
    let rows = list["tools"].as_array().unwrap();
    assert_eq!(rows.len(), listed.len());
    let big = rows.iter().find(|r| r["name"] == "big").unwrap();
    let wire = listed.iter().find(|t| t["name"] == "big").unwrap();
    assert_eq!(
        big["description_bytes"],
        serde_json::to_vec(&wire["description"]).unwrap().len()
    );
    assert_eq!(
        big["input_schema_bytes"],
        serde_json::to_vec(&wire["inputSchema"]).unwrap().len()
    );
    assert_eq!(
        big["output_schema_bytes"],
        wire.get("outputSchema")
            .map_or(0, |o| serde_json::to_vec(o).unwrap().len())
    );

    let calls: Vec<_> = lines
        .iter()
        .filter(|l| l["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0]["tool"], "big");
    assert_eq!(calls[0]["budget"], "inline");
    assert_eq!(calls[0]["total_bytes"], small_bytes);
    assert_eq!(calls[0]["text_bytes"], 10);
    assert_eq!(calls[0]["is_error"], false);
    assert_eq!(calls[0]["other_blocks"], 0);
    assert_eq!(calls[1]["budget"], "preview");
    assert_eq!(calls[1]["total_bytes"], large_bytes);
    assert_eq!(
        calls[1]["arguments_bytes"],
        serde_json::to_vec(&json!({"n":40000})).unwrap().len()
    );
    assert!(calls[1]["text_bytes"].as_u64().unwrap() < 40000);
    // `expand_response` records the arguments actually sent, not the
    // parsed struct with its defaults re-serialized.
    assert_eq!(calls[2]["tool"], "expand_response");
    assert_eq!(calls[2]["budget"], "exempt");
    assert_eq!(
        calls[2]["arguments_bytes"],
        serde_json::to_vec(&expand_args).unwrap().len()
    );
    assert_eq!(calls[2]["total_bytes"], expand_bytes);
    // No argument values or result content in the file.
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("xxxxxxxx") && !raw.contains("40000"));
}

#[tokio::test]
async fn unwritable_path_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = boot(server_with(dir.path())).await;
    let ok = client.call("big", json!({"n":10})).await;
    assert_eq!(ok["result"]["content"][0]["text"], "x".repeat(10));
    let listing = client.request("tools/list", json!({})).await;
    assert!(listing.get("error").is_none());
}

#[tokio::test]
async fn disabled_by_default_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = McpServer::new(ServerOptions::default());
    server.register_typed_tool("big", "Returns a lot", |a: Args| "x".repeat(a.n));
    let mut client = boot(server).await;
    client.request("tools/list", json!({})).await;
    client.call("big", json!({"n":10})).await;
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
