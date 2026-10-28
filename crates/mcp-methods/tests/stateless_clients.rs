//! Protocol revision 2026-07-28 has no `initialize`: a client opens with
//! `server/discover` and repeats its identity in every request's `_meta`.
//! These tests speak that wire format by hand over the framework's stdio
//! codec and pin what a handshake-less client can do, next to the legacy
//! handshake path they must not disturb.
#![cfg(feature = "server")]

use std::time::Duration;

use mcp_methods::server::stdio::StdioTransport;
use mcp_methods::server::{ListChange, McpServer, ServerOptions};
use rmcp::ServiceExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

const SETTLE: Duration = Duration::from_secs(10);

struct Client {
    read: BufReader<ReadHalf<DuplexStream>>,
    write: WriteHalf<DuplexStream>,
    id: i64,
    /// Every frame received, in order.
    frames: Vec<Value>,
    stateless: bool,
    /// The server's side of the connection, for announcing changes. `serve`
    /// returns only once the first message has been read, so it is
    /// available after the first request.
    peer_rx: Option<tokio::sync::oneshot::Receiver<rmcp::service::Peer<rmcp::RoleServer>>>,
    peer: Option<rmcp::service::Peer<rmcp::RoleServer>>,
}

impl Client {
    fn meta(&self) -> Value {
        json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {"name": "stateless-test", "version": "1"},
            "io.modelcontextprotocol/clientCapabilities": {},
        })
    }

    async fn send(&mut self, frame: Value) {
        self.write
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .unwrap();
        self.write.flush().await.unwrap();
    }

    async fn recv(&mut self, within: Duration) -> Option<Value> {
        let mut line = String::new();
        match tokio::time::timeout(within, self.read.read_line(&mut line)).await {
            Ok(Ok(n)) if n > 0 => {
                let frame: Value = serde_json::from_str(&line).unwrap();
                self.frames.push(frame.clone());
                Some(frame)
            }
            _ => None,
        }
    }

    /// Send a request and return its response, skipping other frames.
    async fn request(&mut self, method: &str, mut params: Value) -> Value {
        self.id += 1;
        let id = self.id;
        if self.stateless {
            params["_meta"] = self.meta();
        }
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        loop {
            let frame = self
                .recv(SETTLE)
                .await
                .unwrap_or_else(|| panic!("no response to {method}"));
            if frame["id"] == id {
                return frame;
            }
        }
    }

    /// The server-side peer of this connection; the connection must have
    /// carried a request already.
    async fn server_peer(&mut self) -> rmcp::service::Peer<rmcp::RoleServer> {
        if self.peer.is_none() {
            self.peer = Some(self.peer_rx.take().unwrap().await.unwrap());
        }
        self.peer.clone().unwrap()
    }

    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }
}

fn boot(server: McpServer, stateless: bool) -> Client {
    let (a, b) = tokio::io::duplex(1024 * 1024);
    let (peer_tx, peer_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (read, write) = tokio::io::split(a);
        let service = server
            .serve(StdioTransport::from_io(read, write))
            .await
            .unwrap();
        let _ = peer_tx.send(service.peer().clone());
        let _ = service.waiting().await;
    });
    let (read, write) = tokio::io::split(b);
    Client {
        read: BufReader::new(read),
        write,
        id: 0,
        frames: Vec::new(),
        stateless,
        peer_rx: Some(peer_rx),
        peer: None,
    }
}

/// `initialize` + `notifications/initialized`; returns the negotiated version.
async fn handshake(client: &mut Client, version: &str) -> String {
    let init = client
        .request(
            "initialize",
            json!({"protocolVersion": version, "capabilities": {},
                   "clientInfo": {"name": "legacy-test", "version": "1"}}),
        )
        .await;
    assert!(init.get("error").is_none(), "{init}");
    client
        .send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .await;
    init["result"]["protocolVersion"].as_str().unwrap().into()
}

#[derive(Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Args {
    value: String,
}

fn big_server() -> McpServer {
    let mut server = McpServer::new(ServerOptions::default());
    server.register_typed_tool("big", "Returns a large payload", |_: Args| {
        json!((0..400)
            .map(|i| json!({"id": i, "text": "x".repeat(200)}))
            .collect::<Vec<_>>())
        .to_string()
    });
    server
}

fn handle_of(frame: &Value) -> Value {
    let text = frame["result"]["content"][0]["text"].as_str().unwrap();
    let preview: Value = serde_json::from_str(text).unwrap();
    preview["response_budget"]["next"]["full_result"].clone()
}

#[tokio::test]
async fn stateless_client_discovers_lists_and_calls_tools() {
    let mut client = boot(McpServer::new(ServerOptions::default()), true);
    let discovered = client.request("server/discover", json!({})).await;
    assert!(discovered.get("error").is_none(), "{discovered}");
    let listed = client.request("tools/list", json!({})).await;
    assert!(listed.get("error").is_none(), "{listed}");
    let called = client.call("ping", json!({})).await;
    assert!(called.get("error").is_none(), "tools/call failed: {called}");
    assert_eq!(called["result"]["resultType"], json!("complete"));
}

#[tokio::test]
async fn stateless_response_handles_resolve_across_calls_and_not_across_clients() {
    let server = big_server();
    let mut client = boot(server.clone(), true);
    let mut outsider = boot(server, true);
    let first = client.call("big", json!({"value": "x"})).await;
    assert!(first.get("error").is_none(), "{first}");
    let handle = handle_of(&first);
    let name = handle["name"].as_str().unwrap();
    let expanded = client.call(name, handle["arguments"].clone()).await;
    assert!(expanded.get("error").is_none(), "{expanded}");
    assert!(expanded["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("\"id\":399"));
    let denied = outsider.call(name, handle["arguments"].clone()).await;
    assert!(
        denied.get("error").is_some(),
        "another stateless connection must not read this one's handle: {denied}"
    );
}

#[tokio::test]
async fn legacy_handshake_client_still_gets_session_scoped_handles() {
    let server = big_server();
    let mut client = boot(server.clone(), false);
    let mut outsider = boot(server, false);
    assert_eq!(handshake(&mut client, "2025-11-25").await, "2025-11-25");
    handshake(&mut outsider, "2025-11-25").await;
    let first = client.call("big", json!({"value": "x"})).await;
    let handle = handle_of(&first);
    let name = handle["name"].as_str().unwrap();
    let expanded = client.call(name, handle["arguments"].clone()).await;
    assert!(expanded.get("error").is_none(), "{expanded}");
    let denied = outsider.call(name, handle["arguments"].clone()).await;
    assert!(denied.get("error").is_some(), "{denied}");
}

#[tokio::test]
async fn initialize_naming_a_handshake_less_version_falls_back_to_the_newest_handshake_version() {
    let mut client = boot(McpServer::new(ServerOptions::default()), false);
    assert_eq!(handshake(&mut client, "2026-07-28").await, "2025-11-25");
}

/// Open a `subscriptions/listen` stream and return its request id once the
/// acknowledgement has arrived.
async fn listen(client: &mut Client, notifications: Value) -> i64 {
    client.id += 1;
    let id = client.id;
    let meta = client.meta();
    client
        .send(
            json!({"jsonrpc":"2.0","id":id,"method":"subscriptions/listen",
                     "params":{"_meta": meta, "notifications": notifications}}),
        )
        .await;
    let ack = client
        .recv(SETTLE)
        .await
        .expect("subscriptions/listen is acknowledged");
    assert_eq!(
        ack["method"], "notifications/subscriptions/acknowledged",
        "{ack}"
    );
    assert_eq!(
        ack["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
        id
    );
    id
}

/// Announce until the client sees a notification with `method`.
async fn announce_until(client: &mut Client, server: &McpServer, method: &str) -> Value {
    for _ in 0..50 {
        server
            .announce_list_changed(&client.server_peer().await, ListChange::BOTH)
            .await
            .unwrap();
        while let Some(frame) = client.recv(Duration::from_millis(100)).await {
            if frame["method"] == method {
                return frame;
            }
        }
    }
    panic!("no {method} reached the client");
}

#[tokio::test]
async fn stateless_client_listens_and_receives_tagged_list_changed() {
    let server = McpServer::new(ServerOptions::default());
    let mut client = boot(server.clone(), true);
    let id = listen(&mut client, json!({"toolsListChanged": true})).await;
    let frame = announce_until(&mut client, &server, "notifications/tools/list_changed").await;
    assert_eq!(
        frame["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
        id
    );
    // The stream asked for tools only: prompts are never sent on it.
    while let Some(frame) = client.recv(Duration::from_millis(200)).await {
        assert_ne!(
            frame["method"], "notifications/prompts/list_changed",
            "{frame}"
        );
    }
}

#[tokio::test]
async fn announce_reaches_a_handshake_client_directly_and_never_untagged_to_a_stateless_one() {
    let server = McpServer::new(ServerOptions::default());
    let mut legacy = boot(server.clone(), false);
    handshake(&mut legacy, "2025-11-25").await;
    let mut stateless = boot(server.clone(), true);
    // A stateless connection that has not subscribed gets nothing.
    stateless.call("ping", json!({})).await;
    server
        .announce_list_changed(&stateless.server_peer().await, ListChange::TOOLS)
        .await
        .unwrap();
    assert!(stateless.recv(Duration::from_millis(300)).await.is_none());
    server
        .announce_list_changed(&legacy.server_peer().await, ListChange::BOTH)
        .await
        .unwrap();
    let first = legacy.recv(SETTLE).await.unwrap();
    let second = legacy.recv(SETTLE).await.unwrap();
    assert_eq!(first["method"], "notifications/tools/list_changed");
    assert_eq!(second["method"], "notifications/prompts/list_changed");
    assert!(first["params"].get("_meta").is_none(), "{first}");
}

#[tokio::test]
async fn a_cancelled_subscription_stops_receiving() {
    let server = McpServer::new(ServerOptions::default());
    let mut client = boot(server.clone(), true);
    let id = listen(&mut client, json!({"toolsListChanged": true})).await;
    announce_until(&mut client, &server, "notifications/tools/list_changed").await;
    client
        .send(json!({"jsonrpc":"2.0","method":"notifications/cancelled",
                     "params":{"requestId": id}}))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    while client.recv(Duration::from_millis(100)).await.is_some() {}
    server
        .announce_list_changed(&client.server_peer().await, ListChange::TOOLS)
        .await
        .unwrap();
    assert!(client.recv(Duration::from_millis(300)).await.is_none());
}

#[tokio::test]
async fn the_server_sends_a_stateless_client_no_requests_and_no_log_messages() {
    let mut client = boot(McpServer::new(ServerOptions::default()), true);
    client.request("server/discover", json!({})).await;
    client.call("ping", json!({})).await;
    while client.recv(Duration::from_millis(200)).await.is_some() {}
    for frame in &client.frames {
        assert!(
            frame.get("method").is_none(),
            "unsolicited server frame for a handshake-less client: {frame}"
        );
    }
}
