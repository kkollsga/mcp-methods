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
    #[allow(dead_code)]
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

fn ledger_lines(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn env_var_enables_and_builder_wins() {
    let dir = tempfile::tempdir().unwrap();
    let env_path = dir.path().join("env.jsonl");
    let builder_path = dir.path().join("builder.jsonl");
    std::env::set_var("MCP_METHODS_SEND_LEDGER", &env_path);
    let mut client = boot(McpServer::new(ServerOptions::default())).await;
    client.request("tools/list", json!({})).await;
    let lines = ledger_lines(&env_path);
    assert!(lines.iter().any(|l| l["method"] == "tools/list"));

    let mut client =
        boot(McpServer::new(ServerOptions::default()).with_send_ledger(&builder_path)).await;
    client.request("tools/list", json!({})).await;
    assert!(ledger_lines(&builder_path)
        .iter()
        .any(|l| l["method"] == "tools/list"));
    std::env::remove_var("MCP_METHODS_SEND_LEDGER");
}
