//! Newline-delimited JSON-RPC stdio transport that keeps each request's text.
//!
//! rmcp's own `transport::stdio()` decodes a line and discards it. Raw tool
//! routes need the exact bytes the client sent (key order, whitespace, numbers
//! that do not survive a `serde_json::Value` round trip), so [`StdioTransport`]
//! attaches a [`RawJsonRpcRequest`] to every decoded request's extensions.
//! Handlers read it from `context.request_context.extensions`.
//!
//! Framing matches rmcp's `JsonRpcMessageCodec`: one message per `\n`-terminated
//! line, a trailing `\r` and a leading UTF-8 BOM tolerated, empty lines skipped,
//! a final line without `\n` decoded at EOF. A line that fails to decode is
//! offered to the optional [`StdioTransport::with_recover`] hook; without a
//! recovery, a syntactically broken line (including bad UTF-8 inside a JSON
//! string) is logged and skipped, while well-formed JSON that is not a valid
//! message is answered with an `invalid_request` error carrying a null id.

use std::future::Future;
use std::sync::Arc;

use futures::SinkExt;
use rmcp::model::{ErrorData, GetExtensions, JsonRpcMessage};
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::async_rw::{JsonRpcMessageCodec, JsonRpcMessageCodecError};
use rmcp::transport::Transport;
use rmcp::RoleServer;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::Mutex;
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, FramedWrite};

/// Exact JSON-RPC request text, available to handlers for one request.
///
/// Inserted into the request's extensions by [`StdioTransport`]. `source` is
/// the line as received, minus the line terminator, a trailing `\r` and a
/// leading BOM.
#[derive(Clone, Debug)]
pub struct RawJsonRpcRequest {
    /// The request line exactly as sent.
    pub source: Arc<str>,
    /// `true` when the request only decoded through the
    /// [`StdioTransport::with_recover`] hook, so `source` is not what the
    /// typed message was parsed from.
    pub recovered: bool,
}

/// Recovery for a line that failed to decode: given the raw line, return the
/// message to deliver instead, or `None` to fall back to the default handling.
pub type RecoverFn =
    Arc<dyn Fn(&str) -> Option<RxJsonRpcMessage<RoleServer>> + Send + Sync + 'static>;

/// [`JsonRpcMessageCodec`] wrapper that also returns each decoded line's text.
struct RawLineCodec<T> {
    inner: JsonRpcMessageCodec<T>,
}

impl<T> Default for RawLineCodec<T> {
    fn default() -> Self {
        Self {
            inner: JsonRpcMessageCodec::default(),
        }
    }
}

impl<T: serde::de::DeserializeOwned> Decoder for RawLineCodec<T> {
    type Item = (T, Arc<str>);
    type Error = JsonRpcMessageCodecError;

    fn decode(&mut self, input: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            let before = input.len();
            let raw = input
                .iter()
                .position(|byte| *byte == b'\n')
                .map(|end| input[..end].to_vec());
            match self.inner.decode(input)? {
                Some(message) => {
                    let raw = raw.expect("the line codec returned a complete line");
                    return Ok(Some((message, raw_text(&raw)?)));
                }
                // The inner codec consumed a line without yielding (an empty
                // line); look at the next one.
                None if input.len() < before => continue,
                None => return Ok(None),
            }
        }
    }

    fn decode_eof(&mut self, input: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let raw = input.as_ref().strip_suffix(b"\r").unwrap_or(input.as_ref());
        let raw = raw_text(raw)?;
        self.inner
            .decode_eof(input)
            .map(|message| message.map(|message| (message, raw)))
    }
}

/// Line bytes (no `\n`) to text: drops a trailing `\r` and a leading BOM;
/// invalid UTF-8 surfaces as the codec's `Serde` (`Io` category) error.
fn raw_text(line: &[u8]) -> Result<Arc<str>, JsonRpcMessageCodecError> {
    const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let line = line.strip_prefix(UTF8_BOM).unwrap_or(line);
    std::str::from_utf8(line).map(Arc::from).map_err(|error| {
        serde_json::Error::io(std::io::Error::new(std::io::ErrorKind::InvalidData, error)).into()
    })
}

type ServerMessageCodec = JsonRpcMessageCodec<TxJsonRpcMessage<RoleServer>>;
pub(crate) type SharedWriter<W> = Arc<Mutex<Option<FramedWrite<W, ServerMessageCodec>>>>;

/// rmcp server transport over any async reader/writer pair, keeping raw
/// request text. Build with [`StdioTransport::stdio`] for the process's
/// stdin/stdout or [`StdioTransport::from_io`] for any other stream.
///
/// The reader type `R` is the swappable input side; the writer lives behind a
/// shared lock so `send` futures (which outlive `&mut self`) can reach it.
pub struct StdioTransport<R, W> {
    read: BufReader<R>,
    line: Vec<u8>,
    decoder: RawLineCodec<RxJsonRpcMessage<RoleServer>>,
    write: SharedWriter<W>,
    recover: Option<RecoverFn>,
}

impl StdioTransport<tokio::io::Stdin, tokio::io::Stdout> {
    /// Transport over the process's stdin and stdout.
    pub fn stdio() -> Self {
        Self::from_io(tokio::io::stdin(), tokio::io::stdout())
    }
}

impl<R, W> StdioTransport<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Transport over an arbitrary reader/writer pair.
    pub fn from_io(read: R, write: W) -> Self {
        Self {
            read: BufReader::new(read),
            line: Vec::new(),
            decoder: RawLineCodec::default(),
            write: Arc::new(Mutex::new(Some(FramedWrite::new(
                write,
                JsonRpcMessageCodec::default(),
            )))),
            recover: None,
        }
    }

    /// Offer lines that fail to decode to `recover` before the default
    /// handling. The hook receives the raw line (BOM and `\r` stripped); a
    /// returned request is delivered with `RawJsonRpcRequest { recovered: true }`.
    pub fn with_recover(
        mut self,
        recover: impl Fn(&str) -> Option<RxJsonRpcMessage<RoleServer>> + Send + Sync + 'static,
    ) -> Self {
        self.recover = Some(Arc::new(recover));
        self
    }

    /// The shared writer, for a holder that must serialise against `send`.
    #[allow(dead_code)] // reason: consumed by the reload handover, not yet wired
    pub(crate) fn writer(&self) -> SharedWriter<W> {
        self.write.clone()
    }
}

fn attach_raw(message: &mut RxJsonRpcMessage<RoleServer>, source: Arc<str>, recovered: bool) {
    if let JsonRpcMessage::Request(request) = message {
        request
            .request
            .extensions_mut()
            .insert(RawJsonRpcRequest { source, recovered });
    }
}

impl<R, W> Transport<RoleServer> for StdioTransport<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let write = self.write.clone();
        async move {
            let mut guard = write.lock().await;
            match guard.as_mut() {
                Some(writer) => writer.send(item).await.map_err(Into::into),
                None => Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "Transport is closed",
                )),
            }
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        loop {
            let decoded = match self.read.read_until(b'\n', &mut self.line).await {
                Ok(0) if self.line.is_empty() => return None,
                Ok(0) => {
                    let mut final_line = BytesMut::from(self.line.as_slice());
                    self.decoder.decode_eof(&mut final_line)
                }
                // A partial line (reader returned before `\n`): keep
                // accumulating into `self.line`.
                Ok(_) if !self.line.ends_with(b"\n") => continue,
                Ok(_) => {
                    let mut complete_line = BytesMut::from(self.line.as_slice());
                    self.decoder.decode(&mut complete_line)
                }
                Err(error) => {
                    tracing::error!("Error reading from stream: {error}");
                    return None;
                }
            };
            let raw = raw_text(self.line.strip_suffix(b"\n").unwrap_or(&self.line)).ok();
            self.line.clear();
            match decoded {
                Ok(Some((mut message, raw))) => {
                    attach_raw(&mut message, raw, false);
                    return Some(message);
                }
                Ok(None) => continue,
                Err(JsonRpcMessageCodecError::Serde(error)) => {
                    if let (Some(raw), Some(recover)) = (raw, &self.recover) {
                        if let Some(mut message) = recover(&raw) {
                            attach_raw(&mut message, raw, true);
                            return Some(message);
                        }
                    }
                    match error.classify() {
                        serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
                            tracing::debug!("Ignoring unparsable incoming message: {error}");
                        }
                        serde_json::error::Category::Data | serde_json::error::Category::Io => {
                            let mut guard = self.write.lock().await;
                            let writer = guard.as_mut()?;
                            let response = TxJsonRpcMessage::<RoleServer>::error(
                                ErrorData::invalid_request("Invalid request", None),
                                None,
                            );
                            if writer.send(response).await.is_err() {
                                return None;
                            }
                        }
                    }
                }
                Err(error) => {
                    tracing::error!("Error reading from stream: {error}");
                    return None;
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.write.lock().await.take();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ClientRequest;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream};

    type T = StdioTransport<DuplexStream, DuplexStream>;

    /// Transport plus the client's write half (`tx`) and read half (`rx`).
    fn pair() -> (T, DuplexStream, DuplexStream) {
        let (client_tx, server_rx) = duplex(4096);
        let (server_tx, client_rx) = duplex(4096);
        (
            StdioTransport::from_io(server_rx, server_tx),
            client_tx,
            client_rx,
        )
    }

    fn raw_of(message: &RxJsonRpcMessage<RoleServer>) -> RawJsonRpcRequest {
        match message {
            JsonRpcMessage::Request(request) => request
                .request
                .extensions()
                .get::<RawJsonRpcRequest>()
                .expect("raw request attached")
                .clone(),
            other => panic!("expected a request, got {other:?}"),
        }
    }

    const PING: &str = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    #[tokio::test]
    async fn request_text_is_retained_exactly() {
        let (mut t, mut tx, _rx) = pair();
        let line = r#"{ "method" : "ping",   "id":7, "jsonrpc":"2.0" ,"x":1.0e0}"#;
        tx.write_all(format!("{line}\n").as_bytes()).await.unwrap();
        let message = t.receive().await.unwrap();
        let raw = raw_of(&message);
        assert_eq!(&*raw.source, line);
        assert!(!raw.recovered);
    }

    #[tokio::test]
    async fn several_messages_in_one_chunk() {
        let (mut t, mut tx, _rx) = pair();
        let second = r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;
        tx.write_all(format!("{PING}\n{second}\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(&*raw_of(&t.receive().await.unwrap()).source, PING);
        assert_eq!(&*raw_of(&t.receive().await.unwrap()).source, second);
    }

    #[tokio::test]
    async fn message_split_across_reads() {
        let (mut t, mut tx, _rx) = pair();
        let (head, tail) = PING.split_at(10);
        tx.write_all(head.as_bytes()).await.unwrap();
        let writer = tokio::spawn(async move {
            tokio::task::yield_now().await;
            tx.write_all(format!("{tail}\n").as_bytes()).await.unwrap();
            tx
        });
        let message = t.receive().await.unwrap();
        assert_eq!(&*raw_of(&message).source, PING);
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn crlf_and_bom_are_stripped_from_the_text() {
        let (mut t, mut tx, _rx) = pair();
        tx.write_all(b"\xEF\xBB\xBF").await.unwrap();
        tx.write_all(format!("{PING}\r\n").as_bytes())
            .await
            .unwrap();
        let message = t.receive().await.unwrap();
        assert_eq!(&*raw_of(&message).source, PING);
        assert!(matches!(
            message,
            JsonRpcMessage::Request(ref r) if matches!(r.request, ClientRequest::PingRequest(_))
        ));
    }

    #[tokio::test]
    async fn empty_lines_are_skipped() {
        let (mut t, mut tx, _rx) = pair();
        tx.write_all(format!("\n\r\n\n{PING}\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(&*raw_of(&t.receive().await.unwrap()).source, PING);
    }

    #[tokio::test]
    async fn eof_without_trailing_newline_decodes_the_last_line() {
        let (mut t, mut tx, _rx) = pair();
        tx.write_all(PING.as_bytes()).await.unwrap();
        drop(tx);
        assert_eq!(&*raw_of(&t.receive().await.unwrap()).source, PING);
        assert!(t.receive().await.is_none());
    }

    #[tokio::test]
    async fn eof_with_no_pending_bytes_ends_the_stream() {
        let (mut t, tx, _rx) = pair();
        drop(tx);
        assert!(t.receive().await.is_none());
    }

    /// Everything the server wrote, after closing both transport halves.
    async fn drain(t: T, mut rx: DuplexStream) -> Vec<u8> {
        drop(t);
        let mut out = Vec::new();
        rx.read_to_end(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn invalid_utf8_line_is_dropped_and_the_stream_continues() {
        // serde_json classifies bad UTF-8 inside a string as a syntax error,
        // so (as in the original transport) it is skipped without a reply.
        let (mut t, mut tx, rx) = pair();
        tx.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"p\xFF\"}\n")
            .await
            .unwrap();
        tx.write_all(format!("{PING}\n").as_bytes()).await.unwrap();
        assert_eq!(&*raw_of(&t.receive().await.unwrap()).source, PING);
        assert!(drain(t, rx).await.is_empty());
    }

    #[tokio::test]
    async fn json_that_is_not_a_request_gets_an_invalid_request_reply() {
        let (mut t, mut tx, rx) = pair();
        tx.write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":1}}\n{PING}\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(&*raw_of(&t.receive().await.unwrap()).source, PING);
        let reply: serde_json::Value = serde_json::from_slice(&drain(t, rx).await).unwrap();
        assert_eq!(reply["id"], serde_json::Value::Null);
        assert_eq!(reply["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn syntactically_broken_line_is_skipped_silently() {
        let (mut t, mut tx, rx) = pair();
        tx.write_all(format!("{{not json\n{PING}\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(&*raw_of(&t.receive().await.unwrap()).source, PING);
        let rest = drain(t, rx).await;
        assert!(rest.is_empty(), "no reply for a syntax error: {rest:?}");
    }

    #[tokio::test]
    async fn recover_hook_supplies_the_message_and_flags_it() {
        let (t, mut tx, _rx) = pair();
        let mut t = t.with_recover(|raw| {
            // Accept a line the strict decoder rejects, rewriting it.
            raw.contains("NaN")
                .then(|| serde_json::from_str(&raw.replace("NaN", "0")).expect("recovered decode"))
        });
        let bad = r#"{"jsonrpc":"2.0","id":3,"method":"ping","params":{"v":NaN}}"#;
        tx.write_all(format!("{bad}\n").as_bytes()).await.unwrap();
        let raw = raw_of(&t.receive().await.unwrap());
        assert_eq!(&*raw.source, bad);
        assert!(raw.recovered);
    }

    #[tokio::test]
    async fn recover_hook_declining_falls_back_to_default_handling() {
        let (t, mut tx, _rx) = pair();
        let mut t = t.with_recover(|_| None);
        tx.write_all(format!("{{not json\n{PING}\n").as_bytes())
            .await
            .unwrap();
        assert!(!raw_of(&t.receive().await.unwrap()).recovered);
    }

    #[tokio::test]
    async fn responses_are_written_one_per_line_and_close_stops_sends() {
        let (mut t, _tx, mut rx) = pair();
        for id in 1..=2 {
            let reply = TxJsonRpcMessage::<RoleServer>::error(
                ErrorData::invalid_request("x", None),
                Some(rmcp::model::NumberOrString::Number(id)),
            );
            t.send(reply).await.unwrap();
        }
        let mut buf = vec![0u8; 1024];
        let n = rx.read(&mut buf).await.unwrap();
        let text = std::str::from_utf8(&buf[..n]).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text:?}");
        assert!(text.ends_with('\n'));
        for (i, line) in lines.iter().enumerate() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["id"], i as i64 + 1);
        }
        t.close().await.unwrap();
        let again =
            TxJsonRpcMessage::<RoleServer>::error(ErrorData::invalid_request("x", None), None);
        let error = t.send(again).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotConnected);
    }
}
