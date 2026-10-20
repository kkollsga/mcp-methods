//! In-place reload: `ping(reload=true)` replaces the running process image
//! with the newly installed build and continues the same MCP session.
//!
//! Handover by replay. The old process takes the writer lock, freezes the
//! stdin pump, and writes a private file holding the client's `initialize`
//! peer info, the exact `ping(reload=true)` request line, every stdin byte it
//! had read but not decoded, the tool names and its version. It then
//! `exec`s the configured argv with `MCP_METHODS_RELOAD_HANDOVER` pointing at that file
//! (same PID, same stdio). The new process, entered through
//! [`McpServer::serve_with_handover`], reads and deletes the file, serves
//! with the recorded peer info instead of a handshake, and feeds the replay
//! bytes ahead of live stdin, so the replayed ping is answered with
//! `reloaded: <old> → <new>` and any request pipelined behind it is
//! answered normally. State that lived in the old process (retained
//! response ids, loaded skills, workspace and session state) is gone and
//! the answer says so.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientRequest, ContentBlock, ErrorData, JsonRpcMessage,
    NumberOrString,
};
use rmcp::service::{RequestContext, RunningService, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::RoleServer;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::server::self_check::SelfCheckState;
use crate::server::server::McpServer;
use crate::server::stdio::{Gate, RawJsonRpcRequest, StdioTransport};

/// Environment variable naming the handover file for the next process.
pub(crate) const HANDOVER_ENV: &str = "MCP_METHODS_RELOAD_HANDOVER";

/// What the old process leaves for the new one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Handover {
    /// The client's `initialize` parameters (an `InitializeRequestParams`).
    pub(crate) peer_info: serde_json::Value,
    /// The `ping(reload=true)` request line exactly as received.
    pub(crate) replay_line: String,
    /// Input bytes the old process had read but not decoded.
    pub(crate) pending: Vec<u8>,
    /// Tool names the old process served, sorted.
    pub(crate) tools: Vec<String>,
    pub(crate) old_version: String,
}

impl Handover {
    /// The request line followed by the undecoded input, ready to be put in
    /// front of live stdin.
    pub(crate) fn replay_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.replay_line.len() + 1 + self.pending.len());
        bytes.extend_from_slice(self.replay_line.as_bytes());
        bytes.push(b'\n');
        bytes.extend_from_slice(&self.pending);
        bytes
    }
}

// Only the unix reload path writes a handover; windows builds never call it.
#[cfg_attr(not(unix), allow(dead_code))]
/// Create the handover file, readable by the owner only.
pub(crate) fn write_handover(handover: &Handover) -> std::io::Result<PathBuf> {
    use std::io::Write;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = std::env::temp_dir().join(format!(
        "mcp-methods-reload-{}-{nanos}.json",
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&path)?;
    let written = serde_json::to_vec(handover)
        .map_err(std::io::Error::other)
        .and_then(|bytes| file.write_all(&bytes));
    if let Err(error) = written {
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(path)
}

/// Handover files older than this are orphans whatever their pid says.
#[cfg(unix)]
const HANDOVER_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

/// Remove handover files in `dir` that no process will ever read: owned by
/// this user and named for a pid that is gone, or older than `max_age`.
/// `keep` (the file this process is about to read) is never touched. Errors
/// are ignored; this is housekeeping.
#[cfg(unix)]
pub(crate) fn sweep_stale_handovers(
    dir: &std::path::Path,
    max_age: std::time::Duration,
    keep: Option<&std::path::Path>,
) {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // SAFETY: geteuid takes no arguments and cannot fail.
    let uid = unsafe { libc::geteuid() };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("mcp-methods-reload-"))
            .and_then(|n| n.strip_suffix(".json"))
            .and_then(|n| n.split('-').next())
            .and_then(|pid| pid.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        let path = entry.path();
        if keep == Some(path.as_path()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || meta.uid() != uid {
            continue;
        }
        let old = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > max_age);
        // SAFETY: signal 0 only probes for the pid's existence.
        let gone = unsafe { libc::kill(pid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone || old {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Read a handover file. The file is removed whether or not it parsed.
pub(crate) fn read_handover(path: &std::path::Path) -> Result<Handover, String> {
    let read = std::fs::read(path);
    let _ = std::fs::remove_file(path);
    let bytes = read.map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("cannot parse {}: {e}", path.display()))
}

/// Whether `params` is `ping` with `reload: true`.
pub(crate) fn params_are_reload_ping(params: &CallToolRequestParams) -> bool {
    params.name == "ping"
        && params
            .arguments
            .as_ref()
            .and_then(|a| a.get("reload"))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
}

pub(crate) fn is_reload_ping(request: &CallToolRequestParams) -> bool {
    params_are_reload_ping(request)
}

fn message_is_reload_ping(message: &rmcp::service::RxJsonRpcMessage<RoleServer>) -> bool {
    matches!(
        message,
        JsonRpcMessage::Request(r)
            if matches!(&r.request, ClientRequest::CallToolRequest(c) if params_are_reload_ping(&c.params))
    )
}

/// Releases the transport's input gate when dropped.
pub(crate) struct GateRelease(Arc<Gate>);

impl GateRelease {
    pub(crate) fn new(gate: Arc<Gate>) -> Self {
        Self(gate)
    }
}

impl Drop for GateRelease {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn refused(reason: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "reload refused: {reason}"
    ))])
}

/// Handle `ping(reload=true)`. Returns the answer when the process is not
/// replaced; a successful reload never returns from the old process.
pub(crate) async fn reload_ping(
    check: &Arc<SelfCheckState>,
    context: &RequestContext<RoleServer>,
    tools: Vec<String>,
) -> CallToolResult {
    if let Some(old) = check.take_reloaded() {
        return CallToolResult::success(vec![ContentBlock::text(format!(
            "reloaded: {old} → {new}\n\
             state reset by the reload: retained response ids (expand_response), \
             loaded skills, and workspace or session state started over.",
            new = check.running_version()
        ))]);
    }
    if let Some(why) = check.reload_unavailable(true) {
        return refused(why);
    }
    if !check.refresh().await {
        return refused(format!(
            "already up to date ({} {})",
            check.name(),
            check.running_version()
        ));
    }
    // Every request decoded before this one (the transport decodes nothing
    // after it until this handler returns) must have had its answer written:
    // the exec would otherwise drop answers still on their way out.
    let Some(link) = check.link() else {
        return refused("reload needs the framework stdio transport");
    };
    if !link
        .outstanding
        .wait_until_only(Some(&context.id), OUTSTANDING_WAIT)
        .await
    {
        return refused(
            "other requests are still in flight (waited for their answers); \
             retry when they finish",
        );
    }
    let Some(config) = check.reload_config() else {
        return refused("reload is not configured for this server");
    };
    if let Some(can_reload) = &config.can_reload {
        if let Err(reason) = can_reload() {
            return refused(reason);
        }
    }
    if let Some(selftest) = config.selftest.clone() {
        let path = check.path().to_path_buf();
        match tokio::task::spawn_blocking(move || selftest(&path)).await {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => {
                return refused(format!("the new build failed its selftest: {reason}"))
            }
            Err(error) => return refused(format!("selftest did not complete: {error}")),
        }
    }
    let Some(raw) = context.extensions.get::<RawJsonRpcRequest>() else {
        return refused("the request text is unavailable");
    };
    let Some(peer_info) = context
        .peer
        .peer_info()
        .and_then(|info| serde_json::to_value(&*info).ok())
    else {
        return refused("the session's initialize parameters are unavailable");
    };
    let replay_line = raw.source.to_string();
    perform(check, config.argv.clone(), peer_info, replay_line, tools).await
}

/// Longest a reload waits for earlier requests' answers to be written.
const OUTSTANDING_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(unix)]
async fn perform(
    check: &Arc<SelfCheckState>,
    argv: Vec<std::ffi::OsString>,
    peer_info: serde_json::Value,
    replay_line: String,
    tools: Vec<String>,
) -> CallToolResult {
    use std::os::unix::process::CommandExt;
    let Some(link) = check.link() else {
        return refused("reload needs the framework stdio transport");
    };
    let Some((program, args)) = argv.split_first() else {
        return refused("reload_argv is empty");
    };
    // Exclusive output from here to the exec: nothing else may be written
    // between the flush and the replacement of the process image.
    let held = match link.writer.hold().await {
        Ok(held) => held,
        Err(error) => return refused(format!("cannot take the output stream: {error}")),
    };
    let pending = link.input.freeze_and_snapshot();
    let handover = Handover {
        peer_info,
        replay_line,
        pending,
        tools,
        old_version: check.running_version().to_string(),
    };
    let path = match write_handover(&handover) {
        Ok(path) => path,
        Err(error) => {
            link.input.unfreeze();
            return refused(format!("cannot write the handover: {error}"));
        }
    };
    if let Err(reason) = exec_viable(program) {
        link.input.unfreeze();
        let _ = std::fs::remove_file(&path);
        drop(held);
        return refused(format!(
            "cannot start {}: {reason}",
            program.to_string_lossy()
        ));
    }
    let before_exec = check.reload_config().and_then(|c| c.before_exec.as_ref());
    if let Some(before_exec) = before_exec {
        before_exec();
    }
    link.input.restore_blocking();
    let error = std::process::Command::new(program)
        .args(args)
        .env(HANDOVER_ENV, &path)
        .exec();
    // `exec` returned: the old process is still the one serving.
    link.input.reapply_nonblocking();
    let _ = std::fs::remove_file(&path);
    link.input.unfreeze();
    drop(held);
    let program = program.to_string_lossy();
    if before_exec.is_some() {
        // The consumer already released its state; continuing would serve
        // from a half-torn-down process.
        check.exit_after_answer(link.outstanding.clone());
        return CallToolResult::error(vec![ContentBlock::text(format!(
            "reload failed: cannot start {program}: {error}. The server released its \
             state for the reload (before_exec ran), so it is shutting down; \
             restart the MCP client."
        ))]);
    }
    refused(format!("cannot start {program}: {error}"))
}

/// Whether `exec` of `program` can plausibly succeed: an executable regular
/// file at that path, or on `PATH` for a bare name. Checked before the
/// consumer's `before_exec` hook so a missing or non-executable target never
/// costs the consumer its state.
#[cfg(unix)]
fn exec_viable(program: &std::ffi::OsStr) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    fn direct(path: &std::path::Path) -> Result<(), String> {
        let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
        if !meta.is_file() {
            return Err("not a regular file".to_string());
        }
        let c_path =
            std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: `c_path` is a valid NUL-terminated string.
        if unsafe { libc::access(c_path.as_ptr(), libc::X_OK) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }
    if program.as_bytes().contains(&b'/') {
        return direct(std::path::Path::new(program));
    }
    let on_path = std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| direct(&dir.join(program)).is_ok())
    });
    if on_path {
        Ok(())
    } else {
        Err("not found on PATH".to_string())
    }
}

#[cfg(not(unix))]
async fn perform(
    _check: &Arc<SelfCheckState>,
    _argv: Vec<std::ffi::OsString>,
    _peer_info: serde_json::Value,
    _replay_line: String,
    _tools: Vec<String>,
) -> CallToolResult {
    refused("reload is unsupported on this platform")
}

/// Best-effort JSON-RPC id of a request line.
fn request_id(line: &str) -> Option<NumberOrString> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    serde_json::from_value(value.get("id")?.clone()).ok()
}

impl McpServer {
    /// Serve over the framework stdio transport, resuming a reloaded session
    /// when this process was started by `ping(reload=true)`.
    ///
    /// Use this instead of `server.serve(transport)` to make reload
    /// available. Without a handover (the normal case) it is exactly
    /// `serve`. With one it reads and deletes the handover file, unsets
    /// `MCP_METHODS_RELOAD_HANDOVER`, serves with the recorded client info and no
    /// handshake, replays the interrupted request, re-adopts client roots
    /// when configured, and sends `tools/list_changed` when the tool set
    /// differs from the old process's.
    ///
    /// Before anything else it removes orphaned handover files (see
    /// `sweep_stale_handovers`) left by reloads whose new image died early.
    ///
    /// A handover that exists but cannot be read leaves no session to
    /// resume: the error is returned (and the file removed), so a caller
    /// that propagates it exits non-zero rather than waiting for a client
    /// handshake that will never come. A handover whose client info cannot
    /// be restored answers the replayed request with an error first.
    pub async fn serve_with_handover<R, W>(
        self,
        transport: StdioTransport<R, W>,
    ) -> anyhow::Result<RunningService<RoleServer, McpServer>>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let state = self.options().self_check.clone();
        let mut transport = transport;
        if state.as_ref().is_some_and(|s| s.reload_config().is_some()) {
            transport = transport.with_pause_after(message_is_reload_ping);
        }
        if let Some(state) = &state {
            state.attach(transport.link());
        }
        let handover_path = std::env::var_os(HANDOVER_ENV);
        #[cfg(unix)]
        sweep_stale_handovers(
            &std::env::temp_dir(),
            HANDOVER_MAX_AGE,
            handover_path.as_deref().map(std::path::Path::new),
        );
        let Some(path) = handover_path else {
            return Ok(rmcp::ServiceExt::serve(self, transport).await?);
        };
        std::env::remove_var(HANDOVER_ENV);
        let handover = read_handover(std::path::Path::new(&path))
            .map_err(|e| anyhow::anyhow!("reload handover unusable, no session to resume: {e}"))?;
        let peer_info = match serde_json::from_value(handover.peer_info.clone()) {
            Ok(info) => info,
            Err(error) => {
                let reply = TxJsonRpcMessage::<RoleServer>::error(
                    ErrorData::internal_error(
                        format!("reload failed to resume the session: {error}"),
                        None,
                    ),
                    request_id(&handover.replay_line),
                );
                let _ = transport.send(reply).await;
                anyhow::bail!("reload handover carried unusable client info: {error}");
            }
        };
        if let Some(state) = &state {
            state.mark_reloaded(handover.old_version.clone());
        }
        transport.prepend_input(&handover.replay_bytes());
        let service = rmcp::service::serve_directly(self.clone(), transport, Some(peer_info));
        let server = self;
        let peer = service.peer().clone();
        let old_tools = handover.tools;
        tokio::spawn(async move {
            crate::server::roots::on_client_initialized(server.options(), &peer).await;
            if server.tool_names() != old_tools {
                let _ = peer.notify_tool_list_changed().await;
            }
        });
        Ok(service)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handover_round_trips_including_non_utf8_pending_bytes() {
        let handover = Handover {
            peer_info: serde_json::json!({"protocolVersion":"2024-11-05"}),
            replay_line: r#"{"jsonrpc":"2.0","id":3,"method":"tools/call"}"#.into(),
            pending: vec![b'{', 0xFF, b'\n', 0, 200],
            tools: vec!["grep".into(), "ping".into()],
            old_version: "1.0.0".into(),
        };
        let path = write_handover(&handover).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let back = read_handover(&path).unwrap();
        assert_eq!(back, handover);
        assert!(!path.exists(), "the file is deleted on read");
        let mut expected = handover.replay_line.clone().into_bytes();
        expected.push(b'\n');
        expected.extend_from_slice(&handover.pending);
        assert_eq!(back.replay_bytes(), expected);
    }

    #[test]
    fn unreadable_handover_is_an_error_and_the_file_is_still_removed() {
        let path =
            std::env::temp_dir().join(format!("mcp-methods-bad-{}.json", std::process::id()));
        std::fs::write(&path, b"not json").unwrap();
        assert!(read_handover(&path).is_err());
        assert!(!path.exists());
        assert!(read_handover(&path).is_err());
    }

    #[test]
    fn request_id_reads_numbers_and_strings() {
        assert_eq!(request_id(r#"{"id":4}"#), Some(NumberOrString::Number(4)));
        assert!(matches!(
            request_id(r#"{"id":"a"}"#),
            Some(NumberOrString::String(_))
        ));
        assert_eq!(request_id("garbage"), None);
    }
}

#[cfg(all(test, unix))]
mod session_tests {
    use super::*;
    use crate::server::{SelfCheck, ServerOptions};
    use rmcp::handler::server::router::tool::ToolRoute;
    use rmcp::model::Tool;
    use serde_json::{json, Value};
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    /// Env var and stdin descriptor flags are process-global; every test that
    /// serves through `serve_with_handover` holds this for its whole run.
    static SERVE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    const INIT: &str = r#"{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"c","version":"1"}}"#;

    /// Client side of a served transport: requests go through a real pipe
    /// (so the stdin pump is exercised), responses come back over a duplex.
    struct Harness {
        _serve: tokio::sync::MutexGuard<'static, ()>,
        dir: tempfile::TempDir,
        pipe_in: std::fs::File,
        _pipe_out: std::os::fd::OwnedFd,
        out: BufReader<DuplexStream>,
        notes: Vec<Value>,
        id: i64,
        gate: Arc<tokio::sync::Notify>,
        /// Set by the injected replacement for `process::exit`.
        exited: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Harness {
        fn bin(&self) -> PathBuf {
            self.dir.path().join("bin")
        }

        fn replace(&self) {
            std::fs::write(self.bin(), b"a different, longer build").unwrap();
        }

        fn send_raw(&mut self, text: &str) {
            use std::io::Write;
            self.pipe_in.write_all(text.as_bytes()).unwrap();
        }

        fn line(&mut self, id: i64, method: &str, params: Value) -> String {
            format!(
                "{}\n",
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
            )
        }

        async fn next_frame(&mut self) -> Option<Value> {
            let mut line = String::new();
            let read = tokio::time::timeout(Duration::from_secs(10), self.out.read_line(&mut line))
                .await
                .expect("timed out waiting for a frame")
                .unwrap();
            (read > 0).then(|| serde_json::from_str(&line).unwrap())
        }

        async fn frame_for(&mut self, id: i64) -> Value {
            loop {
                let frame = self.next_frame().await.expect("server closed the stream");
                if frame["id"] == id {
                    return frame;
                }
                self.notes.push(frame);
            }
        }

        async fn request(&mut self, method: &str, params: Value) -> Value {
            self.id += 1;
            let id = self.id;
            let line = self.line(id, method, params);
            self.send_raw(&line);
            self.frame_for(id).await
        }

        async fn call(&mut self, name: &str, arguments: Value) -> Value {
            self.request("tools/call", json!({"name":name,"arguments":arguments}))
                .await
        }

        /// Text of the last content block of a `tools/call` answer.
        fn text(frame: &Value) -> String {
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

    fn pipe() -> (std::fs::File, std::os::fd::OwnedFd) {
        use std::os::fd::{FromRawFd, OwnedFd};
        let mut fds = [0i32; 2];
        // SAFETY: `fds` has room for the two descriptors `pipe(2)` writes.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: both descriptors are fresh and owned by nobody else.
        unsafe {
            (
                std::fs::File::from_raw_fd(fds[1]),
                OwnedFd::from_raw_fd(fds[0]),
            )
        }
    }

    fn slow_route(gate: Arc<tokio::sync::Notify>) -> ToolRoute<McpServer> {
        ToolRoute::new_dyn(
            Tool::new("slow", "waits for the test", Arc::new(Default::default())),
            move |_| {
                let gate = gate.clone();
                Box::pin(async move {
                    gate.notified().await;
                    Ok(CallToolResult::success(vec![ContentBlock::text("slow done")]).into())
                })
            },
        )
    }

    async fn boot_with(configure: impl FnOnce(SelfCheck) -> SelfCheck, pumped: bool) -> Harness {
        let serve = SERVE_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bin"), b"v1").unwrap();
        let check = configure(
            SelfCheck::new("demo", dir.path().join("bin"), "1.2.3")
                .with_installed_version(|| Some("9.9.9".to_string())),
        );
        let mut server = McpServer::new(ServerOptions::default().with_self_check(check));
        let gate = Arc::new(tokio::sync::Notify::new());
        let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let exit_flag = exited.clone();
        server
            .options()
            .self_check
            .as_ref()
            .expect("self check configured")
            .set_exit_hook(move || exit_flag.store(true, Ordering::SeqCst));
        server.tool_router_mut().add_route(slow_route(gate.clone()));
        let (pipe_in, pipe_out) = pipe();
        let (server_tx, client_rx) = duplex(1 << 20);
        let (mut harness_in, harness_pending) = (pipe_in, None::<()>);
        let _ = harness_pending;
        if pumped {
            use std::os::fd::AsRawFd;
            let transport =
                StdioTransport::<tokio::io::Empty, _>::pumped(pipe_out.as_raw_fd(), server_tx)
                    .expect("pump registers a pipe");
            tokio::spawn(async move {
                let service = server.serve_with_handover(transport).await.unwrap();
                let _ = service.waiting().await;
            });
        } else {
            // Same client pipe, but read through tokio so no pump is active.
            use std::os::fd::{AsRawFd, FromRawFd};
            // SAFETY: duplicate so the pipe end stays owned by the harness.
            let dup = unsafe { libc::dup(pipe_out.as_raw_fd()) };
            let file = unsafe { std::fs::File::from_raw_fd(dup) };
            let transport = StdioTransport::from_io(tokio::fs::File::from_std(file), server_tx);
            tokio::spawn(async move {
                let service = server.serve_with_handover(transport).await.unwrap();
                let _ = service.waiting().await;
            });
        }
        let mut h = Harness {
            _serve: serve,
            dir,
            pipe_in: std::mem::replace(&mut harness_in, std::fs::File::open("/dev/null").unwrap()),
            _pipe_out: pipe_out,
            out: BufReader::new(client_rx),
            notes: Vec::new(),
            id: 0,
            gate,
            exited,
        };
        let init = h
            .request("initialize", serde_json::from_str(INIT).unwrap())
            .await;
        assert!(init.get("error").is_none(), "{init}");
        h.send_raw("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n");
        h
    }

    fn ping_schema(tools: &Value) -> Value {
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "ping")
            .unwrap()["inputSchema"]
            .clone()
    }

    #[tokio::test]
    async fn ping_schema_gains_reload_only_when_reload_is_configured() {
        let mut plain = boot_with(|c| c, true).await;
        let plain_schema = ping_schema(&plain.request("tools/list", json!({})).await);
        drop(plain);
        let mut reloadable = boot_with(|c| c.with_reload(["/bin/true"]), true).await;
        let with_reload = ping_schema(&reloadable.request("tools/list", json!({})).await);
        assert_eq!(with_reload["properties"]["reload"]["type"], "boolean");
        let mut without = with_reload.clone();
        without["properties"]
            .as_object_mut()
            .unwrap()
            .remove("reload");
        assert_eq!(without, plain_schema, "only `reload` is added");
        assert!(plain_schema["properties"].get("reload").is_none());
        drop(reloadable);
        // A server with no self-check at all is byte-identical to the plain one.
        let _lock = SERVE_LOCK.lock().await;
        let server = McpServer::new(ServerOptions::default());
        let bare = rmcp::ServerHandler::get_tool(&server, "ping").unwrap();
        assert_eq!(Value::Object((*bare.input_schema).clone()), plain_schema);
    }

    #[tokio::test]
    async fn refused_when_reload_is_not_configured() {
        let mut h = boot_with(|c| c, true).await;
        h.replace();
        let frame = h.call("ping", json!({"reload": true})).await;
        assert_eq!(frame["result"]["isError"], true);
        assert!(Harness::text(&frame).contains("not configured"), "{frame}");
        assert!(Harness::text(&h.call("ping", json!({})).await).starts_with("pong"));
    }

    #[tokio::test]
    async fn refused_when_up_to_date() {
        let mut h = boot_with(|c| c.with_reload(["/nonexistent/server"]), true).await;
        let frame = h.call("ping", json!({"reload": true})).await;
        assert_eq!(frame["result"]["isError"], true);
        assert!(
            Harness::text(&frame).contains("already up to date"),
            "{frame}"
        );
    }

    #[tokio::test]
    async fn refused_while_another_call_is_in_flight_then_proceeds() {
        let mut h = boot_with(|c| c.with_reload(["/nonexistent/server"]), true).await;
        h.replace();
        h.id += 1;
        let slow_id = h.id;
        let slow = h.line(slow_id, "tools/call", json!({"name":"slow","arguments":{}}));
        h.send_raw(&slow);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let frame = h.call("ping", json!({"reload": true})).await;
        assert_eq!(frame["result"]["isError"], true);
        assert!(Harness::text(&frame).contains("still in flight"), "{frame}");
        h.gate.notify_one();
        assert!(Harness::text(&h.frame_for(slow_id).await).starts_with("slow done"));
        // Nothing else running: it now reaches the exec, which fails.
        let frame = h.call("ping", json!({"reload": true})).await;
        assert!(Harness::text(&frame).contains("cannot start"), "{frame}");
    }

    #[tokio::test]
    async fn consumer_veto_and_failed_selftest_refuse_and_the_server_keeps_serving() {
        let mut h = boot_with(
            |c| {
                c.with_reload(["/nonexistent/server"])
                    .with_can_reload(|| Err("a write is pending".to_string()))
            },
            true,
        )
        .await;
        h.replace();
        let frame = h.call("ping", json!({"reload": true})).await;
        assert!(
            Harness::text(&frame).contains("a write is pending"),
            "{frame}"
        );
        drop(h);

        let seen = Arc::new(std::sync::Mutex::new(None));
        let seen2 = seen.clone();
        let mut h = boot_with(
            move |c| {
                c.with_reload(["/nonexistent/server"])
                    .with_selftest(move |path| {
                        *seen2.lock().unwrap() = Some(path.to_path_buf());
                        Err("exit 1".to_string())
                    })
            },
            true,
        )
        .await;
        h.replace();
        let frame = h.call("ping", json!({"reload": true})).await;
        assert_eq!(frame["result"]["isError"], true);
        assert!(
            Harness::text(&frame).contains("failed its selftest: exit 1"),
            "{frame}"
        );
        assert_eq!(seen.lock().unwrap().as_deref(), Some(h.bin().as_path()));
        assert!(Harness::text(&h.call("ping", json!({})).await).starts_with("pong"));
    }

    #[tokio::test]
    async fn unexecutable_target_keeps_serving_without_running_before_exec_and_answers_the_pipelined_request(
    ) {
        let hook = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook2 = hook.clone();
        let mut h = boot_with(
            move |c| {
                c.with_reload(["/nonexistent/server"])
                    .with_before_exec(move || hook2.store(true, Ordering::SeqCst))
            },
            true,
        )
        .await;
        h.replace();
        h.id += 1;
        let (ping_id, next_id) = (h.id, h.id + 1);
        h.id = next_id;
        let both = format!(
            "{}{}",
            h.line(
                ping_id,
                "tools/call",
                json!({"name":"ping","arguments":{"reload":true}})
            ),
            h.line(next_id, "tools/call", json!({"name":"ping","arguments":{}}))
        );
        h.send_raw(&both);
        let first = h.frame_for(ping_id).await;
        assert!(
            Harness::text(&first).contains("cannot start /nonexistent/server"),
            "{first}"
        );
        assert!(
            !hook.load(Ordering::SeqCst),
            "a target that cannot be executed is caught before before_exec"
        );
        let second = h.frame_for(next_id).await;
        assert!(Harness::text(&second).starts_with("pong"), "{second}");
        let pid_prefix = format!("mcp-methods-reload-{}-", std::process::id());
        let leftover = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with(&pid_prefix));
        assert!(
            !leftover,
            "the handover file is removed after a failed exec"
        );
        // Input still flows after the pump was frozen and resumed.
        assert!(Harness::text(&h.call("ping", json!({})).await).starts_with("pong"));
    }

    #[tokio::test]
    async fn reload_waits_for_a_slow_answer_to_be_written_before_it_proceeds() {
        let mut h = boot_with(|c| c.with_reload(["/nonexistent/server"]), true).await;
        h.replace();
        h.id += 1;
        let (slow_id, ping_id) = (h.id, h.id + 1);
        h.id = ping_id;
        let both = format!(
            "{}{}",
            h.line(slow_id, "tools/call", json!({"name":"slow","arguments":{}})),
            h.line(
                ping_id,
                "tools/call",
                json!({"name":"ping","arguments":{"reload":true}})
            )
        );
        h.send_raw(&both);
        // The slow call finishes well inside the wait: the reload is held
        // back until its answer is written, then reaches the exec stage.
        tokio::time::sleep(Duration::from_millis(300)).await;
        h.gate.notify_one();
        let slow = h.frame_for(slow_id).await;
        assert!(Harness::text(&slow).starts_with("slow done"), "{slow}");
        let ping = h.frame_for(ping_id).await;
        let text = Harness::text(&ping);
        assert!(text.contains("cannot start"), "{ping}");
        assert!(!text.contains("in flight"), "{ping}");
    }

    #[tokio::test]
    async fn a_slow_call_that_outlasts_the_wait_refuses_the_reload_and_is_still_answered() {
        let mut h = boot_with(|c| c.with_reload(["/nonexistent/server"]), true).await;
        h.replace();
        h.id += 1;
        let slow_id = h.id;
        let slow = h.line(slow_id, "tools/call", json!({"name":"slow","arguments":{}}));
        h.send_raw(&slow);
        let ping = h.call("ping", json!({"reload": true})).await;
        assert!(Harness::text(&ping).contains("still in flight"), "{ping}");
        h.gate.notify_one();
        let slow = h.frame_for(slow_id).await;
        assert!(Harness::text(&slow).starts_with("slow done"), "{slow}");
    }

    #[tokio::test]
    async fn exec_failing_after_before_exec_answers_with_an_error_and_shuts_the_server_down() {
        use std::os::unix::fs::PermissionsExt;
        // Executable regular file, so the pre-check passes, whose interpreter
        // does not exist, so the exec itself fails.
        let bin_dir = tempfile::tempdir().unwrap();
        let script = bin_dir.path().join("server");
        std::fs::write(&script, "#!/nonexistent/interpreter\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hook = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook2 = hook.clone();
        let argv = [script.clone()];
        let mut h = boot_with(
            move |c| {
                c.with_reload(argv)
                    .with_before_exec(move || hook2.store(true, Ordering::SeqCst))
            },
            true,
        )
        .await;
        h.replace();
        let frame = h.call("ping", json!({"reload": true})).await;
        assert!(hook.load(Ordering::SeqCst), "before_exec ran");
        assert_eq!(frame["result"]["isError"], true);
        let text = Harness::text(&frame);
        assert!(text.contains("shutting down"), "{frame}");
        assert!(text.contains("cannot start"), "{frame}");
        tokio::time::timeout(Duration::from_secs(10), async {
            while !h.exited.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the server ends the process after answering");
    }

    #[test]
    fn exec_viability_requires_an_executable_regular_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("prog");
        assert!(exec_viable(file.as_os_str()).is_err(), "missing");
        std::fs::write(&file, "x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(exec_viable(file.as_os_str()).is_err(), "not executable");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(exec_viable(file.as_os_str()).is_ok());
        assert!(exec_viable(dir.path().as_os_str()).is_err(), "a directory");
        assert!(
            exec_viable(std::ffi::OsStr::new("sh")).is_ok(),
            "bare name on PATH"
        );
        assert!(exec_viable(std::ffi::OsStr::new("no-such-program-anywhere")).is_err());
    }

    #[test]
    fn sweep_removes_only_orphaned_handover_files() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let mut dead = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = dead.id();
        dead.wait().unwrap();
        let make = |name: String| {
            let path = dir.path().join(name);
            std::fs::write(&path, b"{}").unwrap();
            path
        };
        let orphan = make(format!("mcp-methods-reload-{dead_pid}-1.json"));
        let live = make(format!("mcp-methods-reload-{me}-2.json"));
        let current = make(format!("mcp-methods-reload-{me}-3.json"));
        let old_live = make(format!("mcp-methods-reload-{me}-4.json"));
        let unrelated = make("mcp-methods-other.json".into());
        let unparsable = make("mcp-methods-reload-notapid-5.json".into());
        std::fs::File::options()
            .write(true)
            .open(&old_live)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(7200))
            .unwrap();
        sweep_stale_handovers(dir.path(), HANDOVER_MAX_AGE, Some(&current));
        assert!(!orphan.exists(), "dead pid");
        assert!(!old_live.exists(), "older than the age bound");
        assert!(live.exists(), "live pid, fresh");
        assert!(current.exists(), "the file this process will read");
        assert!(unrelated.exists() && unparsable.exists(), "not ours");
    }

    #[tokio::test]
    async fn notice_remedy_follows_whether_reload_can_run() {
        let notice_for = |pumped: bool| async move {
            let mut h = boot_with(|c| c.with_reload(["/bin/true"]), pumped).await;
            h.replace();
            Harness::text(&h.call("read_source", json!({"file_path":"x"})).await)
        };
        assert!(notice_for(true)
            .await
            .ends_with("call ping(reload=true) to load it"));
        assert!(notice_for(false)
            .await
            .ends_with("restart the MCP client to load it"));
    }

    #[tokio::test]
    async fn reload_refused_with_a_reason_when_stdin_is_not_pollable() {
        let mut h = boot_with(|c| c.with_reload(["/bin/true"]), false).await;
        h.replace();
        let frame = h.call("ping", json!({"reload": true})).await;
        assert_eq!(frame["result"]["isError"], true);
        assert!(Harness::text(&frame).contains("not pollable"), "{frame}");
    }

    const REPLAY_PING: &str = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"ping","arguments":{"reload":true}}}"#;
    const NEXT_PING: &str =
        r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"ping","arguments":{}}}"#;

    /// Serve a resumed session over a duplex: no `initialize` is sent.
    struct Resumed {
        client_tx: DuplexStream,
        out: BufReader<DuplexStream>,
        _serve: tokio::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    async fn resume(
        handover: &Handover,
        raw_file: Option<&[u8]>,
    ) -> (Resumed, tokio::task::JoinHandle<anyhow::Result<()>>) {
        let serve = SERVE_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bin"), b"v2").unwrap();
        let path = match raw_file {
            Some(bytes) => {
                let p = dir.path().join("handover.json");
                std::fs::write(&p, bytes).unwrap();
                p
            }
            None => write_handover(handover).unwrap(),
        };
        std::env::set_var(HANDOVER_ENV, &path);
        let check =
            SelfCheck::new("demo", dir.path().join("bin"), "2.0.0").with_reload(["/bin/true"]);
        let server = McpServer::new(ServerOptions::default().with_self_check(check));
        let (client_tx, server_rx) = duplex(1 << 20);
        let (server_tx, client_rx) = duplex(1 << 20);
        let transport = StdioTransport::from_io(server_rx, server_tx);
        let run = tokio::spawn(async move {
            let service = server.serve_with_handover(transport).await?;
            let _ = service.waiting().await;
            Ok(())
        });
        assert!(
            tokio::time::timeout(Duration::from_secs(10), async {
                while std::env::var_os(HANDOVER_ENV).is_some() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .is_ok(),
            "the handover variable is unset at boot"
        );
        assert!(!path.exists(), "the handover file is deleted at boot");
        (
            Resumed {
                client_tx,
                out: BufReader::new(client_rx),
                _serve: serve,
                _dir: dir,
            },
            run,
        )
    }

    impl Resumed {
        async fn frame(&mut self, within: Duration) -> Option<Value> {
            let mut line = String::new();
            let read = tokio::time::timeout(within, self.out.read_line(&mut line))
                .await
                .ok()?
                .unwrap();
            (read > 0).then(|| serde_json::from_str(&line).unwrap())
        }
    }

    fn sample_handover(tools: Vec<String>) -> Handover {
        Handover {
            peer_info: serde_json::from_str(INIT).unwrap(),
            replay_line: REPLAY_PING.to_string(),
            pending: format!("{NEXT_PING}\n").into_bytes(),
            tools,
            old_version: "1.0.0".to_string(),
        }
    }

    fn current_tools() -> Vec<String> {
        McpServer::new(ServerOptions::default()).tool_names()
    }

    #[tokio::test]
    async fn resumed_session_answers_the_replay_and_the_pipelined_request_without_initialize() {
        let (mut r, _run) = resume(&sample_handover(current_tools()), None).await;
        let mut frames = Vec::new();
        while frames.len() < 2 {
            frames.push(r.frame(Duration::from_secs(10)).await.expect("two answers"));
        }
        let by_id = |id: i64| frames.iter().find(|f| f["id"] == id).unwrap().clone();
        let replayed = Harness::text(&by_id(7));
        assert!(
            replayed.starts_with("reloaded: 1.0.0 → 2.0.0"),
            "{replayed}"
        );
        assert!(replayed.contains("retained response ids"), "{replayed}");
        assert_ne!(by_id(7)["result"]["isError"], true);
        let next = Harness::text(&by_id(8));
        assert!(next.starts_with("pong"), "{next}");
        assert!(next.contains("demo 2.0.0 (up to date)"), "{next}");
        // Same tool set: no list_changed.
        assert!(r.frame(Duration::from_millis(300)).await.is_none());
        // The session keeps working and a later reload ping is a real request
        // again (the one-shot marker was consumed).
        r.client_tx
            .write_all(format!("{NEXT_PING}\n").as_bytes())
            .await
            .unwrap();
        assert!(
            Harness::text(&r.frame(Duration::from_secs(10)).await.unwrap()).starts_with("pong")
        );
        r.client_tx
            .write_all(format!("{REPLAY_PING}\n").as_bytes())
            .await
            .unwrap();
        let again = r.frame(Duration::from_secs(10)).await.unwrap();
        assert_eq!(again["result"]["isError"], true, "{again}");
        assert!(Harness::text(&again).contains("not pollable"), "{again}");
    }

    #[tokio::test]
    async fn resume_sends_list_changed_when_the_tool_set_differs() {
        let (mut r, _run) = resume(&sample_handover(vec!["removed_tool".into()]), None).await;
        let mut saw_change = false;
        let mut saw_reply = false;
        while !(saw_change && saw_reply) {
            let frame = r.frame(Duration::from_secs(10)).await.expect("frames");
            saw_change |= frame["method"] == "notifications/tools/list_changed";
            saw_reply |= frame["id"] == 7;
        }
    }

    #[tokio::test]
    async fn unreadable_handover_fails_boot_and_still_cleans_up() {
        let (_r, run) = resume(&sample_handover(vec![]), Some(b"{ not json")).await;
        let result = tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .unwrap()
            .unwrap();
        let error = result.err().expect("boot fails").to_string();
        assert!(error.contains("no session to resume"), "{error}");
    }

    #[tokio::test]
    async fn handover_with_unusable_client_info_answers_the_replay_with_an_error() {
        let mut handover = sample_handover(vec![]);
        handover.peer_info = json!({"nonsense": true});
        let (mut r, run) = resume(&handover, None).await;
        let frame = r
            .frame(Duration::from_secs(10))
            .await
            .expect("an error answer");
        assert_eq!(frame["id"], 7);
        assert!(
            frame["error"]["message"]
                .as_str()
                .unwrap()
                .contains("resume"),
            "{frame}"
        );
        let result = tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
    }
}
