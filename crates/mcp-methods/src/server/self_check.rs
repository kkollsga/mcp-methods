//! Self-update detection: notice when the binary a server was started
//! from has been replaced on disk, and tell the agent so.
//!
//! A long-lived stdio server keeps running the code it was started
//! with, so an upgrade (`pip install -U`, `cargo install`, a rebuilt
//! dev binary) is invisible to the session that already holds the old
//! process. [`SelfCheck`] names the file to watch; the framework
//! compares a stat snapshot taken at
//! [`ServerOptions::with_self_check`](crate::server::ServerOptions::with_self_check)
//! against the file on every tool call and, from the first mismatch on,
//! appends a one-line `stale:` notice to completed tool results.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// Probe returning the version of the build currently on disk, or
/// `None` when it cannot be determined.
pub(crate) type InstalledVersionProbe = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// What to watch and how to describe it. Build with [`SelfCheck::new`]
/// and hand to
/// [`ServerOptions::with_self_check`](crate::server::ServerOptions::with_self_check).
#[derive(Clone)]
pub struct SelfCheck {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) running_version: String,
    pub(crate) installed_version: Option<InstalledVersionProbe>,
}

impl SelfCheck {
    /// `name` is the display name used in the notice, `path` the file
    /// whose replacement means "stale" (normally the running
    /// executable), `running_version` the version this process was
    /// built as.
    pub fn new(
        name: impl Into<String>,
        path: impl Into<PathBuf>,
        running_version: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            running_version: running_version.into(),
            installed_version: None,
        }
    }

    /// Supply the probe that reports the on-disk version. Called at
    /// most once, when staleness is first detected; its result is
    /// cached for the life of the process. Without it the notice says
    /// "a newer build".
    pub fn with_installed_version(
        mut self,
        probe: impl Fn() -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.installed_version = Some(Arc::new(probe));
        self
    }
}

impl std::fmt::Debug for SelfCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelfCheck")
            .field("name", &self.name)
            .field("path", &self.path)
            .field("running_version", &self.running_version)
            .finish_non_exhaustive()
    }
}

/// Identity of the watched file: replacing it by rename changes the
/// inode, rewriting it in place changes mtime or size.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    mtime: Option<std::time::SystemTime>,
    size: u64,
}

fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let (dev, ino) = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    };
    Some(Fingerprint {
        #[cfg(unix)]
        dev,
        #[cfg(unix)]
        ino,
        mtime: meta.modified().ok(),
        size: meta.len(),
    })
}

/// Live state behind a configured [`SelfCheck`]: the construction-time
/// snapshot and the latch. Shared by every clone of the server.
#[doc(hidden)]
pub struct SelfCheckState {
    config: SelfCheck,
    snapshot: Option<Fingerprint>,
    stale: AtomicBool,
    /// The installed version (or `None` when unknown), probed once at
    /// latch time.
    installed: OnceLock<Option<String>>,
}

impl SelfCheckState {
    pub(crate) fn new(config: SelfCheck) -> Self {
        let snapshot = fingerprint(&config.path);
        Self {
            config,
            snapshot,
            stale: AtomicBool::new(false),
            installed: OnceLock::new(),
        }
    }

    /// Re-stat the watched file until the first mismatch (a missing
    /// file included), then latch. Returns whether the server is
    /// stale. The fresh path allocates nothing of its own.
    pub(crate) fn is_stale(&self) -> bool {
        if self.stale.load(Ordering::Acquire) {
            return true;
        }
        if fingerprint(&self.config.path) == self.snapshot {
            return false;
        }
        self.installed.get_or_init(|| {
            self.config
                .installed_version
                .as_ref()
                .and_then(|probe| probe())
        });
        self.stale.store(true, Ordering::Release);
        true
    }

    pub(crate) fn name(&self) -> &str {
        &self.config.name
    }

    pub(crate) fn running_version(&self) -> &str {
        &self.config.running_version
    }

    /// Installed version once latched stale and the probe produced one.
    pub(crate) fn installed_version(&self) -> Option<&str> {
        self.installed.get()?.as_deref()
    }

    /// Whether `ping(reload=true)` can load the new build. The notice's
    /// remedy clause follows this single predicate.
    fn reload_available(&self) -> bool {
        false
    }

    /// The notice text, or `None` while fresh.
    pub(crate) fn notice(&self) -> Option<String> {
        if !self.is_stale() {
            return None;
        }
        let remedy = if self.reload_available() {
            "call ping(reload=true) to load it"
        } else {
            "restart the MCP client to load it"
        };
        Some(format!(
            "stale: {} {} running, {} installed — {remedy}",
            self.name(),
            self.running_version(),
            self.installed_version().unwrap_or("a newer build"),
        ))
    }

    /// One `ping` line: identity plus freshness.
    pub(crate) fn ping_line(&self) -> String {
        if !self.is_stale() {
            return format!("{} {} (up to date)", self.name(), self.running_version());
        }
        match self.installed_version() {
            Some(v) => format!(
                "{} {} (stale: {v} installed)",
                self.name(),
                self.running_version()
            ),
            None => format!("{} {} (stale)", self.name(), self.running_version()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watched(dir: &Path) -> (PathBuf, SelfCheckState) {
        let path = dir.join("bin");
        std::fs::write(&path, b"v1").unwrap();
        let state = SelfCheckState::new(SelfCheck::new("t", &path, "1.0.0"));
        (path, state)
    }

    #[test]
    fn untouched_file_stays_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let (_p, state) = watched(dir.path());
        assert!(!state.is_stale());
        assert!(!state.is_stale());
        assert!(state.notice().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rename_replace_latches_stale_even_with_identical_content() {
        let dir = tempfile::tempdir().unwrap();
        let (path, state) = watched(dir.path());
        let fresh = dir.path().join("bin.new");
        std::fs::write(&fresh, b"v1").unwrap();
        std::fs::rename(&fresh, &path).unwrap();
        assert!(state.is_stale());
    }

    #[test]
    fn in_place_growth_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let (path, state) = watched(dir.path());
        std::fs::write(&path, b"version two").unwrap();
        assert!(state.is_stale());
    }

    #[test]
    fn deletion_is_stale_and_the_latch_holds_after_restoration() {
        let dir = tempfile::tempdir().unwrap();
        let (path, state) = watched(dir.path());
        std::fs::remove_file(&path).unwrap();
        assert!(state.is_stale());
        std::fs::write(&path, b"v1").unwrap();
        assert!(state.is_stale(), "latched: no more stats after a mismatch");
    }

    #[test]
    fn installed_probe_runs_once_and_feeds_the_notice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bin");
        std::fs::write(&path, b"v1").unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let state = SelfCheckState::new(
            SelfCheck::new("t", &path, "1.0.0").with_installed_version(move || {
                c.fetch_add(1, Ordering::SeqCst);
                Some("2.0.0".to_string())
            }),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no probe while fresh");
        std::fs::write(&path, b"version two").unwrap();
        let first = state.notice().unwrap();
        let second = state.notice().unwrap();
        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            first,
            "stale: t 1.0.0 running, 2.0.0 installed — restart the MCP client to load it"
        );
    }

    #[test]
    fn notice_falls_back_to_a_newer_build_without_a_probe_result() {
        let dir = tempfile::tempdir().unwrap();
        let (path, state) = watched(dir.path());
        std::fs::write(&path, b"version two").unwrap();
        assert!(state.notice().unwrap().contains("a newer build installed"));
    }
}
