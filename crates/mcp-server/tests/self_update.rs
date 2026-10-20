//! Binary-level proof of the stale-binary notice, `--version` and the
//! in-place reload.
//!
//! The binary is copied into a tempdir and the *copy* is run, so the
//! test can replace the file the process was started from the way an
//! installer does: write a fresh file, rename it over the old name.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_mcp-server");
const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    /// Lines read while waiting for a different id.
    unread: Vec<String>,
    done: Arc<AtomicBool>,
}

/// Longest any test may run before its server is killed, turning a hung
/// read into an `EOF` failure.
const HANG_BOUND: Duration = Duration::from_secs(90);

impl Session {
    fn spawn(exe: &Path, cwd: &Path) -> Session {
        Session::spawn_with(exe, cwd, &[])
    }

    fn spawn_with(exe: &Path, cwd: &Path, args: &[&str]) -> Session {
        let mut child = Command::new(exe)
            .args(args)
            .current_dir(cwd)
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn mcp-server copy");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        // Watchdog: a wedged server would block `read_line` forever; killing
        // it closes the pipe so the test fails instead of hanging.
        let done = Arc::new(AtomicBool::new(false));
        let (watch, pid) = (done.clone(), child.id());
        std::thread::spawn(move || {
            let start = Instant::now();
            while !watch.load(Ordering::Acquire) {
                if start.elapsed() > HANG_BOUND {
                    let _ = Command::new("kill")
                        .args(["-KILL", &pid.to_string()])
                        .status();
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Session {
            child,
            stdin,
            stdout,
            next_id: 0,
            unread: Vec::new(),
            done,
        }
    }

    /// Write requests without waiting; returns their ids.
    fn send(&mut self, requests: &[(&str, &str)]) -> Vec<i64> {
        let mut ids = Vec::new();
        let mut payload = String::new();
        for (method, params) in requests {
            self.next_id += 1;
            ids.push(self.next_id);
            payload.push_str(&format!(
                r#"{{"jsonrpc":"2.0","id":{},"method":"{method}","params":{params}}}"#,
                self.next_id
            ));
            payload.push('\n');
        }
        self.stdin.write_all(payload.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
        ids
    }

    fn wait_for(&mut self, id: i64) -> String {
        let is_answer = |line: &str| {
            serde_json::from_str::<serde_json::Value>(line)
                .is_ok_and(|v| v["id"].as_i64() == Some(id))
        };
        if let Some(at) = self.unread.iter().position(|l| is_answer(l)) {
            return self.unread.remove(at);
        }
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).unwrap();
            assert!(n > 0, "server exited before answering id {id}");
            if is_answer(&line) {
                return line;
            }
            self.unread.push(line);
        }
    }

    fn request(&mut self, method: &str, params: &str) -> String {
        let id = self.send(&[(method, params)])[0];
        self.wait_for(id)
    }

    fn alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
            && Command::new("kill")
                .args(["-0", &self.child.id().to_string()])
                .status()
                .unwrap()
                .success()
    }

    fn initialize(&mut self) {
        self.request(
            "initialize",
            r#"{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"self-update-test","version":"0"}}"#,
        );
        writeln!(
            self.stdin,
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
        )
        .unwrap();
        self.stdin.flush().unwrap();
    }

    fn ping(&mut self) -> String {
        self.request("tools/call", r#"{"name":"ping","arguments":{}}"#)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn version_flag_prints_the_crate_version() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.trim(), format!("mcp-server {VERSION}"));
}

#[test]
fn replacing_the_binary_makes_later_results_carry_the_stale_notice() {
    let td = tempfile::tempdir().unwrap();
    let exe = td.path().join("mcp-server-copy");
    std::fs::copy(BIN, &exe).unwrap();

    let mut session = Session::spawn(&exe, td.path());
    session.initialize();

    let before = session.ping();
    assert!(before.contains("pong"), "{before}");
    assert!(
        before.contains(&format!("mcp-server {VERSION} (up to date)")),
        "{before}"
    );
    assert!(!before.contains("stale:"), "{before}");

    // What an installer does: a new file renamed over the old name.
    let fresh = td.path().join("mcp-server-new");
    std::fs::copy(BIN, &fresh).unwrap();
    std::fs::rename(&fresh, &exe).unwrap();

    let after = session.ping();
    assert!(
        after.contains(&format!("(stale: {VERSION} installed)")),
        "ping body should report stale state and the probed version: {after}"
    );
    // `ping` reports state in its body; a notice belongs on other tools.
    assert!(!after.contains("stale: mcp-server"), "{after}");

    let tool = session.request(
        "tools/call",
        r#"{"name":"read_source","arguments":{"file_path":"x"}}"#,
    );
    assert!(
        tool.contains(&format!(
            "stale: mcp-server {VERSION} running, {VERSION} installed"
        )),
        "{tool}"
    );
    // Reload is wired on unix; elsewhere the remedy is a client restart.
    let remedy = if cfg!(unix) {
        "call ping(reload=true) to load it"
    } else {
        "restart the MCP client to load it"
    };
    assert!(tool.contains(remedy), "{tool}");
}

#[cfg(unix)]
fn stale_session(td: &Path) -> (Session, std::path::PathBuf) {
    stale_session_with(td, &[])
}

#[cfg(unix)]
fn stale_session_with(td: &Path, args: &[&str]) -> (Session, std::path::PathBuf) {
    let exe = td.join("mcp-server-copy");
    std::fs::copy(BIN, &exe).unwrap();
    let mut session = Session::spawn_with(&exe, td, args);
    session.initialize();
    let fresh = td.join("mcp-server-new");
    std::fs::copy(BIN, &fresh).unwrap();
    std::fs::rename(&fresh, &exe).unwrap();
    (session, exe)
}

const READ_MISSING: &str = r#"{"name":"read_source","arguments":{"file_path":"x"}}"#;

#[cfg(unix)]
#[test]
fn ping_reload_replaces_the_process_in_place_and_resumes_the_session() {
    let td = tempfile::tempdir().unwrap();
    let (mut session, _exe) = stale_session(td.path());
    let pid = session.child.id();

    let stale = session.request("tools/call", READ_MISSING);
    assert!(
        stale.contains("call ping(reload=true) to load it"),
        "{stale}"
    );

    // The reload request and a second call arrive in one write.
    let ids = session.send(&[
        (
            "tools/call",
            r#"{"name":"ping","arguments":{"reload":true}}"#,
        ),
        ("tools/call", r#"{"name":"ping","arguments":{}}"#),
    ]);
    let reloaded = session.wait_for(ids[0]);
    assert!(
        reloaded.contains(&format!("reloaded: {VERSION} → {VERSION}")),
        "{reloaded}"
    );
    assert!(reloaded.contains("retained response ids"), "{reloaded}");
    assert!(!reloaded.contains("isError\":true"), "{reloaded}");
    assert!(
        !reloaded.contains("instructions changed"),
        "same build, same instructions: {reloaded}"
    );
    let pipelined = session.wait_for(ids[1]);
    assert!(pipelined.contains("pong"), "{pipelined}");
    assert!(
        pipelined.contains(&format!("mcp-server {VERSION} (up to date)")),
        "the new image watches the new file: {pipelined}"
    );

    // Same process, still the one child; the new image was never sent
    // `initialize` yet answers tool calls.
    assert_eq!(session.child.id(), pid);
    assert!(session.alive());
    let after = session.request("tools/call", READ_MISSING);
    assert!(!after.contains("stale:"), "no longer stale: {after}");

    // The new image can reload again once it is stale itself.
    let exe = td.path().join("mcp-server-copy");
    let fresh = td.path().join("mcp-server-newer");
    std::fs::copy(BIN, &fresh).unwrap();
    std::fs::rename(&fresh, &exe).unwrap();
    let again = session.request(
        "tools/call",
        r#"{"name":"ping","arguments":{"reload":true}}"#,
    );
    assert!(again.contains("reloaded:"), "{again}");
    assert!(session.alive());
}

/// Pull the `result_id` out of a budgeted answer line.
fn result_id(answer: &str) -> String {
    let frame: serde_json::Value = serde_json::from_str(answer).unwrap();
    let text = frame["result"]["content"][0]["text"].as_str().unwrap();
    let body: serde_json::Value =
        serde_json::from_str(text).unwrap_or_else(|_| panic!("answer was not budgeted: {answer}"));
    body["response_budget"]["result_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no result_id in {answer}"))
        .to_string()
}

#[cfg(unix)]
#[test]
fn a_result_id_retained_before_the_reload_expands_after_it() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("big.txt"), "line of text\n".repeat(6000)).unwrap();
    let (mut session, _exe) = stale_session_with(td.path(), &["--source-root", "."]);
    let big = session.request(
        "tools/call",
        r#"{"name":"read_source","arguments":{"file_path":"big.txt"}}"#,
    );
    let id = result_id(&big);

    let reloaded = session.request(
        "tools/call",
        r#"{"name":"ping","arguments":{"reload":true}}"#,
    );
    assert!(reloaded.contains("reloaded:"), "{reloaded}");

    let expanded = session.request(
        "tools/call",
        &format!(
            r#"{{"name":"expand_response","arguments":{{"result_id":"{id}","response":{{"mode":"full"}}}}}}"#
        ),
    );
    assert!(!expanded.contains("\"error\""), "{expanded}");
    let frame: serde_json::Value = serde_json::from_str(&expanded).unwrap();
    let text = frame["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.matches("line of text").count() >= 6000, "full payload");
}

#[cfg(unix)]
#[test]
fn ping_reload_is_refused_when_the_new_build_fails_its_selftest() {
    use std::os::unix::fs::PermissionsExt;
    let td = tempfile::tempdir().unwrap();
    let (mut session, exe) = stale_session(td.path());
    // The installer delivered something that cannot start.
    let broken = td.path().join("broken");
    std::fs::write(&broken, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&broken, &exe).unwrap();

    let refused = session.request(
        "tools/call",
        r#"{"name":"ping","arguments":{"reload":true}}"#,
    );
    assert!(refused.contains("\"isError\":true"), "{refused}");
    assert!(refused.contains("reload refused"), "{refused}");
    assert!(refused.contains("selftest"), "{refused}");

    // The old process is still serving.
    assert!(session.alive());
    let ping = session.request("tools/call", r#"{"name":"ping","arguments":{}}"#);
    assert!(ping.contains("pong"), "{ping}");
    assert!(
        ping.contains(&format!("mcp-server {VERSION} (stale")),
        "{ping}"
    );
}

#[test]
fn reload_preflight_exits_zero_on_a_clean_boot_and_nonzero_on_rejected_input() {
    let td = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(BIN)
            .args(args)
            .current_dir(td.path())
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_TOKEN")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    assert!(run(&["--reload-preflight"]).status.success());
    let missing = td.path().join("no-such-dir");
    let missing = missing.to_str().unwrap();
    assert!(!run(&["--source-root", missing, "--reload-preflight"])
        .status
        .success());
    assert!(!run(&["--mcp-config", missing, "--reload-preflight"])
        .status
        .success());
    assert!(!run(&["--no-such-flag", "--reload-preflight"])
        .status
        .success());
    assert_eq!(
        std::fs::read_dir(td.path()).unwrap().count(),
        0,
        "the preflight writes nothing"
    );
}

#[cfg(unix)]
#[test]
fn ping_reload_is_refused_when_the_new_build_passes_version_but_fails_preflight() {
    use std::os::unix::fs::PermissionsExt;
    let td = tempfile::tempdir().unwrap();
    let (mut session, exe) = stale_session(td.path());
    let flaky = td.path().join("flaky");
    std::fs::write(
        &flaky,
        "#!/bin/sh\ncase \"$*\" in *--reload-preflight*) exit 2;; esac\necho 'mcp-server 9.9.9'\n",
    )
    .unwrap();
    std::fs::set_permissions(&flaky, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&flaky, &exe).unwrap();

    let refused = session.request(
        "tools/call",
        r#"{"name":"ping","arguments":{"reload":true}}"#,
    );
    assert!(refused.contains("\"isError\":true"), "{refused}");
    assert!(refused.contains("--reload-preflight"), "{refused}");
    assert!(session.alive());
    let ping = session.request("tools/call", r#"{"name":"ping","arguments":{}}"#);
    assert!(ping.contains("pong"), "{ping}");
}
