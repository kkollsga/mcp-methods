//! Binary-level proof of the stale-binary notice and `--version`.
//!
//! The binary is copied into a tempdir and the *copy* is run, so the
//! test can replace the file the process was started from the way an
//! installer does: write a fresh file, rename it over the old name.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_mcp-server");
const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Session {
    fn spawn(exe: &Path, cwd: &Path) -> Session {
        let mut child = Command::new(exe)
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
        Session {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    fn request(&mut self, method: &str, params: &str) -> String {
        self.next_id += 1;
        let id = self.next_id;
        writeln!(
            self.stdin,
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
        )
        .unwrap();
        self.stdin.flush().unwrap();
        let needle = format!("\"id\":{id}");
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).unwrap();
            assert!(n > 0, "server exited before answering {method}");
            if line.contains(&needle) {
                return line;
            }
        }
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
    assert!(tool.contains("restart the MCP client"), "{tool}");
}
