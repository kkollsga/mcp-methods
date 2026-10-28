//! Announcing "the tool/prompt list changed" to both kinds of client.
//!
//! A client that opened with `initialize` (2025-11-25 and older) is told
//! with a plain `notifications/tools/list_changed` on its connection. A
//! client on 2026-07-28 or later has no handshake and receives no
//! unsolicited notifications at all: it opens a `subscriptions/listen`
//! request and the server answers on that request's stream, each
//! notification tagged with `io.modelcontextprotocol/subscriptionId`.
//! [`McpServer::announce_list_changed`] does the right one of the two for
//! each recipient.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rmcp::model::SubscriptionFilter;
use rmcp::service::{Peer, ServiceError, SubscriptionSink};
use rmcp::RoleServer;

use crate::server::server::McpServer;

/// Which lists an [`McpServer::announce_list_changed`] call reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListChange {
    pub tools: bool,
    pub prompts: bool,
}

impl ListChange {
    /// `notifications/tools/list_changed` only.
    pub const TOOLS: Self = Self {
        tools: true,
        prompts: false,
    };
    /// `notifications/prompts/list_changed` only.
    pub const PROMPTS: Self = Self {
        tools: false,
        prompts: true,
    };
    /// Both, tools first: the tool plane is the one every real client
    /// exposes to the model.
    pub const BOTH: Self = Self {
        tools: true,
        prompts: true,
    };
}

struct Entry {
    key: u64,
    sink: SubscriptionSink,
    /// The `subscriptions/listen` line as received, for replay after an
    /// in-place reload. `None` when the transport keeps no raw text.
    raw: Option<Arc<str>>,
}

#[derive(Default)]
struct Inner {
    open: Vec<Entry>,
    /// Which lists have been announced as changed at any point in this
    /// process.
    changed: Option<ListChange>,
}

/// The `subscriptions/listen` streams currently open, across every
/// connection this server (and its clones) serves.
///
/// A stream must hear about every list change that its client could have
/// missed, and its client can miss one in two ways. rmcp acknowledges a
/// request before it runs the handler's `listen`, which registers the
/// stream, so an announcement in that gap reaches no stream. And a client
/// sends `subscriptions/listen` and `tools/list` back to back without
/// waiting for the acknowledgement, so the list can be served, changed
/// and announced before the stream is granted at all. Request order cannot
/// tell those apart from a harmless announcement, so [`register`] replays
/// every category announced at any earlier point: at most one redundant
/// `list_changed` per new stream on a server that has changed, which is a
/// hint to re-list, and never a lost one. `announce` and `register` take
/// the same lock, so an announcement is either replayed or finds the stream
/// open.
///
/// [`register`]: Subscriptions::register
#[derive(Default)]
pub(crate) struct Subscriptions {
    inner: Mutex<Inner>,
    next_key: AtomicU64,
    /// Test seam: parks `listen` after the acknowledgement went out and
    /// before the stream is registered. `(reached, release)`.
    #[cfg(test)]
    pub(crate) listen_gate: Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
}

/// Removes its subscription from the registry when dropped.
pub(crate) struct Registration {
    registry: Arc<Subscriptions>,
    key: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.registry.lock().open.retain(|e| e.key != self.key);
    }
}

impl Inner {
    fn record(&mut self, change: ListChange) {
        let seen = self.changed.get_or_insert(ListChange {
            tools: false,
            prompts: false,
        });
        seen.tools |= change.tools;
        seen.prompts |= change.prompts;
    }
}

impl Subscriptions {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    pub(crate) async fn hold_before_register(&self) {
        let gate = self.listen_gate.lock().unwrap().clone();
        if let Some((reached, release)) = gate {
            reached.notify_one();
            release.notified().await;
        }
    }

    /// Record `change` without sending it, so every stream registered
    /// from now on is caught up. An in-place reload calls this before the
    /// replayed `subscriptions/listen` lines are read.
    pub(crate) fn mark_changed(&self, change: ListChange) {
        self.lock().record(change);
    }

    pub(crate) async fn register(
        self: &Arc<Self>,
        sink: SubscriptionSink,
        raw: Option<Arc<str>>,
    ) -> Registration {
        let key = self.next_key.fetch_add(1, Ordering::Relaxed);
        let catch_up = {
            let mut inner = self.lock();
            inner.open.push(Entry {
                key,
                sink: sink.clone(),
                raw,
            });
            inner.changed.unwrap_or(ListChange {
                tools: false,
                prompts: false,
            })
        };
        send_change(&sink, catch_up).await;
        Registration {
            registry: self.clone(),
            key,
        }
    }

    /// The raw `subscriptions/listen` lines of every open stream.
    pub(crate) fn raw_lines(&self) -> Vec<String> {
        self.lock()
            .open
            .iter()
            .filter_map(|e| e.raw.as_deref().map(str::to_owned))
            .collect()
    }

    /// Send `change` on every open stream. A stream that did not ask for a
    /// list is skipped by the sink itself; a closed one is dropped by its
    /// [`Registration`].
    pub(crate) async fn announce(&self, change: ListChange) {
        let sinks: Vec<SubscriptionSink> = {
            let mut inner = self.lock();
            inner.record(change);
            inner.open.iter().map(|e| e.sink.clone()).collect()
        };
        for sink in sinks {
            send_change(&sink, change).await;
        }
    }
}

async fn send_change(sink: &SubscriptionSink, change: ListChange) {
    if change.tools {
        log_send_failure(sink.notify_tool_list_changed().await);
    }
    if change.prompts {
        log_send_failure(sink.notify_prompt_list_changed().await);
    }
}

fn log_send_failure(result: Result<(), rmcp::service::SubscriptionSendError>) {
    use rmcp::service::SubscriptionSendError::{NotificationNotAccepted, SubscriptionClosed};
    match result {
        Ok(()) | Err(NotificationNotAccepted(_) | SubscriptionClosed) => {}
        Err(error) => tracing::debug!("list-changed notification not delivered: {error}"),
    }
}

/// The filter this server grants a `subscriptions/listen` request: the two
/// list-changed categories it advertises. rmcp intersects the result with
/// the request and with `get_info()`'s capabilities.
pub(crate) fn accepted_filter() -> SubscriptionFilter {
    SubscriptionFilter::builder()
        .tools_list_changed()
        .prompts_list_changed()
        .build()
}

/// Does `peer` take unsolicited notifications on its connection, i.e. did
/// it negotiate a revision with an `initialize` handshake?
fn takes_direct_notifications(peer: &Peer<RoleServer>) -> bool {
    peer.peer_info()
        .is_some_and(|info| info.protocol_version.has_initialize())
}

async fn send_direct(peer: &Peer<RoleServer>, change: ListChange) -> Result<(), ServiceError> {
    if change.tools {
        peer.notify_tool_list_changed().await?;
    }
    if change.prompts {
        peer.notify_prompt_list_changed().await?;
    }
    Ok(())
}

impl McpServer {
    /// Only the direct half of [`announce_list_changed`](Self::announce_list_changed).
    pub(crate) async fn announce_direct(
        &self,
        peer: &Peer<RoleServer>,
        change: ListChange,
    ) -> Result<(), ServiceError> {
        if takes_direct_notifications(peer) {
            send_direct(peer, change).await
        } else {
            Ok(())
        }
    }

    /// Announce that the tool list, the prompt list, or both changed.
    ///
    /// The one call a consumer makes after
    /// [`reinject_skills`](Self::reinject_skills), a tool-set rebuild or
    /// any other change a client should re-fetch. `peer` is the peer the
    /// triggering request came from (`&context.peer`).
    ///
    /// - A peer that completed `initialize` gets a direct
    ///   `notifications/tools/list_changed` / `prompts/list_changed`.
    /// - A 2026-07-28 peer gets nothing directly (it would never be
    ///   delivered). Every open `subscriptions/listen` stream on this
    ///   server that asked for the list gets the notification instead,
    ///   tagged with its subscription id.
    ///
    /// Streams are server-wide: a stateless client is told even when a
    /// different connection triggered the change. Returns the direct
    /// send's failure, if any; stream delivery failures are logged, since
    /// a closed stream is not the caller's error. The streams are
    /// announced to even when the direct send fails.
    pub async fn announce_list_changed(
        &self,
        peer: &Peer<RoleServer>,
        change: ListChange,
    ) -> Result<(), ServiceError> {
        let direct = self.announce_direct(peer, change).await;
        self.subscriptions().announce(change).await;
        direct
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::stdio::StdioTransport;
    use crate::server::ServerOptions;
    use rmcp::ServiceExt;
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const META: &str = r#"{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"c","version":"1"},"io.modelcontextprotocol/clientCapabilities":{}}"#;

    async fn frame(read: &mut (impl AsyncBufReadExt + Unpin), within_ms: u64) -> Option<Value> {
        let mut line = String::new();
        match tokio::time::timeout(
            std::time::Duration::from_millis(within_ms),
            read.read_line(&mut line),
        )
        .await
        {
            Ok(Ok(n)) if n > 0 => Some(serde_json::from_str(&line).unwrap()),
            _ => None,
        }
    }

    /// Open a stream, force an announcement into the gap between the
    /// acknowledgement and the registration, then assert the stream is told.
    async fn announce_in_the_gap(change: ListChange, method: &str, requested: Value) {
        let server = McpServer::new(ServerOptions::default());
        let reached = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *server.subscriptions().listen_gate.lock().unwrap() =
            Some((reached.clone(), release.clone()));
        let (a, b) = tokio::io::duplex(1 << 20);
        let serving = server.clone();
        tokio::spawn(async move {
            let (read, write) = tokio::io::split(a);
            let service = serving
                .serve(StdioTransport::from_io(read, write))
                .await
                .unwrap();
            let _ = service.waiting().await;
        });
        let (read, mut write) = tokio::io::split(b);
        let mut read = BufReader::new(read);
        let meta: Value = serde_json::from_str(META).unwrap();
        let request = json!({"jsonrpc":"2.0","id":7,"method":"subscriptions/listen",
            "params":{"_meta": meta, "notifications": requested}});
        write
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let ack = frame(&mut read, 10_000).await.expect("acknowledged");
        assert_eq!(ack["method"], "notifications/subscriptions/acknowledged");
        // `listen` is parked before registering: this announcement reaches
        // no registered stream.
        tokio::time::timeout(std::time::Duration::from_secs(10), reached.notified())
            .await
            .expect("listen reached the gate");
        server.subscriptions().announce(change).await;
        release.notify_one();
        let told = frame(&mut read, 3_000)
            .await
            .expect("the list change announced after the acknowledgement was lost");
        assert_eq!(told["method"], method, "{told}");
        assert_eq!(
            told["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
            7
        );
    }

    #[tokio::test]
    async fn a_tools_change_between_acknowledgement_and_registration_is_delivered() {
        announce_in_the_gap(
            ListChange::TOOLS,
            "notifications/tools/list_changed",
            json!({"toolsListChanged": true}),
        )
        .await;
    }

    /// Serve `server` over a duplex pipe and send a tools-list-changed
    /// `subscriptions/listen` with id 7; returns the client's two ends.
    async fn open_listen(
        server: &McpServer,
    ) -> (
        BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (a, b) = tokio::io::duplex(1 << 20);
        let serving = server.clone();
        tokio::spawn(async move {
            let (read, write) = tokio::io::split(a);
            let service = serving
                .serve(StdioTransport::from_io(read, write))
                .await
                .unwrap();
            let _ = service.waiting().await;
        });
        let (read, mut write) = tokio::io::split(b);
        let meta: Value = serde_json::from_str(META).unwrap();
        let request = json!({"jsonrpc":"2.0","id":7,"method":"subscriptions/listen",
            "params":{"_meta": meta, "notifications": {"toolsListChanged": true}}});
        write
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        (BufReader::new(read), write)
    }

    /// A client that sends `subscriptions/listen` and `tools/list` back to
    /// back does not wait for the acknowledgement: the list can be served
    /// and changed before the stream is even granted.
    #[tokio::test]
    async fn a_change_announced_before_the_stream_was_opened_is_delivered_when_it_opens() {
        let server = McpServer::new(ServerOptions::default());
        server.subscriptions().announce(ListChange::TOOLS).await;
        let (mut read, _write) = open_listen(&server).await;
        let ack = frame(&mut read, 10_000).await.expect("acknowledged");
        assert_eq!(ack["method"], "notifications/subscriptions/acknowledged");
        let told = frame(&mut read, 3_000)
            .await
            .expect("a change announced before the stream opened was lost");
        assert_eq!(told["method"], "notifications/tools/list_changed", "{told}");
    }

    #[tokio::test]
    async fn a_stream_opened_on_a_server_that_never_changed_is_told_nothing() {
        let server = McpServer::new(ServerOptions::default());
        let (mut read, _write) = open_listen(&server).await;
        let ack = frame(&mut read, 10_000).await.expect("acknowledged");
        assert_eq!(ack["method"], "notifications/subscriptions/acknowledged");
        assert!(frame(&mut read, 300).await.is_none());
        // A real change afterwards still arrives.
        server.subscriptions().announce(ListChange::TOOLS).await;
        let told = frame(&mut read, 3_000).await.expect("live announce");
        assert_eq!(told["method"], "notifications/tools/list_changed", "{told}");
    }
}
