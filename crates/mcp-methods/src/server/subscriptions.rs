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

/// Longest a resumed process waits for its replayed listen streams.
const REOPEN_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

struct Entry {
    key: u64,
    sink: SubscriptionSink,
    /// The `subscriptions/listen` line as received, for replay after an
    /// in-place reload. `None` when the transport keeps no raw text.
    raw: Option<Arc<str>>,
}

/// The `subscriptions/listen` streams currently open, across every
/// connection this server (and its clones) serves.
#[derive(Default)]
pub(crate) struct Subscriptions {
    open: Mutex<Vec<Entry>>,
    next_key: AtomicU64,
}

/// Removes its subscription from the registry when dropped.
pub(crate) struct Registration {
    registry: Arc<Subscriptions>,
    key: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.registry.lock().retain(|e| e.key != self.key);
    }
}

impl Subscriptions {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn register(
        self: &Arc<Self>,
        sink: SubscriptionSink,
        raw: Option<Arc<str>>,
    ) -> Registration {
        let key = self.next_key.fetch_add(1, Ordering::Relaxed);
        self.lock().push(Entry { key, sink, raw });
        Registration {
            registry: self.clone(),
            key,
        }
    }

    /// Wait until `count` streams are registered, at most [`REOPEN_WAIT`].
    /// A resumed process re-opens the streams by replaying their request
    /// lines, which the serve loop handles asynchronously.
    pub(crate) async fn await_open(&self, count: usize) {
        let deadline = tokio::time::Instant::now() + REOPEN_WAIT;
        while self.lock().len() < count && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// The raw `subscriptions/listen` lines of every open stream.
    pub(crate) fn raw_lines(&self) -> Vec<String> {
        self.lock()
            .iter()
            .filter_map(|e| e.raw.as_deref().map(str::to_owned))
            .collect()
    }

    /// Send `change` on every open stream. A stream that did not ask for a
    /// list is skipped by the sink itself; a closed one is dropped by its
    /// [`Registration`].
    pub(crate) async fn announce(&self, change: ListChange) {
        let sinks: Vec<SubscriptionSink> = self.lock().iter().map(|e| e.sink.clone()).collect();
        for sink in sinks {
            if change.tools {
                log_send_failure(sink.notify_tool_list_changed().await);
            }
            if change.prompts {
                log_send_failure(sink.notify_prompt_list_changed().await);
            }
        }
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
    pub(crate) async fn await_subscriptions(&self, count: usize) {
        self.subscriptions().await_open(count).await;
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
        let direct = if takes_direct_notifications(peer) {
            send_direct(peer, change).await
        } else {
            Ok(())
        };
        self.subscriptions().announce(change).await;
        direct
    }
}
