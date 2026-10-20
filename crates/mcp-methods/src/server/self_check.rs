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

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Probe returning the version of the build currently on disk, or
/// `None` when it cannot be determined.
pub(crate) type InstalledVersionProbe = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Consumer veto: `Err(reason)` refuses a reload.
pub(crate) type CanReload = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;
/// Consumer hook run immediately before the process image is replaced.
pub(crate) type BeforeExec = Arc<dyn Fn() + Send + Sync>;
/// Check of the new build at the watched path: `Err(reason)` refuses.
pub(crate) type Selftest = Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

/// Opt-in reload settings; present exactly when `reload_argv` is non-empty.
#[derive(Clone)]
pub(crate) struct ReloadConfig {
    pub(crate) argv: Vec<OsString>,
    pub(crate) can_reload: Option<CanReload>,
    // Run only by the unix reload path; the builder stays cross-platform.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) before_exec: Option<BeforeExec>,
    pub(crate) selftest: Option<Selftest>,
}

/// What to watch and how to describe it. Build with [`SelfCheck::new`]
/// and hand to
/// [`ServerOptions::with_self_check`](crate::server::ServerOptions::with_self_check).
#[derive(Clone)]
pub struct SelfCheck {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) running_version: String,
    pub(crate) installed_version: Option<InstalledVersionProbe>,
    pub(crate) reload_argv: Vec<OsString>,
    pub(crate) can_reload: Option<CanReload>,
    pub(crate) before_exec: Option<BeforeExec>,
    pub(crate) selftest: Option<Selftest>,
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
            reload_argv: Vec::new(),
            can_reload: None,
            before_exec: None,
            selftest: None,
        }
    }

    /// Enable `ping(reload=true)`: when the watched build has been replaced,
    /// the process re-executes `argv` (program first, then its arguments) in
    /// place, keeping its PID and its stdio, and resumes the MCP session
    /// without a second `initialize`. Unix only, and only when the server is
    /// served through
    /// [`McpServer::serve_with_handover`](crate::server::McpServer::serve_with_handover)
    /// over [`StdioTransport::stdio`](crate::server::stdio::StdioTransport::stdio);
    /// otherwise the notice tells the user to restart the client instead.
    pub fn with_reload(mut self, argv: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
        self.reload_argv = argv.into_iter().map(Into::into).collect();
        self
    }

    /// Veto a reload for a reason only the consumer knows (an uncommitted
    /// write, an exclusive lock held); `Err(reason)` is shown to the agent
    /// and the server keeps running. Only used with
    /// [`with_reload`](Self::with_reload).
    pub fn with_can_reload(
        mut self,
        check: impl Fn() -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        self.can_reload = Some(Arc::new(check));
        self
    }

    /// Run `hook` right before the process image is replaced (flush and
    /// release whatever must not outlive the old code).
    pub fn with_before_exec(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.before_exec = Some(Arc::new(hook));
        self
    }

    /// Check the new build before committing to it. Called on a blocking
    /// thread with the watched path; `Err(reason)` aborts the reload and the
    /// old process keeps serving.
    pub fn with_selftest(
        mut self,
        check: impl Fn(&Path) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        self.selftest = Some(Arc::new(check));
        self
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

/// Guard from [`SelfCheckState::enter_call`].
pub(crate) struct InFlight(Arc<SelfCheckState>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::AcqRel);
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
    /// The installed version (or `None` when unknown), probed once, on a
    /// blocking thread, after the latch trips.
    installed: tokio::sync::OnceCell<Option<String>>,
    reload: Option<ReloadConfig>,
    /// Transport handles, attached by `serve_with_handover`.
    link: OnceLock<crate::server::stdio::TransportLink>,
    /// Tool calls currently inside `call_tool`.
    in_flight: AtomicUsize,
    /// Version of the process this one replaced, until the replayed
    /// `ping(reload=true)` consumes it.
    reloaded_from: Mutex<Option<String>>,
}

impl SelfCheckState {
    pub(crate) fn new(config: SelfCheck) -> Self {
        let snapshot = fingerprint(&config.path);
        let reload = (!config.reload_argv.is_empty()).then(|| ReloadConfig {
            argv: config.reload_argv.clone(),
            can_reload: config.can_reload.clone(),
            before_exec: config.before_exec.clone(),
            selftest: config.selftest.clone(),
        });
        Self {
            config,
            snapshot,
            stale: AtomicBool::new(false),
            installed: tokio::sync::OnceCell::new(),
            reload,
            link: OnceLock::new(),
            in_flight: AtomicUsize::new(0),
            reloaded_from: Mutex::new(None),
        }
    }

    /// Re-stat the watched file until the first mismatch (a missing
    /// file included), then latch. The fresh path allocates nothing of
    /// its own.
    fn latch(&self) -> bool {
        if self.stale.load(Ordering::Acquire) {
            return true;
        }
        if fingerprint(&self.config.path) == self.snapshot {
            return false;
        }
        self.stale.store(true, Ordering::Release);
        true
    }

    /// [`latch`](Self::latch), then (once, on a blocking thread) the
    /// installed-version probe. Every reader of the stale state calls this
    /// first; it returns whether the server is stale.
    pub(crate) async fn refresh(&self) -> bool {
        if !self.latch() {
            return false;
        }
        self.installed
            .get_or_init(|| async {
                let probe = self.config.installed_version.clone()?;
                tokio::task::spawn_blocking(move || probe())
                    .await
                    .ok()
                    .flatten()
            })
            .await;
        true
    }

    /// Whether [`refresh`](Self::refresh) has seen a replacement.
    fn is_stale(&self) -> bool {
        self.stale.load(Ordering::Acquire)
    }

    pub(crate) fn name(&self) -> &str {
        &self.config.name
    }

    pub(crate) fn running_version(&self) -> &str {
        &self.config.running_version
    }

    /// Installed version once refreshed stale and the probe produced one.
    pub(crate) fn installed_version(&self) -> Option<&str> {
        self.installed.get()?.as_deref()
    }

    /// Whether reload was configured at all.
    pub(crate) fn reload_config(&self) -> Option<&ReloadConfig> {
        self.reload.as_ref()
    }

    pub(crate) fn path(&self) -> &Path {
        &self.config.path
    }

    /// The transport handles, once `serve_with_handover` attached them.
    pub(crate) fn link(&self) -> Option<&crate::server::stdio::TransportLink> {
        self.link.get()
    }

    pub(crate) fn attach(&self, link: crate::server::stdio::TransportLink) {
        let _ = self.link.set(link);
    }

    /// Count a tool call as in flight until the guard drops.
    pub(crate) fn enter_call(self: &Arc<Self>) -> InFlight {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        InFlight(self.clone())
    }

    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    /// Record that this process replaced `old_version`.
    pub(crate) fn mark_reloaded(&self, old_version: String) {
        *self.reloaded_from.lock().unwrap_or_else(|e| e.into_inner()) = Some(old_version);
    }

    /// The one-shot "reloaded from" marker, consumed by the first
    /// `ping(reload=true)` after a reload.
    pub(crate) fn take_reloaded(&self) -> Option<String> {
        self.reloaded_from
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Why `ping(reload=true)` cannot run here, or `None` when it can.
    /// `ping_present` is whether the server still serves a `ping` route.
    pub(crate) fn reload_unavailable(&self, ping_present: bool) -> Option<&'static str> {
        if self.reload.is_none() {
            return Some("reload is not configured for this server");
        }
        if !cfg!(unix) {
            return Some("reload is unsupported on this platform");
        }
        if !ping_present {
            return Some("this server has no ping tool to trigger a reload");
        }
        match self.link() {
            None => Some("reload needs the framework stdio transport (serve_with_handover)"),
            Some(link) if !link.input.pump_active() => {
                Some("reload is unavailable: stdin is not pollable (a file or terminal)")
            }
            Some(_) => None,
        }
    }

    /// Whether `ping(reload=true)` can load the new build. The notice's
    /// remedy clause follows this single predicate.
    fn reload_available(&self, ping_present: bool) -> bool {
        self.reload_unavailable(ping_present).is_none()
    }

    /// The notice text, or `None` while fresh. Call after
    /// [`refresh`](Self::refresh).
    pub(crate) fn notice(&self, ping_present: bool) -> Option<String> {
        if !self.is_stale() {
            return None;
        }
        let remedy = if self.reload_available(ping_present) {
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

    #[tokio::test]
    async fn untouched_file_stays_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let (_p, state) = watched(dir.path());
        assert!(!state.refresh().await);
        assert!(!state.refresh().await);
        assert!(state.notice(true).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rename_replace_latches_stale_even_with_identical_content() {
        let dir = tempfile::tempdir().unwrap();
        let (path, state) = watched(dir.path());
        let fresh = dir.path().join("bin.new");
        std::fs::write(&fresh, b"v1").unwrap();
        std::fs::rename(&fresh, &path).unwrap();
        assert!(state.refresh().await);
    }

    #[tokio::test]
    async fn in_place_growth_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let (path, state) = watched(dir.path());
        std::fs::write(&path, b"version two").unwrap();
        assert!(state.refresh().await);
    }

    #[tokio::test]
    async fn deletion_is_stale_and_the_latch_holds_after_restoration() {
        let dir = tempfile::tempdir().unwrap();
        let (path, state) = watched(dir.path());
        std::fs::remove_file(&path).unwrap();
        assert!(state.refresh().await);
        std::fs::write(&path, b"v1").unwrap();
        assert!(
            state.refresh().await,
            "latched: no more stats after a mismatch"
        );
    }

    #[tokio::test]
    async fn installed_probe_runs_once_off_the_runtime_thread_and_feeds_the_notice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bin");
        std::fs::write(&path, b"v1").unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let caller = std::thread::current().id();
        let state = SelfCheckState::new(
            SelfCheck::new("t", &path, "1.0.0").with_installed_version(move || {
                assert_ne!(std::thread::current().id(), caller);
                c.fetch_add(1, Ordering::SeqCst);
                Some("2.0.0".to_string())
            }),
        );
        state.refresh().await;
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no probe while fresh");
        std::fs::write(&path, b"version two").unwrap();
        state.refresh().await;
        let first = state.notice(true).unwrap();
        state.refresh().await;
        let second = state.notice(true).unwrap();
        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            first,
            "stale: t 1.0.0 running, 2.0.0 installed — restart the MCP client to load it"
        );
    }

    #[tokio::test]
    async fn notice_falls_back_to_a_newer_build_without_a_probe_result() {
        let dir = tempfile::tempdir().unwrap();
        let (path, state) = watched(dir.path());
        std::fs::write(&path, b"version two").unwrap();
        state.refresh().await;
        assert!(state
            .notice(true)
            .unwrap()
            .contains("a newer build installed"));
    }

    #[test]
    fn reload_is_unavailable_without_config_or_transport() {
        let dir = tempfile::tempdir().unwrap();
        let (_p, state) = watched(dir.path());
        assert!(state
            .reload_unavailable(true)
            .unwrap()
            .contains("not configured"));
        let configured = SelfCheckState::new(
            SelfCheck::new("t", dir.path().join("bin"), "1.0.0").with_reload(["/bin/true"]),
        );
        let why = configured.reload_unavailable(true).unwrap();
        assert!(
            why.contains("serve_with_handover") || why.contains("unsupported"),
            "{why}"
        );
    }
}
