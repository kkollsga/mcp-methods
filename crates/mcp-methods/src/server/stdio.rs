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

use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use futures::SinkExt;
use rmcp::model::{ClientNotification, ErrorData, GetExtensions, JsonRpcMessage, RequestId};
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::async_rw::{JsonRpcMessageCodec, JsonRpcMessageCodecError};
use rmcp::transport::Transport;
use rmcp::RoleServer;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::sync::{Mutex, Notify};
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

/// Outcome of one [`InputBuffer::pump_step`].
#[cfg(unix)]
pub(crate) enum PumpStep {
    /// Bytes were appended (or the call was interrupted): read again.
    Data,
    /// Nothing more to read right now.
    WouldBlock,
    /// End of input or a read error; the buffer is marked finished.
    Eof,
    /// A reload owns the input.
    Frozen,
}

struct InState {
    /// Bytes read from the OS and not yet decoded into a line.
    buf: Vec<u8>,
    eof: bool,
    /// Set while a reload owns the input: the pump must not read, so the
    /// bytes still in the OS buffer stay there for the next process.
    // Only the unix reload path freezes the input; windows never sets it.
    #[cfg_attr(not(unix), allow(dead_code))]
    frozen: bool,
}

/// The single buffer between the OS and the decoder. Every byte the process
/// has read from stdin but not yet decoded is in `state.buf`, so a reload can
/// hand all of it to the next process; the stdin pump appends here while
/// holding the lock across its `read(2)`, so no byte is ever in transit.
pub(crate) struct InputBuffer {
    state: StdMutex<InState>,
    ready: Notify,
    // Awaited by the unix stdin pump only.
    #[cfg_attr(not(unix), allow(dead_code))]
    unfrozen: Notify,
    pump_active: AtomicBool,
    /// Why no pump feeds this buffer, when `stdio()` tried and failed.
    unpumped: StdMutex<Option<String>>,
    #[cfg(unix)]
    pump: StdMutex<Option<crate::server::stdin_pump::PumpHandle>>,
}

impl InputBuffer {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: StdMutex::new(InState {
                buf: Vec::new(),
                eof: false,
                frozen: false,
            }),
            ready: Notify::new(),
            unfrozen: Notify::new(),
            pump_active: AtomicBool::new(false),
            unpumped: StdMutex::new(None),
            #[cfg(unix)]
            pump: StdMutex::new(None),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, InState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the non-consuming stdin pump feeds this buffer.
    pub(crate) fn pump_active(&self) -> bool {
        self.pump_active.load(Ordering::Acquire)
    }

    /// Why the pump could not be used, or a generic reason when none was
    /// attempted (a transport built with `from_io`).
    pub(crate) fn unpumped_reason(&self) -> String {
        self.unpumped
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| "stdin is not pollable (not read through the framework pump)".into())
    }

    fn push(&self, bytes: &[u8]) {
        self.state().buf.extend_from_slice(bytes);
        self.ready.notify_one();
    }

    /// Put `bytes` ahead of anything already buffered (the handover replay
    /// precedes live input).
    pub(crate) fn prepend(&self, bytes: &[u8]) {
        let mut state = self.state();
        let mut joined = Vec::with_capacity(bytes.len() + state.buf.len());
        joined.extend_from_slice(bytes);
        joined.extend_from_slice(&state.buf);
        state.buf = joined;
        drop(state);
        self.ready.notify_one();
    }

    /// The next complete line including its `\n`.
    fn take_line(&self) -> Option<Vec<u8>> {
        let mut state = self.state();
        let end = state.buf.iter().position(|b| *b == b'\n')? + 1;
        Some(state.buf.drain(..end).collect())
    }

    #[cfg(unix)]
    pub(crate) fn mark_eof_external(&self) {
        self.mark_eof();
    }

    fn mark_eof(&self) {
        self.state().eof = true;
        self.ready.notify_one();
    }

    /// At end of input: whatever remains (a final line without `\n`), once.
    fn take_at_eof(&self) -> Option<Vec<u8>> {
        let mut state = self.state();
        state.eof.then(|| std::mem::take(&mut state.buf))
    }

    // Unix reload path only.
    #[cfg_attr(not(unix), allow(dead_code))]
    /// Stop the pump from reading and return every undecoded byte, leaving
    /// the buffer intact so [`unfreeze`](Self::unfreeze) can resume.
    pub(crate) fn freeze_and_snapshot(&self) -> Vec<u8> {
        let mut state = self.state();
        state.frozen = true;
        state.buf.clone()
    }

    // Unix reload path only.
    #[cfg_attr(not(unix), allow(dead_code))]
    /// Resume reading after a reload that did not replace the process.
    pub(crate) fn unfreeze(&self) {
        self.state().frozen = false;
        self.unfrozen.notify_one();
    }

    /// One nonblocking `read(2)` of `fd` into the buffer, under the state
    /// lock so the bytes are never in transit between the OS and the buffer
    /// while a reload snapshots it.
    #[cfg(unix)]
    pub(crate) fn pump_step(&self, fd: std::os::fd::RawFd, chunk: &mut [u8]) -> PumpStep {
        let mut state = self.state();
        if state.frozen {
            return PumpStep::Frozen;
        }
        // SAFETY: `chunk` is a valid writable region of `chunk.len()` bytes.
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        let step = if n > 0 {
            state.buf.extend_from_slice(&chunk[..n as usize]);
            PumpStep::Data
        } else if n == 0 {
            state.eof = true;
            PumpStep::Eof
        } else {
            match std::io::Error::last_os_error().kind() {
                std::io::ErrorKind::WouldBlock => PumpStep::WouldBlock,
                std::io::ErrorKind::Interrupted => PumpStep::Data,
                _ => {
                    state.eof = true;
                    PumpStep::Eof
                }
            }
        };
        drop(state);
        if matches!(step, PumpStep::Data | PumpStep::Eof) {
            self.ready.notify_one();
        }
        step
    }

    /// Resolves when a reload that froze the input has been abandoned.
    #[cfg(unix)]
    pub(crate) async fn wait_unfrozen(&self) {
        self.unfrozen.notified().await;
    }

    // Unix reload path only.
    #[cfg_attr(not(unix), allow(dead_code))]
    /// Restore the descriptor's original mode ahead of an `exec`.
    pub(crate) fn restore_blocking(&self) {
        #[cfg(unix)]
        if let Some(handle) = self.pump.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            handle.restore_flags();
        }
    }

    // Unix reload path only.
    #[cfg_attr(not(unix), allow(dead_code))]
    /// Re-enter nonblocking mode after an `exec` that failed.
    pub(crate) fn reapply_nonblocking(&self) {
        #[cfg(unix)]
        if let Some(handle) = self.pump.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            handle.reapply_nonblocking();
        }
    }

    /// Stop the pump and give stdin back its original blocking mode.
    fn shutdown(&self) {
        #[cfg(unix)]
        if let Some(handle) = self.pump.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.stop();
        }
    }
}

/// Inbound requests whose response or error has not yet been written.
///
/// A request is outstanding from the moment the transport decodes it until
/// the transport has written (and flushed) its answer, which is later than
/// the handler returning: rmcp hands the answer to a spawned task. A reload
/// uses this to know no answer is still on its way out.
pub(crate) struct Outstanding {
    ids: StdMutex<HashSet<RequestId>>,
    changed: Notify,
}

impl Outstanding {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            ids: StdMutex::new(HashSet::new()),
            changed: Notify::new(),
        })
    }

    fn ids(&self) -> std::sync::MutexGuard<'_, HashSet<RequestId>> {
        self.ids.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn begin(&self, id: RequestId) {
        self.ids().insert(id);
    }

    fn finish(&self, id: &RequestId) {
        self.ids().remove(id);
        self.changed.notify_waiters();
    }

    /// Track a decoded inbound message: a request becomes outstanding, and a
    /// cancellation retires its request (rmcp need not answer a cancelled one).
    fn observe_inbound(&self, message: &RxJsonRpcMessage<RoleServer>) {
        match message {
            JsonRpcMessage::Request(request) => self.begin(request.id.clone()),
            JsonRpcMessage::Notification(notification) => {
                if let ClientNotification::CancelledNotification(cancelled) =
                    &notification.notification
                {
                    if let Some(id) = &cancelled.params.request_id {
                        self.finish(id);
                    }
                }
            }
            _ => {}
        }
    }

    /// Wait until no request other than `own` is outstanding; `false` when
    /// `within` elapses first. Answers keep flowing meanwhile: nothing here
    /// holds the writer.
    // Unix reload path only.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) async fn wait_until_only(&self, own: Option<&RequestId>, within: Duration) -> bool {
        let settled = async {
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.ids().iter().all(|id| Some(id) == own) {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(within, settled).await.is_ok()
    }
}

/// Pause point after a request that must be handled before any later input is
/// decoded (a reload): the transport stops delivering messages until the
/// handler calls [`release`](Self::release), so everything after that
/// request is still undecoded when the reload hands the input over.
pub(crate) struct Gate {
    paused: AtomicBool,
    changed: Notify,
}

/// Longest the transport waits for a paused gate; a handler that never
/// releases it (a request that fails before dispatch) must not wedge input.
const GATE_TIMEOUT: Duration = Duration::from_secs(60);

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            paused: AtomicBool::new(false),
            changed: Notify::new(),
        })
    }

    fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    pub(crate) fn release(&self) {
        self.paused.store(false, Ordering::Release);
        self.changed.notify_one();
    }

    async fn wait(&self) {
        let waiting = async {
            while self.paused.load(Ordering::Acquire) {
                self.changed.notified().await;
            }
        };
        if tokio::time::timeout(GATE_TIMEOUT, waiting).await.is_err() {
            tracing::warn!("input gate not released within {GATE_TIMEOUT:?}; resuming");
            self.paused.store(false, Ordering::Release);
        }
    }
}

/// Holds the output side exclusively: nothing more is written while the
/// returned guard lives.
pub(crate) trait WriterHold: Send + Sync {
    /// Flush, then take the writer lock.
    // Unix reload path only.
    #[cfg_attr(not(unix), allow(dead_code))]
    fn hold(
        &self,
    ) -> futures::future::BoxFuture<'static, std::io::Result<Box<dyn std::any::Any + Send>>>;
}

impl<W: AsyncWrite + Unpin + Send + 'static> WriterHold for SharedWriter<W> {
    fn hold(
        &self,
    ) -> futures::future::BoxFuture<'static, std::io::Result<Box<dyn std::any::Any + Send>>> {
        let writer = self.clone();
        Box::pin(async move {
            let mut guard = writer.lock_owned().await;
            match guard.as_mut() {
                Some(framed) => {
                    futures::SinkExt::<TxJsonRpcMessage<RoleServer>>::flush(framed).await?
                }
                None => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotConnected,
                        "Transport is closed",
                    ))
                }
            }
            Ok(Box::new(guard) as Box<dyn std::any::Any + Send>)
        })
    }
}

/// Type-erased handles a reload needs from a running transport.
#[derive(Clone)]
pub(crate) struct TransportLink {
    pub(crate) input: Arc<InputBuffer>,
    pub(crate) gate: Arc<Gate>,
    pub(crate) outstanding: Arc<Outstanding>,
    // Held by the unix reload path only.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) writer: Arc<dyn WriterHold>,
}

type PauseAfter = Arc<dyn Fn(&RxJsonRpcMessage<RoleServer>) -> bool + Send + Sync>;

/// rmcp server transport over any async reader/writer pair, keeping raw
/// request text. Build with [`StdioTransport::stdio`] for the process's
/// stdin/stdout or [`StdioTransport::from_io`] for any other stream.
///
/// All undecoded input lives in one shared buffer; on unix,
/// [`stdio`](Self::stdio) fills it from a non-consuming pump on fd 0 so that
/// a reload can carry unread input into the replacement process. The writer
/// lives behind a shared lock so `send` futures (which outlive `&mut self`)
/// can reach it.
pub struct StdioTransport<R, W> {
    reader: Option<R>,
    input: Arc<InputBuffer>,
    gate: Arc<Gate>,
    outstanding: Arc<Outstanding>,
    pause_after: Option<PauseAfter>,
    decoder: RawLineCodec<RxJsonRpcMessage<RoleServer>>,
    write: SharedWriter<W>,
    recover: Option<RecoverFn>,
}

impl StdioTransport<tokio::io::Stdin, tokio::io::Stdout> {
    /// Transport over the process's stdin and stdout.
    ///
    /// On unix, inside a tokio runtime, stdin is polled by a pump that never
    /// consumes more than it buffers, which is what makes
    /// `ping(reload=true)` available. The pump switches fd 0 to
    /// `O_NONBLOCK`, a flag shared with every process holding the same open
    /// file description, so it is not used on a terminal (a signal death
    /// would leave the user's shell nonblocking). When fd 0 is a terminal or
    /// cannot be registered (a regular file), or there is no runtime, input
    /// is read with tokio's blocking stdin and reload reports itself
    /// unavailable.
    pub fn stdio() -> Self {
        #[cfg(unix)]
        {
            match Self::pumped(0, tokio::io::stdout()) {
                Ok(transport) => transport,
                Err(error) => {
                    tracing::info!("stdin pump unavailable, reload disabled: {error}");
                    let transport = Self::from_io(tokio::io::stdin(), tokio::io::stdout());
                    *transport
                        .input
                        .unpumped
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
                    transport
                }
            }
        }
        #[cfg(not(unix))]
        {
            Self::from_io(tokio::io::stdin(), tokio::io::stdout())
        }
    }
}

impl<R, W> StdioTransport<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    fn assemble(reader: Option<R>, input: Arc<InputBuffer>, write: W) -> Self {
        Self {
            reader,
            input,
            gate: Gate::new(),
            outstanding: Outstanding::new(),
            pause_after: None,
            decoder: RawLineCodec::default(),
            write: Arc::new(Mutex::new(Some(FramedWrite::new(
                write,
                JsonRpcMessageCodec::default(),
            )))),
            recover: None,
        }
    }

    /// Transport reading file descriptor `fd` through the stdin pump.
    #[cfg(unix)]
    pub(crate) fn pumped(fd: std::os::fd::RawFd, write: W) -> std::io::Result<Self> {
        let input = InputBuffer::new();
        let handle = crate::server::stdin_pump::start(fd, input.clone())?;
        *input.pump.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        input.pump_active.store(true, Ordering::Release);
        Ok(Self::assemble(None, input, write))
    }

    /// Transport over an arbitrary reader/writer pair.
    pub fn from_io(read: R, write: W) -> Self {
        Self::assemble(Some(read), InputBuffer::new(), write)
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

    /// After delivering a message `pred` accepts, hold back all further input
    /// until [`Gate::release`] is called on this transport's gate.
    pub(crate) fn with_pause_after(
        mut self,
        pred: impl Fn(&RxJsonRpcMessage<RoleServer>) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.pause_after = Some(Arc::new(pred));
        self
    }

    /// Put `bytes` ahead of all pending input.
    pub(crate) fn prepend_input(&self, bytes: &[u8]) {
        self.input.prepend(bytes);
    }

    /// Shared handles for a reload to take over this transport's streams.
    pub(crate) fn link(&self) -> TransportLink
    where
        W: Send + 'static,
    {
        TransportLink {
            input: self.input.clone(),
            gate: self.gate.clone(),
            outstanding: self.outstanding.clone(),
            writer: Arc::new(self.write.clone()),
        }
    }

    /// The next line (with its `\n`, or the unterminated tail at end of
    /// input) and whether it is that tail; `None` once input is exhausted.
    async fn next_line(&mut self) -> Option<(Vec<u8>, bool)> {
        let mut chunk = [0u8; 8192];
        loop {
            if let Some(line) = self.input.take_line() {
                return Some((line, false));
            }
            if let Some(tail) = self.input.take_at_eof() {
                return (!tail.is_empty()).then_some((tail, true));
            }
            match self.reader.as_mut() {
                Some(reader) => match reader.read(&mut chunk).await {
                    Ok(0) => self.input.mark_eof(),
                    Ok(n) => self.input.push(&chunk[..n]),
                    Err(error) => {
                        tracing::error!("Error reading from stream: {error}");
                        return None;
                    }
                },
                None => self.input.ready.notified().await,
            }
        }
    }
}

impl<R, W> Drop for StdioTransport<R, W> {
    fn drop(&mut self) {
        self.input.shutdown();
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
        let outstanding = self.outstanding.clone();
        let answered = match &item {
            JsonRpcMessage::Response(response) => Some(response.id.clone()),
            JsonRpcMessage::Error(error) => error.id.clone(),
            _ => None,
        };
        async move {
            let sent = {
                let mut guard = write.lock().await;
                match guard.as_mut() {
                    Some(writer) => writer.send(item).await.map_err(Into::into),
                    None => Err(std::io::Error::new(
                        std::io::ErrorKind::NotConnected,
                        "Transport is closed",
                    )),
                }
            };
            if let Some(id) = answered {
                outstanding.finish(&id);
            }
            sent
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        loop {
            self.gate.wait().await;
            let (line, at_eof) = self.next_line().await?;
            let decoded = if at_eof {
                let mut final_line = BytesMut::from(line.as_slice());
                self.decoder.decode_eof(&mut final_line)
            } else {
                let mut complete_line = BytesMut::from(line.as_slice());
                self.decoder.decode(&mut complete_line)
            };
            let raw = raw_text(line.strip_suffix(b"\n").unwrap_or(&line)).ok();
            match decoded {
                Ok(Some((mut message, raw))) => {
                    attach_raw(&mut message, raw, false);
                    self.outstanding.observe_inbound(&message);
                    if self.pause_after.as_ref().is_some_and(|pred| pred(&message)) {
                        self.gate.pause();
                    }
                    return Some(message);
                }
                Ok(None) => continue,
                Err(JsonRpcMessageCodecError::Serde(error)) => {
                    if let (Some(raw), Some(recover)) = (raw, &self.recover) {
                        if let Some(mut message) = recover(&raw) {
                            attach_raw(&mut message, raw, true);
                            self.outstanding.observe_inbound(&message);
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
    use std::time::Duration;
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

    #[tokio::test]
    async fn undecoded_bytes_are_all_in_the_shared_buffer() {
        // The handover data path: after one message is decoded, everything
        // the transport has read but not decoded (the next message and a
        // partial line) is what a reload carries over.
        let (mut t, mut tx, _rx) = pair();
        let second = r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;
        tx.write_all(format!("{PING}\n{second}\n{{\"par").as_bytes())
            .await
            .unwrap();
        assert_eq!(&*raw_of(&t.receive().await.unwrap()).source, PING);
        let link = t.link();
        let pending = link.input.freeze_and_snapshot();
        assert_eq!(pending, format!("{second}\n{{\"par").into_bytes());
    }

    #[tokio::test]
    async fn prepended_replay_is_decoded_before_live_input() {
        let (mut t, mut tx, _rx) = pair();
        tx.write_all(format!("{PING}\n").as_bytes()).await.unwrap();
        t.prepend_input(b"{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\"}\n");
        let first = raw_of(&t.receive().await.unwrap());
        assert!(first.source.contains("\"id\":9"));
        assert_eq!(&*raw_of(&t.receive().await.unwrap()).source, PING);
    }

    #[tokio::test]
    async fn gate_holds_back_input_after_a_matching_message_until_released() {
        let (t, mut tx, _rx) = pair();
        let mut t = t.with_pause_after(|_| true);
        let second = r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;
        tx.write_all(format!("{PING}\n{second}\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(&*raw_of(&t.receive().await.unwrap()).source, PING);
        let gate = t.link().gate;
        let held = tokio::time::timeout(Duration::from_millis(150), t.receive()).await;
        assert!(held.is_err(), "second message must wait for the gate");
        gate.release();
        assert_eq!(&*raw_of(&t.receive().await.unwrap()).source, second);
    }

    #[tokio::test]
    async fn requests_stay_outstanding_until_their_answer_is_written() {
        use rmcp::model::{NumberOrString, ServerResult};
        let (mut t, mut tx, _rx) = pair();
        let link = t.link();
        let own = NumberOrString::Number(1);
        tx.write_all(format!("{PING}\n").as_bytes()).await.unwrap();
        t.receive().await.unwrap();
        let short = Duration::from_millis(60);
        assert!(!link.outstanding.wait_until_only(None, short).await);
        assert!(link.outstanding.wait_until_only(Some(&own), short).await);

        // A waiter is woken by the answer being written, not by the handler.
        let outstanding = link.outstanding.clone();
        let waiter = tokio::spawn(async move {
            outstanding
                .wait_until_only(None, Duration::from_secs(5))
                .await
        });
        tokio::task::yield_now().await;
        let reply = TxJsonRpcMessage::<RoleServer>::response(ServerResult::empty(()), own);
        t.send(reply).await.unwrap();
        assert!(waiter.await.unwrap());
    }

    #[tokio::test]
    async fn an_error_answer_and_a_cancellation_each_retire_their_request() {
        let (mut t, mut tx, _rx) = pair();
        let link = t.link();
        let second = r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;
        tx.write_all(format!("{PING}\n{second}\n").as_bytes())
            .await
            .unwrap();
        t.receive().await.unwrap();
        t.receive().await.unwrap();
        let reply = TxJsonRpcMessage::<RoleServer>::error(
            ErrorData::internal_error("boom", None),
            Some(rmcp::model::NumberOrString::Number(1)),
        );
        t.send(reply).await.unwrap();
        let short = Duration::from_millis(60);
        assert!(!link.outstanding.wait_until_only(None, short).await);
        tx.write_all(
            br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}}"#,
        )
        .await
        .unwrap();
        tx.write_all(b"\n").await.unwrap();
        t.receive().await.unwrap();
        assert!(link.outstanding.wait_until_only(None, short).await);
    }

    #[cfg(unix)]
    mod pump {
        use super::*;
        use std::io::Write;
        use std::os::fd::{AsRawFd, FromRawFd};

        fn pipe() -> (std::fs::File, std::os::fd::OwnedFd) {
            let mut fds = [0i32; 2];
            // SAFETY: `fds` has room for the two descriptors `pipe(2)` writes.
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            // SAFETY: both descriptors are fresh and owned by nobody else.
            unsafe {
                (
                    std::fs::File::from_raw_fd(fds[1]),
                    std::os::fd::OwnedFd::from_raw_fd(fds[0]),
                )
            }
        }

        fn flags(fd: i32) -> i32 {
            // SAFETY: F_GETFL takes no pointer argument.
            unsafe { libc::fcntl(fd, libc::F_GETFL) }
        }

        #[tokio::test]
        async fn freezing_leaves_later_bytes_in_the_os_pipe() {
            let (mut write, read) = pipe();
            let fd = read.as_raw_fd();
            let blocking = flags(fd);
            assert_eq!(blocking & libc::O_NONBLOCK, 0);
            let (server_tx, _client_rx) = duplex(4096);
            let mut t = StdioTransport::<tokio::io::Empty, _>::pumped(fd, server_tx).unwrap();
            assert_ne!(flags(fd) & libc::O_NONBLOCK, 0, "pump polls nonblocking");
            assert!(t.link().input.pump_active());

            write
                .write_all(format!("{PING}\n{{\"half").as_bytes())
                .unwrap();
            assert_eq!(&*raw_of(&t.receive().await.unwrap()).source, PING);
            // The partial line has reached the buffer once the pump caught up.
            let link = t.link();
            let snapshot = loop {
                let snap = link.input.freeze_and_snapshot();
                if snap == b"{\"half" {
                    break snap;
                }
                link.input.unfreeze();
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
            assert_eq!(snapshot, b"{\"half");

            // Frozen: a later write is NOT consumed by this process.
            write.write_all(b"LATER\n").unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(link.input.freeze_and_snapshot(), b"{\"half");
            link.input.restore_blocking();
            assert_eq!(
                flags(fd) & libc::O_NONBLOCK,
                0,
                "original mode restored for exec"
            );
            let mut leftover = [0u8; 16];
            // SAFETY: valid buffer; the descriptor is blocking but data is waiting.
            let n = unsafe { libc::read(fd, leftover.as_mut_ptr().cast(), leftover.len()) };
            assert_eq!(&leftover[..n as usize], b"LATER\n");

            // An abandoned reload resumes reading.
            link.input.reapply_nonblocking();
            link.input.unfreeze();
            write.write_all(b"\n").unwrap();
            let line = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(l) = link.input.take_line() {
                        break l;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(line, b"{\"half\n");
            drop(t);
            assert_eq!(
                flags(fd) & libc::O_NONBLOCK,
                0,
                "dropping the transport restores stdin"
            );
        }

        #[tokio::test]
        async fn a_terminal_is_never_pumped_and_keeps_its_blocking_mode() {
            let (mut master, mut slave) = (0i32, 0i32);
            // SAFETY: both out-pointers are valid; the optional arguments are null.
            let opened = unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    // The termios/winsize pointer mutability differs by libc target.
                    std::ptr::null::<u8>() as _,
                    std::ptr::null::<u8>() as _,
                )
            };
            assert_eq!(opened, 0, "openpty");
            // SAFETY: openpty returned two fresh descriptors nobody else owns.
            let (_master, slave) = unsafe {
                (
                    std::os::fd::OwnedFd::from_raw_fd(master),
                    std::os::fd::OwnedFd::from_raw_fd(slave),
                )
            };
            let fd = slave.as_raw_fd();
            assert!(crate::server::stdin_pump::is_terminal(fd));
            let before = flags(fd);
            assert_eq!(before & libc::O_NONBLOCK, 0);
            let (server_tx, _client_rx) = duplex(64);
            let error = StdioTransport::<tokio::io::Empty, _>::pumped(fd, server_tx)
                .err()
                .expect("a terminal is refused");
            assert!(error.to_string().contains("terminal"), "{error}");
            assert_eq!(flags(fd), before, "never switched to O_NONBLOCK");
            let (_write, read) = pipe();
            assert!(!crate::server::stdin_pump::is_terminal(read.as_raw_fd()));
        }

        #[tokio::test]
        async fn unpollable_descriptor_is_rejected_and_left_untouched() {
            let file = std::fs::File::open("/dev/null").unwrap();
            let fd = file.as_raw_fd();
            let before = flags(fd);
            let (server_tx, _client_rx) = duplex(64);
            let result = StdioTransport::<tokio::io::Empty, _>::pumped(fd, server_tx);
            assert!(result.is_err());
            assert_eq!(flags(fd), before);
        }
    }
}
