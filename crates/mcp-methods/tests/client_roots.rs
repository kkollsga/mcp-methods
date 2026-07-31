//! MCP client-`roots` adoption, driven over a real transport.
//!
//! These tests speak the wire protocol by hand — `tokio::io::duplex` plus
//! newline-delimited JSON — rather than through a typed rmcp client. Three
//! reasons, all of which matter here:
//!
//! 1. The requirement is stated in terms of *frames* ("a client that does
//!    not advertise roots is never sent a `roots/list`"), and a raw client
//!    can capture and assert on every byte the server emits.
//! 2. Pathological clients — one that never answers, one that answers with
//!    a `https://` URI, one that answers with an empty array — are trivial
//!    to script and impossible to express with a well-behaved typed client.
//! 3. It needs no additional feature of `rmcp` (a typed client would pull
//!    in the `client` feature).
//!
//! They are also the runtime proof of the claim the design rests on: that
//! awaiting a server→client `roots/list` request *inside* `on_initialized`
//! completes rather than deadlocking. Every test that adopts a root would
//! hang instead of failing if that claim were wrong.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mcp_methods::server::workspace::{RootOwnership, Workspace};
use mcp_methods::server::{McpServer, ServerOptions};
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

/// Generous enough that a slow CI box never flakes, short enough that a
/// genuinely absent frame does not stall the suite.
const SETTLE: Duration = Duration::from_millis(2_000);

/// How long to let the server chew before concluding it sent nothing. The
/// adoption path is spawned, so "nothing yet" needs a real wait to mean
/// "nothing ever".
const QUIET: Duration = Duration::from_millis(400);

// ---------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------

/// A hand-written MCP client over one half of a duplex pipe.
struct RawClient {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    /// Every frame the server has sent, in order. The "was a `roots/list`
    /// ever put on the wire?" assertions read this.
    received: Vec<Value>,
    next_id: i64,
}

impl RawClient {
    async fn send(&mut self, frame: Value) {
        let mut line = serde_json::to_string(&frame).expect("frame serialises");
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .expect("write to duplex");
        self.writer.flush().await.expect("flush duplex");
    }

    /// Read the next frame, or `None` if the server sent nothing within
    /// `within` (or hung up).
    async fn recv(&mut self, within: Duration) -> Option<Value> {
        let mut line = String::new();
        let read = tokio::time::timeout(within, self.reader.read_line(&mut line)).await;
        match read {
            Ok(Ok(0)) | Err(_) => None,
            Ok(Ok(_)) => {
                let frame: Value = serde_json::from_str(line.trim()).expect("server sent JSON");
                self.received.push(frame.clone());
                Some(frame)
            }
            Ok(Err(e)) => panic!("transport read failed: {e}"),
        }
    }

    /// Drive `initialize` → response → `notifications/initialized`.
    ///
    /// `capabilities` is passed through verbatim so a test can advertise
    /// roots, advertise roots without `listChanged`, or advertise nothing.
    async fn handshake(&mut self, capabilities: Value) {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": capabilities,
                "clientInfo": { "name": "raw-test-client", "version": "0.0.0" }
            }
        }))
        .await;
        let response = self.recv(SETTLE).await.expect("initialize response");
        assert_eq!(response["id"], json!(id), "unexpected frame: {response}");
        assert!(
            response.get("result").is_some(),
            "initialize failed: {response}"
        );
        self.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
    }

    /// Wait for a server→client request with the given method, returning
    /// its frame. Panics if something else arrives first or nothing does.
    async fn expect_request(&mut self, method: &str) -> Value {
        let frame = self
            .recv(SETTLE)
            .await
            .unwrap_or_else(|| panic!("expected a {method} request, got nothing"));
        assert_eq!(
            frame["method"],
            json!(method),
            "expected {method}, got: {frame}"
        );
        frame
    }

    async fn respond(&mut self, request: &Value, result: Value) {
        let id = request["id"].clone();
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
            .await;
    }

    async fn respond_error(&mut self, request: &Value, message: &str) {
        let id = request["id"].clone();
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32603, "message": message }
        }))
        .await;
    }

    /// Round-trip a `tools/list` so we know the server is alive, and — by
    /// having answered something — that it has had ample opportunity to
    /// send anything it was going to send.
    async fn assert_still_serving(&mut self) {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": "tools/list" }))
            .await;
        loop {
            let frame = self
                .recv(SETTLE)
                .await
                .expect("server stopped answering tools/list");
            if frame["id"] == json!(id) {
                assert!(frame.get("result").is_some(), "tools/list failed: {frame}");
                return;
            }
        }
    }

    /// The frame-level requirement: a `roots/list` request was never put on
    /// the wire.
    fn assert_no_roots_list_sent(&self) {
        let offenders: Vec<&Value> = self
            .received
            .iter()
            .filter(|f| f["method"] == json!("roots/list"))
            .collect();
        assert!(
            offenders.is_empty(),
            "server sent {} roots/list request(s) it should not have: {:?}",
            offenders.len(),
            offenders
        );
    }

    fn roots_list_count(&self) -> usize {
        self.received
            .iter()
            .filter(|f| f["method"] == json!("roots/list"))
            .count()
    }

    /// Absorb whatever the server sends for `QUIET`, so `received` is a
    /// complete record before a negative assertion reads it.
    async fn drain_quiet(&mut self) {
        while self.recv(QUIET).await.is_some() {}
    }
}

/// Boot a server around `ws` and return a client wired to it. The returned
/// join handle owns the running service; dropping it ends the session.
fn boot(ws: &Workspace) -> (RawClient, tokio::task::JoinHandle<()>) {
    let options = ServerOptions::default().with_workspace(ws.clone());
    let server = McpServer::new(options);
    let (server_side, client_side) = tokio::io::duplex(64 * 1024);
    let handle = tokio::spawn(async move {
        let service = match server.serve(server_side).await {
            Ok(service) => service,
            // The client half is dropped at the end of a test; a handshake
            // that loses the race is not a failure.
            Err(_) => return,
        };
        let _ = service.waiting().await;
    });
    let (read, write) = tokio::io::split(client_side);
    (
        RawClient {
            reader: BufReader::new(read),
            writer: write,
            received: Vec::new(),
            next_id: 1,
        },
        handle,
    )
}

fn roots_capability() -> Value {
    json!({ "roots": { "listChanged": true } })
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Wait for the workspace to bind `expected`, polling rather than sleeping
/// a fixed amount — adoption happens on a task we do not hold.
async fn await_active_root(ws: &Workspace, expected: &Path) {
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        if ws.active_repo_path().as_deref() == Some(expected) {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "workspace never adopted {}; active root is {:?}",
                expected.display(),
                ws.active_repo_path()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn tempdir_with(children: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let td = tempfile::tempdir().expect("tempdir");
    // macOS puts tempdirs under the /var -> /private/var symlink, so every
    // path assertion has to compare canonical forms or it is vacuous.
    let base = td.path().canonicalize().expect("canonicalize tempdir");
    for child in children {
        std::fs::create_dir_all(base.join(child)).expect("create child dir");
    }
    (td, base)
}

// ---------------------------------------------------------------------
// The happy path (and the deadlock proof)
// ---------------------------------------------------------------------

#[tokio::test]
async fn unanchored_server_adopts_the_clients_root() {
    let (_td, base) = tempdir_with(&["project"]);
    let project = base.join("project");
    let ws = Workspace::open_local_unanchored(None)
        .unwrap()
        .with_adopt_client_roots();
    assert_eq!(ws.root_ownership(), RootOwnership::Unowned);
    assert!(ws.active_repo_path().is_none(), "must boot unanchored");

    let (mut client, _service) = boot(&ws);
    client.handshake(roots_capability()).await;

    // If awaiting `list_roots` inside `on_initialized` deadlocked, this is
    // where the test would hang rather than fail.
    let request = client.expect_request("roots/list").await;
    client
        .respond(
            &request,
            json!({ "roots": [ { "uri": file_uri(&project), "name": "project" } ] }),
        )
        .await;

    await_active_root(&ws, &project).await;
    assert_eq!(ws.root_ownership(), RootOwnership::Adopted);
    // The inventory directory is created at first activation, not at boot.
    assert!(project.join(".mcp-workspace").is_dir());
    client.assert_still_serving().await;
}

#[tokio::test]
async fn adoption_binds_the_first_valid_root_and_skips_the_rest() {
    let (_td, base) = tempdir_with(&["first", "second"]);
    let ws = Workspace::open_local_unanchored(None)
        .unwrap()
        .with_adopt_client_roots();
    let (mut client, _service) = boot(&ws);
    client.handshake(roots_capability()).await;

    let request = client.expect_request("roots/list").await;
    client
        .respond(
            &request,
            json!({
                "roots": [
                    // Not a file:// URI — skipped, not fatal.
                    { "uri": "https://example.com/repo" },
                    // Does not exist — skipped.
                    { "uri": file_uri(&base.join("missing")) },
                    { "uri": file_uri(&base.join("first")) },
                    { "uri": file_uri(&base.join("second")) },
                ]
            }),
        )
        .await;

    await_active_root(&ws, &base.join("first")).await;
    client.assert_still_serving().await;
}

// ---------------------------------------------------------------------
// Requirement 3 — clients that do not advertise roots are untouched
// ---------------------------------------------------------------------

#[tokio::test]
async fn client_without_roots_capability_is_never_sent_roots_list() {
    let (_td, base) = tempdir_with(&["project"]);
    let ws = Workspace::open_local_unanchored(None)
        .unwrap()
        .with_adopt_client_roots();
    let (mut client, _service) = boot(&ws);

    // A client advertising nothing at all — the overwhelmingly common case.
    client.handshake(json!({})).await;
    client.drain_quiet().await;
    client.assert_still_serving().await;
    client.drain_quiet().await;

    client.assert_no_roots_list_sent();
    assert!(
        ws.active_repo_path().is_none(),
        "nothing was advertised, so nothing may be bound"
    );
    assert_eq!(ws.root_ownership(), RootOwnership::Unowned);
    let _ = base;
}

#[tokio::test]
async fn adoption_disabled_means_no_roots_list_even_for_a_roots_client() {
    let (_td, base) = tempdir_with(&["project"]);
    // Same unanchored workspace, but without the opt-in builder.
    let ws = Workspace::open_local_unanchored(None).unwrap();
    let (mut client, _service) = boot(&ws);

    client.handshake(roots_capability()).await;
    client.drain_quiet().await;
    client.assert_still_serving().await;
    client.drain_quiet().await;

    client.assert_no_roots_list_sent();
    assert!(ws.active_repo_path().is_none());
    let _ = base;
}

// ---------------------------------------------------------------------
// Requirement 1 — explicit configuration always wins
// ---------------------------------------------------------------------

#[tokio::test]
async fn configured_root_is_kept_and_the_advertised_one_ignored() {
    let (_td, base) = tempdir_with(&["configured", "advertised"]);
    let configured = base.join("configured");
    let ws = Workspace::open_local(configured.clone(), None)
        .unwrap()
        // Enabled, and still inert: the operator owns the root.
        .with_adopt_client_roots();
    assert_eq!(ws.root_ownership(), RootOwnership::Operator);

    let (mut client, _service) = boot(&ws);
    client.handshake(roots_capability()).await;
    client.drain_quiet().await;
    client.assert_still_serving().await;
    client.drain_quiet().await;

    // The guard chain exits on ownership, so the request never happens —
    // a stronger result than "the answer was ignored".
    client.assert_no_roots_list_sent();
    assert_eq!(ws.active_repo_path().as_deref(), Some(configured.as_path()));
    assert_eq!(ws.root_ownership(), RootOwnership::Operator);
    let _ = base.join("advertised");
}

// ---------------------------------------------------------------------
// Requirement 2 — containment
// ---------------------------------------------------------------------

#[tokio::test]
async fn advertised_root_outside_the_sandbox_is_rejected() {
    let (_td, base) = tempdir_with(&["sandbox", "sandbox/inside", "outside"]);
    let ws = Workspace::open_local_unanchored(None)
        .unwrap()
        .with_sandbox_root(&base.join("sandbox"))
        .unwrap()
        .with_adopt_client_roots();

    let (mut client, _service) = boot(&ws);
    client.handshake(roots_capability()).await;
    let request = client.expect_request("roots/list").await;
    client
        .respond(
            &request,
            json!({ "roots": [ { "uri": file_uri(&base.join("outside")) } ] }),
        )
        .await;
    client.drain_quiet().await;

    assert!(
        ws.active_repo_path().is_none(),
        "an out-of-sandbox root must leave the server unanchored, got {:?}",
        ws.active_repo_path()
    );
    assert_eq!(ws.root_ownership(), RootOwnership::Unowned);
    client.assert_still_serving().await;
}

#[tokio::test]
async fn advertised_root_inside_the_sandbox_is_adopted() {
    let (_td, base) = tempdir_with(&["sandbox", "sandbox/inside", "outside"]);
    let inside = base.join("sandbox").join("inside");
    let ws = Workspace::open_local_unanchored(None)
        .unwrap()
        .with_sandbox_root(&base.join("sandbox"))
        .unwrap()
        .with_adopt_client_roots();

    let (mut client, _service) = boot(&ws);
    client.handshake(roots_capability()).await;
    let request = client.expect_request("roots/list").await;
    client
        .respond(
            &request,
            json!({ "roots": [ { "uri": file_uri(&inside) } ] }),
        )
        .await;

    await_active_root(&ws, &inside).await;
    assert_eq!(ws.root_ownership(), RootOwnership::Adopted);
}

// ---------------------------------------------------------------------
// Pathological clients
// ---------------------------------------------------------------------

#[tokio::test]
async fn client_that_answers_with_a_non_file_uri_leaves_the_server_unanchored() {
    let ws = Workspace::open_local_unanchored(None)
        .unwrap()
        .with_adopt_client_roots();
    let (mut client, _service) = boot(&ws);
    client.handshake(roots_capability()).await;

    let request = client.expect_request("roots/list").await;
    client
        .respond(
            &request,
            json!({ "roots": [ { "uri": "https://example.com/repo", "name": "web" } ] }),
        )
        .await;
    client.drain_quiet().await;

    assert!(ws.active_repo_path().is_none());
    assert_eq!(ws.root_ownership(), RootOwnership::Unowned);
    client.assert_still_serving().await;
}

#[tokio::test]
async fn client_that_answers_with_a_nonexistent_path_leaves_the_server_unanchored() {
    let (_td, base) = tempdir_with(&[]);
    let ws = Workspace::open_local_unanchored(None)
        .unwrap()
        .with_adopt_client_roots();
    let (mut client, _service) = boot(&ws);
    client.handshake(roots_capability()).await;

    let request = client.expect_request("roots/list").await;
    client
        .respond(
            &request,
            json!({ "roots": [ { "uri": file_uri(&base.join("does-not-exist")) } ] }),
        )
        .await;
    client.drain_quiet().await;

    assert!(ws.active_repo_path().is_none());
    client.assert_still_serving().await;
}

#[tokio::test]
async fn client_that_answers_with_no_roots_leaves_the_server_unanchored() {
    let ws = Workspace::open_local_unanchored(None)
        .unwrap()
        .with_adopt_client_roots();
    let (mut client, _service) = boot(&ws);
    client.handshake(roots_capability()).await;

    let request = client.expect_request("roots/list").await;
    client.respond(&request, json!({ "roots": [] })).await;
    client.drain_quiet().await;

    assert!(ws.active_repo_path().is_none());
    assert_eq!(ws.root_ownership(), RootOwnership::Unowned);
    client.assert_still_serving().await;
}

#[tokio::test]
async fn client_that_errors_the_request_leaves_the_server_functional() {
    let ws = Workspace::open_local_unanchored(None)
        .unwrap()
        .with_adopt_client_roots();
    let (mut client, _service) = boot(&ws);
    client.handshake(roots_capability()).await;

    let request = client.expect_request("roots/list").await;
    client.respond_error(&request, "roots unavailable").await;
    client.drain_quiet().await;

    assert!(ws.active_repo_path().is_none());
    client.assert_still_serving().await;
    // No retry loop: one failed attempt is the whole attempt.
    assert_eq!(client.roots_list_count(), 1);
}

/// The timeout belt. `Peer::list_roots` has no deadline of its own, so a
/// client that accepts the request and never answers would otherwise pin
/// the adoption task forever. Deliberately slow — it waits out the real
/// 5-second belt to prove the arm fires.
#[tokio::test]
async fn client_that_never_answers_times_out_and_the_server_stays_usable() {
    let ws = Workspace::open_local_unanchored(None)
        .unwrap()
        .with_adopt_client_roots();
    let (mut client, _service) = boot(&ws);
    client.handshake(roots_capability()).await;

    let _request = client.expect_request("roots/list").await;
    // ... and we simply never answer it.
    tokio::time::sleep(Duration::from_millis(6_500)).await;
    client.drain_quiet().await;

    assert!(
        ws.active_repo_path().is_none(),
        "a silent client must leave the server unanchored"
    );
    assert_eq!(ws.root_ownership(), RootOwnership::Unowned);
    client.assert_still_serving().await;
    assert_eq!(
        client.roots_list_count(),
        1,
        "the timeout must not start a retry loop"
    );
}

// ---------------------------------------------------------------------
// roots/list_changed
// ---------------------------------------------------------------------

#[tokio::test]
async fn list_changed_re_adopts_while_the_root_is_adopted() {
    let (_td, base) = tempdir_with(&["first", "second"]);
    let ws = Workspace::open_local_unanchored(None)
        .unwrap()
        .with_adopt_client_roots();
    let (mut client, _service) = boot(&ws);
    client.handshake(roots_capability()).await;

    let request = client.expect_request("roots/list").await;
    client
        .respond(
            &request,
            json!({ "roots": [ { "uri": file_uri(&base.join("first")) } ] }),
        )
        .await;
    await_active_root(&ws, &base.join("first")).await;

    client
        .send(json!({ "jsonrpc": "2.0", "method": "notifications/roots/list_changed" }))
        .await;
    let request = client.expect_request("roots/list").await;
    client
        .respond(
            &request,
            json!({ "roots": [ { "uri": file_uri(&base.join("second")) } ] }),
        )
        .await;

    await_active_root(&ws, &base.join("second")).await;
    assert_eq!(ws.root_ownership(), RootOwnership::Adopted);
}

#[tokio::test]
async fn set_root_dir_after_adoption_takes_ownership_and_list_changed_is_ignored() {
    let (_td, base) = tempdir_with(&["adopted", "operator", "later"]);
    let ws = Workspace::open_local_unanchored(None)
        .unwrap()
        .with_adopt_client_roots();
    let (mut client, _service) = boot(&ws);
    client.handshake(roots_capability()).await;

    let request = client.expect_request("roots/list").await;
    client
        .respond(
            &request,
            json!({ "roots": [ { "uri": file_uri(&base.join("adopted")) } ] }),
        )
        .await;
    await_active_root(&ws, &base.join("adopted")).await;

    // The operator steps in.
    let operator = base.join("operator");
    ws.set_root_dir(&operator, None);
    assert_eq!(ws.active_repo_path().as_deref(), Some(operator.as_path()));
    assert_eq!(ws.root_ownership(), RootOwnership::Operator);

    let before = client.roots_list_count();
    client
        .send(json!({ "jsonrpc": "2.0", "method": "notifications/roots/list_changed" }))
        .await;
    client.drain_quiet().await;
    client.assert_still_serving().await;
    client.drain_quiet().await;

    assert_eq!(
        client.roots_list_count(),
        before,
        "an operator-owned root must not trigger another roots/list"
    );
    assert_eq!(
        ws.active_repo_path().as_deref(),
        Some(operator.as_path()),
        "operator ownership must survive roots/list_changed"
    );
}

#[tokio::test]
async fn list_changed_from_a_client_that_did_not_advertise_it_is_ignored() {
    let (_td, base) = tempdir_with(&["project"]);
    let ws = Workspace::open_local_unanchored(None)
        .unwrap()
        .with_adopt_client_roots();
    let (mut client, _service) = boot(&ws);
    // `roots` advertised, `listChanged` deliberately absent.
    client.handshake(json!({ "roots": {} })).await;

    let request = client.expect_request("roots/list").await;
    client
        .respond(
            &request,
            json!({ "roots": [ { "uri": file_uri(&base.join("project")) } ] }),
        )
        .await;
    await_active_root(&ws, &base.join("project")).await;

    let before = client.roots_list_count();
    client
        .send(json!({ "jsonrpc": "2.0", "method": "notifications/roots/list_changed" }))
        .await;
    client.drain_quiet().await;
    client.assert_still_serving().await;
    client.drain_quiet().await;

    assert_eq!(
        client.roots_list_count(),
        before,
        "a client that never advertised roots.listChanged must not be re-queried"
    );
}
