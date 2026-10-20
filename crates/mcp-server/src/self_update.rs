//! Wiring for the framework's stale-binary notice: watch the running
//! executable and probe the on-disk build for its version.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mcp_methods::server::SelfCheck;

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The executable path to watch. Linux appends " (deleted)" to
/// `/proc/self/exe` once the file was unlinked; the name that matters
/// is the one without it.
fn normalize_exe_path(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(stripped) => PathBuf::from(stripped),
        None => path,
    }
}

/// `mcp-server 1.2.3` → `1.2.3`.
fn parse_version_output(stdout: &str) -> Option<String> {
    let version = stdout.split_whitespace().last()?;
    version
        .starts_with(|c: char| c.is_ascii_digit())
        .then(|| version.to_string())
}

/// Run `<exe> --version`, bounded by [`PROBE_TIMEOUT`]; `None` on any
/// failure (missing file, non-zero exit, timeout, unparseable output).
fn probe_installed_version(exe: &Path) -> Option<String> {
    let mut child = Command::new(exe)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait().ok()? {
            Some(status) if status.success() => break,
            Some(_) => return None,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    parse_version_output(&out)
}

/// The [`SelfCheck`] for this process, or `None` when the executable
/// path cannot be determined.
pub fn self_check() -> Option<SelfCheck> {
    let exe = normalize_exe_path(std::env::current_exe().ok()?);
    let probe_path = exe.clone();
    Some(
        SelfCheck::new("mcp-server", exe, env!("CARGO_PKG_VERSION"))
            .with_installed_version(move || probe_installed_version(&probe_path)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_linux_deleted_suffix_only() {
        assert_eq!(
            normalize_exe_path(PathBuf::from("/opt/bin/mcp-server (deleted)")),
            PathBuf::from("/opt/bin/mcp-server")
        );
        assert_eq!(
            normalize_exe_path(PathBuf::from("/opt/bin/mcp-server")),
            PathBuf::from("/opt/bin/mcp-server")
        );
    }

    #[test]
    fn parses_clap_version_output() {
        assert_eq!(
            parse_version_output("mcp-server 0.4.13\n").as_deref(),
            Some("0.4.13")
        );
        assert_eq!(parse_version_output("garbage\n"), None);
        assert_eq!(parse_version_output(""), None);
    }

    #[test]
    fn probe_of_a_missing_binary_is_none() {
        assert_eq!(
            probe_installed_version(Path::new("/nonexistent/mcp-server")),
            None
        );
    }
}
