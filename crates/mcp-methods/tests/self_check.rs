//! Self-update notice end to end: real JSON-RPC calls against a server
//! watching a temp file that the test then replaces.
#![cfg(feature = "server")]
use mcp_methods::server::{McpServer, SelfCheck, ServerOptions};
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::model::{CallToolResult, ContentBlock, Tool};
use rmcp::ServiceExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

const NOTICE: &str =
    "stale: demo 1.2.3 running, 9.9.9 installed — restart the MCP client to load it";

struct Client {
    read: BufReader<ReadHalf<DuplexStream>>,
    write: WriteHalf<DuplexStream>,
    id: usize,
    info: Value,
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
    /// The last text block of a call's result.
    async fn text(&mut self, name: &str, arguments: Value) -> String {
        let frame = self.call(name, arguments).await;
        frame["result"]["content"]
            .as_array()
            .unwrap_or_else(|| panic!("no content in {frame}"))
            .iter()
            .rev()
            .find_map(|b| b["text"].as_str())
            .unwrap()
            .to_string()
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
        info: Value::Null,
    };
    let init = client.request("initialize", json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"c","version":"1"}})).await;
    assert!(init.get("error").is_none(), "{init}");
    client.info = init["result"]["serverInfo"].clone();
    client
        .write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    client
}

#[derive(Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Args {
    #[serde(default)]
    value: String,
}

fn raw_route(name: &'static str, is_error: bool) -> ToolRoute<McpServer> {
    ToolRoute::new_dyn(
        Tool::new(name, "raw", Arc::new(Default::default())),
        move |_| {
            Box::pin(async move {
                let blocks = vec![ContentBlock::text("raw body")];
                Ok(if is_error {
                    CallToolResult::error(blocks)
                } else {
                    CallToolResult::success(blocks)
                }
                .into())
            })
        },
    )
}

fn big() -> String {
    json!((0..120)
        .map(|i| json!({"id":i,"text":"界\\\"".repeat(500)}))
        .collect::<Vec<_>>())
    .to_string()
}

struct Fixture {
    dir: tempfile::TempDir,
    client: Client,
}
impl Fixture {
    fn bin(&self) -> std::path::PathBuf {
        self.dir.path().join("bin")
    }
    /// Make the watched file differ from its snapshot.
    fn replace(&self) {
        std::fs::write(self.bin(), b"a different, longer build").unwrap();
    }
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bin"), b"v1").unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a.txt"), "hello\n").unwrap();
    let check = SelfCheck::new("demo", dir.path().join("bin"), "1.2.3")
        .with_installed_version(|| Some("9.9.9".to_string()));
    let opts = ServerOptions::default()
        .with_static_source_roots(vec![dir.path().join("src").to_str().unwrap().to_string()])
        .with_self_check(check);
    let mut server = McpServer::new(opts);
    server.register_typed_tool("typed", "typed", |_: Args| "typed body".to_string());
    server.register_typed_tool("big", "big", |_: Args| big());
    server.tool_router_mut().add_route(raw_route("raw", false));
    server
        .tool_router_mut()
        .add_route(raw_route("raw_err", true));
    let client = boot(server).await;
    Fixture { dir, client }
}

#[tokio::test]
async fn no_notice_until_the_binary_is_replaced() {
    let mut f = fixture().await;
    for (name, args) in [
        ("typed", json!({})),
        ("raw", json!({})),
        ("read_source", json!({"file_path":"a.txt"})),
    ] {
        let t = f.client.text(name, args).await;
        assert!(!t.contains("stale:"), "{name}: {t}");
    }
}

#[tokio::test]
async fn notice_lands_on_typed_builtin_and_raw_routes_once() {
    let mut f = fixture().await;
    f.replace();
    let t = f.client.text("typed", json!({})).await;
    assert_eq!(t, format!("typed body\n\n{NOTICE}"));
    let t = f
        .client
        .text("read_source", json!({"file_path":"a.txt"}))
        .await;
    assert!(t.ends_with(&format!("\n\n{NOTICE}")), "{t}");
    assert_eq!(t.matches("stale:").count(), 1);
    let t = f.client.text("raw", json!({})).await;
    assert_eq!(t, format!("raw body\n\n{NOTICE}"));
}

#[tokio::test]
async fn error_results_and_ping_carry_no_notice() {
    let mut f = fixture().await;
    f.replace();
    let err = f.client.call("raw_err", json!({})).await;
    assert_eq!(err["result"]["isError"], true);
    assert_eq!(err["result"]["content"][0]["text"], "raw body");
    let unknown = f.client.call("no_such_tool", json!({})).await;
    assert!(unknown.get("error").is_some());
    let ping = f.client.text("ping", json!({})).await;
    assert!(!ping.contains("stale: demo"), "{ping}");
}

#[tokio::test]
async fn ping_reports_identity_and_state_after_the_pong_line() {
    let mut f = fixture().await;
    let ping = f.client.text("ping", json!({})).await;
    assert_eq!(ping, "pong\ndemo 1.2.3 (up to date)");
    let echo = f.client.text("ping", json!({"message":"hi"})).await;
    assert_eq!(echo, "hi\ndemo 1.2.3 (up to date)");
    f.replace();
    let ping = f.client.text("ping", json!({})).await;
    assert_eq!(ping, "pong\ndemo 1.2.3 (stale: 9.9.9 installed)");
}

#[tokio::test]
async fn ping_without_self_check_is_unchanged_and_info_keeps_crate_version() {
    let mut client = boot(McpServer::new(ServerOptions::default())).await;
    assert_eq!(client.text("ping", json!({})).await, "pong");
    assert_eq!(client.info["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn get_info_reports_the_self_check_running_version() {
    let f = fixture().await;
    assert_eq!(f.client.info["version"], "1.2.3");
}

#[tokio::test]
async fn budget_preview_keeps_the_notice_and_retained_payload_never_stores_it() {
    let mut f = fixture().await;
    // Retained before staleness.
    let before = f.client.call("big", json!({})).await;
    let text = before["result"]["content"][0]["text"].as_str().unwrap();
    assert!(!text.contains("stale:"));
    let p: Value = serde_json::from_str(text).unwrap();
    let full = p["response_budget"]["next"]["full_result"].clone();
    f.replace();
    // A fresh preview after staleness still ends with the notice.
    // A preview carries structured content, so the notice is its own
    // trailing block and the preview JSON block stays byte-identical.
    let frame = f.client.call("big", json!({})).await;
    let blocks = frame["result"]["content"].as_array().unwrap();
    assert_eq!(blocks.last().unwrap()["text"], NOTICE, "{blocks:?}");
    assert!(serde_json::from_str::<Value>(blocks[0]["text"].as_str().unwrap()).is_ok());
    // Expanding the pre-stale result: notice added at serve time only,
    // the stored payload is the original bytes.
    let expanded = f
        .client
        .text(full["name"].as_str().unwrap(), full["arguments"].clone())
        .await;
    assert_eq!(expanded, format!("{}\n\n{NOTICE}", big()));
}
