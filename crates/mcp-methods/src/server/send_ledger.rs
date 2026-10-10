//! Opt-in send ledger: a JSONL record of what the server put on the wire.
//!
//! Off by default. When off, no hook serializes anything: every call
//! site checks for a configured ledger first. See
//! [`McpServer::with_send_ledger`](crate::server::McpServer::with_send_ledger).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;
use serde_json::{json, Map, Value};

/// Environment variable naming a ledger file; the builder wins over it.
pub const SEND_LEDGER_ENV: &str = "MCP_METHODS_SEND_LEDGER";

enum State {
    Unopened,
    Open(File),
    Failed,
}

/// Append-only JSONL writer. Never fails its caller: the first open or
/// write error is logged once and the ledger goes quiet.
pub(crate) struct SendLedger {
    path: PathBuf,
    state: Mutex<State>,
}

/// Serialized JSON byte length (the unit every ledger count uses).
pub(crate) fn json_len<T: Serialize + ?Sized>(value: &T) -> usize {
    serde_json::to_vec(value).map(|v| v.len()).unwrap_or(0)
}

/// Serialized length of a result as it goes on the wire. rmcp strips the
/// `resultType` discriminator for peers on a pre-2026-07-28 protocol after
/// the handler returns, so `legacy` removes it here too.
pub(crate) fn wire_len<T: Serialize + ?Sized>(value: &T, legacy: bool) -> usize {
    if !legacy {
        return json_len(value);
    }
    match serde_json::to_value(value) {
        Ok(Value::Object(mut o)) => {
            o.remove("resultType");
            json_len(&o)
        }
        Ok(other) => json_len(&other),
        Err(_) => 0,
    }
}

/// True when the peer negotiated a protocol older than `resultType` (2026-07-28).
pub(crate) fn is_legacy(version: Option<rmcp::model::ProtocolVersion>) -> bool {
    !version.is_some_and(|v| v >= rmcp::model::ProtocolVersion::V_2026_07_28)
}

impl SendLedger {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(State::Unopened),
        }
    }

    /// Read [`SEND_LEDGER_ENV`]; unset or empty means no ledger.
    pub(crate) fn from_env() -> Option<Self> {
        std::env::var_os(SEND_LEDGER_ENV)
            .filter(|v| !v.is_empty())
            .map(|v| Self::new(PathBuf::from(v)))
    }

    /// Append one record. `fields` must be a JSON object; `ts`, `method`
    /// and `session` are added here.
    pub(crate) fn record(&self, method: &str, session: Option<String>, fields: Value) {
        let mut obj = Map::new();
        obj.insert(
            "ts".into(),
            json!(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)),
        );
        obj.insert("method".into(), json!(method));
        if let Some(s) = session {
            obj.insert("session".into(), json!(s));
        }
        if let Value::Object(extra) = fields {
            obj.extend(extra);
        }
        let mut line = match serde_json::to_vec(&Value::Object(obj)) {
            Ok(l) => l,
            Err(_) => return,
        };
        line.push(b'\n');
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*state, State::Unopened) {
            *state = match OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                Ok(f) => State::Open(f),
                Err(e) => {
                    tracing::warn!(path = %self.path.display(), error = %e, "send ledger disabled: cannot open file");
                    State::Failed
                }
            };
        }
        if let State::Open(f) = &mut *state {
            if let Err(e) = f.write_all(&line).and_then(|_| f.flush()) {
                tracing::warn!(path = %self.path.display(), error = %e, "send ledger disabled: write failed");
                *state = State::Failed;
            }
        }
    }
}
