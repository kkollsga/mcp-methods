//! Binary-level proof that peripheral boot failures degrade instead of
//! killing the process before the MCP `initialize` handshake.
//!
//! The reported defect (mcp-servers operator → kglite → here,
//! 2026-08-31): a manifest whose `source_root:` names a directory that
//! no longer exists exited with one stderr line before the handshake,
//! so from a client's side the server simply never answered. The graph
//! / tool surface was fine; only `read_source` / `grep` / `list_source`
//! were affected.
//!
//! These tests drive the real binary over JSON-RPC stdio (the way a
//! client does) rather than calling a boot function, because the thing
//! under test *is* "does the process live long enough to answer".
//!
//! Deliberately dependency-free: JSON is assembled and inspected as
//! text so this suite needs no dev-dependency beyond `tempfile`.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_mcp-server");
const TIMEOUT: Duration = Duration::from_secs(30);

/// A running `mcp-server` with its stdout lines on a channel and its
/// stderr accumulating in the background.
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    stderr_drainer: thread::JoinHandle<()>,
    next_id: i64,
}

impl Server {
    fn spawn(args: &[&str], cwd: &Path) -> Server {
        let mut child = Command::new(BIN)
            .args(args)
            .current_dir(cwd)
            // Keep the boot deterministic: an ambient token or a `.env`
            // above the tempdir must not change what registers.
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_TOKEN")
            .env("RUST_LOG", "info")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn mcp-server");

        let stdin = child.stdin.take().unwrap();

        let (tx, rx) = mpsc::channel();
        let out = child.stdout.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });

        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        let err = child.stderr.take().unwrap();
        let stderr_drainer = thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let mut buf = sink.lock().unwrap();
                buf.push_str(&line);
                buf.push('\n');
            }
        });

        Server {
            child,
            stdin,
            stdout: rx,
            stderr,
            stderr_drainer,
            next_id: 0,
        }
    }

    fn stderr_text(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    fn send(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("server stdin closed — it exited during boot");
        self.stdin.flush().unwrap();
    }

    /// Send a request and return the raw JSON text of the reply whose
    /// `id` matches. Panics (with the collected stderr) if the server
    /// dies or goes quiet.
    fn request(&mut self, method: &str, params: &str) -> String {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
        ));
        let needle = format!("\"id\":{id}");
        loop {
            match self.stdout.recv_timeout(TIMEOUT) {
                Ok(line) => {
                    if line.contains(&needle) {
                        return line;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => panic!(
                    "server exited before answering {method}. stderr:\n{}",
                    self.stderr_text()
                ),
                Err(RecvTimeoutError::Timeout) => panic!(
                    "timed out waiting for {method}. stderr:\n{}",
                    self.stderr_text()
                ),
            }
        }
    }

    fn initialize(&mut self) -> String {
        let resp = self.request(
            "initialize",
            r#"{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"boot-degradation-test","version":"0"}}"#,
        );
        self.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        resp
    }

    fn call_tool(&mut self, name: &str, arguments: &str) -> String {
        self.request(
            "tools/call",
            &format!(r#"{{"name":"{name}","arguments":{arguments}}}"#),
        )
    }

    /// Close stdin, wait for exit, and hand back everything on stderr.
    /// Joining the drainer (rather than sleeping) guarantees the last
    /// line the process wrote is in the buffer before we assert on it.
    fn shutdown(self) -> String {
        let Server {
            mut child,
            stdin,
            stderr,
            stderr_drainer,
            ..
        } = self;
        drop(stdin);
        let _ = child.wait();
        let _ = stderr_drainer.join();
        let text = stderr.lock().unwrap().clone();
        text
    }
}

/// Write a manifest declaring `source_root: source` into a fresh
/// tempdir. `create_root` decides whether `source/` actually exists —
/// the operator's repro is the `false` case.
fn manifest_dir(create_root: bool) -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    let base = td.path();
    if create_root {
        std::fs::create_dir(base.join("source")).unwrap();
        std::fs::write(base.join("source").join("hello.txt"), "sentinel body\n").unwrap();
    }
    std::fs::write(
        base.join("degrade_mcp.yaml"),
        "name: Boot Degradation Test\nsource_root: source\n",
    )
    .unwrap();
    td
}

#[test]
fn a_missing_source_root_still_answers_initialize() {
    let td = manifest_dir(false);
    let yaml = td.path().join("degrade_mcp.yaml");
    let mut server = Server::spawn(&["--mcp-config", yaml.to_str().unwrap()], td.path());

    let init = server.initialize();
    assert!(
        init.contains("\"serverInfo\"") && init.contains("\"protocolVersion\""),
        "initialize did not return a handshake — the missing source root killed the boot: {init}"
    );

    // The source tools are still registered; they explain the absence
    // when called rather than vanishing from the catalogue.
    let tools = server.request("tools/list", "{}");
    assert!(
        tools.contains("\"read_source\""),
        "read_source dropped out of tools/list: {tools}"
    );

    let read = server.call_tool("read_source", r#"{"file_path":"hello.txt"}"#);
    assert!(
        read.contains("no active source root"),
        "read_source must explain that no source root is available: {read}"
    );
    // …and *which* root, at *which* path. "Configure source_root in
    // your manifest" is a misdirection here: the manifest does declare
    // one. Without these two the real cause lives only on stderr, which
    // the calling agent cannot read.
    assert!(
        read.contains(r#"declared source root \"source\" did not resolve"#),
        "read_source must name the declared root that failed: {read}"
    );
    // The path is reported exactly as it was tried: the manifest's own
    // directory (as spelled on the command line) joined with `source`.
    // JSON-escaped, since the reply embeds it inside a JSON string.
    let needle = td
        .path()
        .join("source")
        .display()
        .to_string()
        .replace('\\', "\\\\");
    assert!(
        read.contains(&needle),
        "read_source must name the path the root was looked for at ({needle}): {read}"
    );

    let stderr = server.shutdown();
    assert!(
        stderr.contains("WARN") && stderr.contains(r#"source root "source" is unavailable at"#),
        "expected one WARN naming the unavailable root. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("degrade_mcp.yaml"),
        "the WARN must name the manifest. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(r#"unresolved source roots: ["source"]"#),
        "the boot summary must record the unresolved root. stderr:\n{stderr}"
    );
}

#[test]
fn the_same_manifest_serves_the_root_once_it_exists() {
    let td = manifest_dir(true);
    let yaml = td.path().join("degrade_mcp.yaml");
    let mut server = Server::spawn(&["--mcp-config", yaml.to_str().unwrap()], td.path());
    server.initialize();

    let read = server.call_tool("read_source", r#"{"file_path":"hello.txt"}"#);
    assert!(
        read.contains("sentinel body"),
        "the existing source root was not served: {read}"
    );

    let stderr = server.shutdown();
    assert!(
        stderr.contains("source roots: ["),
        "boot summary must list the served root. stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("unresolved source roots"),
        "a healthy root must not be reported as unresolved. stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("is unavailable at"),
        "no WARN should be emitted when every root resolves. stderr:\n{stderr}"
    );
}

#[test]
fn one_missing_root_does_not_drop_the_others() {
    let td = tempfile::tempdir().unwrap();
    let base = td.path();
    std::fs::create_dir(base.join("kept")).unwrap();
    std::fs::write(base.join("kept").join("kept.txt"), "kept body\n").unwrap();
    let yaml = base.join("multi_mcp.yaml");
    std::fs::write(
        &yaml,
        "name: Multi Root\nsource_roots:\n  - kept\n  - gone\n",
    )
    .unwrap();

    let mut server = Server::spawn(&["--mcp-config", yaml.to_str().unwrap()], base);
    server.initialize();
    let read = server.call_tool("read_source", r#"{"file_path":"kept.txt"}"#);
    assert!(
        read.contains("kept body"),
        "the root that resolved must still be served alongside the one that did not: {read}"
    );

    let stderr = server.shutdown();
    assert!(
        stderr.contains(r#"unresolved source roots: ["gone"]"#),
        "only the missing root belongs in the unresolved list. stderr:\n{stderr}"
    );
}

#[test]
fn a_missing_explicit_env_file_still_answers_initialize() {
    let td = tempfile::tempdir().unwrap();
    let base = td.path();
    let yaml = base.join("env_mcp.yaml");
    std::fs::write(
        &yaml,
        "name: Missing Env File\nenv_file: stash/absent.env\n",
    )
    .unwrap();

    let mut server = Server::spawn(&["--mcp-config", yaml.to_str().unwrap()], base);
    let init = server.initialize();
    assert!(
        init.contains("\"serverInfo\""),
        "initialize did not return a handshake — the missing env_file killed the boot: {init}"
    );
    let ping = server.call_tool("ping", "{}");
    assert!(
        ping.to_lowercase().contains("pong"),
        "the server should be fully serviceable without its env_file: {ping}"
    );

    let stderr = server.shutdown();
    assert!(
        stderr.contains("WARN") && stderr.contains("env_file does not exist"),
        "expected a WARN naming the missing env_file. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("absent.env"),
        "the WARN must name the path that was not there. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("env_file unavailable:"),
        "the boot summary must record the missing env_file. stderr:\n{stderr}"
    );
}

/// An `env_file:` that exists but cannot be *read* is the same class of
/// failure as one that is absent, and used to be reported as the
/// opposite: the read error was swallowed, so the boot summary printed
/// `env: <path>` with no WARN while zero variables had been applied.
#[cfg(unix)]
#[test]
fn an_unreadable_explicit_env_file_warns_instead_of_reporting_a_load() {
    use std::os::unix::fs::PermissionsExt;

    let td = tempfile::tempdir().unwrap();
    let base = td.path();
    let envp = base.join("locked.env");
    std::fs::write(&envp, "MCP_BOOT_TEST_LOCKED=nope\n").unwrap();
    std::fs::set_permissions(&envp, std::fs::Permissions::from_mode(0o000)).unwrap();

    // root reads it regardless of the mode bits — probe rather than
    // guess, and skip when the precondition cannot be established.
    if std::fs::read_to_string(&envp).is_ok() {
        let _ = std::fs::set_permissions(&envp, std::fs::Permissions::from_mode(0o600));
        eprintln!("skipping: this process can read a mode-000 file (running as root?)");
        return;
    }

    let yaml = base.join("locked_mcp.yaml");
    std::fs::write(&yaml, "name: Locked Env File\nenv_file: locked.env\n").unwrap();

    let mut server = Server::spawn(&["--mcp-config", yaml.to_str().unwrap()], base);
    let init = server.initialize();
    assert!(
        init.contains("\"serverInfo\""),
        "initialize did not return a handshake — the unreadable env_file killed the boot: {init}"
    );

    let stderr = server.shutdown();
    // Restore before the tempdir drops so cleanup cannot trip over it.
    let _ = std::fs::set_permissions(&envp, std::fs::Permissions::from_mode(0o600));

    assert!(
        stderr.contains("WARN") && stderr.contains("env_file could not be read"),
        "expected a WARN saying the read failed. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("locked.env"),
        "the WARN must name the file it could not read. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("env_file unavailable:"),
        "the boot summary must record the env_file as unavailable. stderr:\n{stderr}"
    );
    // `; env: <path>` is the boot summary's *loaded* line. A file that
    // never opened must not produce it — that was the whole defect.
    assert!(
        !stderr.contains("; env: "),
        "a file that never opened must not be reported as loaded. stderr:\n{stderr}"
    );
}

/// `workspace: kind: github` in a manifest binds no workspace on its
/// own — only `--workspace DIR` does. The combination used to boot
/// silently into bare mode with no `repo_management`.
#[test]
fn a_github_workspace_manifest_without_the_flag_warns_and_still_serves() {
    let td = tempfile::tempdir().unwrap();
    let base = td.path();
    let yaml = base.join("gh_mcp.yaml");
    std::fs::write(&yaml, "name: GH Workspace\nworkspace:\n  kind: github\n").unwrap();

    let mut server = Server::spawn(&["--mcp-config", yaml.to_str().unwrap()], base);
    let init = server.initialize();
    assert!(
        init.contains("\"serverInfo\""),
        "initialize did not return a handshake: {init}"
    );
    let tools = server.request("tools/list", "{}");
    assert!(
        !tools.contains("\"repo_management\""),
        "with no workspace bound, repo_management must not be registered: {tools}"
    );

    let stderr = server.shutdown();
    assert!(
        stderr.contains("WARN") && stderr.contains("workspace.kind: github"),
        "expected a WARN about the unbound github workspace. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("gh_mcp.yaml"),
        "the WARN must name the manifest. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("--workspace DIR"),
        "the WARN must name the flag that binds one. stderr:\n{stderr}"
    );
}
