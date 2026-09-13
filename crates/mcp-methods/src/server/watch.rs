//! Filesystem-watcher subsystem for `--watch DIR` mode.
//!
//! Boots a debounced recursive watcher on the configured directory and
//! invokes a caller-supplied callback when files change. Downstream
//! binaries register callbacks to drive whatever rebuild they need —
//! kglite-mcp-server, for example, wires this to `code_tree::build()`
//! against the watched directory and atomic-swaps the active graph.
//!
//! mcp-methods's binary on its own does not own a rebuild target;
//! it logs change events at INFO level and forwards them to any
//! registered callback. When no callback is set the watcher still
//! runs, so the change events show up in stderr.
//!
//! ## Default skip patterns
//!
//! Events matching conventional noise paths ([`DEFAULT_SKIP_SUBSTRINGS`]
//! and [`DEFAULT_SKIP_EXTENSIONS`]) are dropped before the callback
//! runs — `.git/`, `target/`, `node_modules/`, `__pycache__/`, `*.pyc`,
//! editor swap files, etc. A wide sandbox under active development
//! generates hundreds of these per second; without the filter every
//! consumer either rebuilds wastefully or implements the same skip
//! list. With it, consumers see only events that could plausibly
//! matter.
//!
//! Bindings that need every mutation or unknown-event path, including
//! conventional noise paths, pass [`WatchConfig::unfiltered`] to
//! [`watch_with_config`]. Non-mutating access events are always discarded
//! before debounce, independently of this path filter.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify_debouncer_mini::notify::{
    Config as NotifyConfig, Event, EventHandler, EventKind, RecommendedWatcher, RecursiveMode,
    Watcher, WatcherKind,
};
use notify_debouncer_mini::{
    new_debouncer_opt, Config as DebounceConfig, DebounceEventHandler, DebounceEventResult,
    Debouncer,
};

/// Callback invoked on a debounced file-change event.
///
/// `paths` is the deduplicated set of paths reported as changed within
/// the debounce window, **after** the active [`WatchConfig`]'s skip
/// filter has run. The callback runs on a background thread; keep it
/// non-blocking or push work onto a channel.
pub type ChangeHandler = Arc<dyn Fn(&[PathBuf]) + Send + Sync>;

/// Default debounce window — short enough to feel responsive, long
/// enough to coalesce noisy editor saves and IDE temp-file dance.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(500);

/// Default substrings to skip. A path containing any of these as a
/// substring is dropped before the callback runs.
///
/// Conventional build / VCS / cache directories that no graph builder,
/// search index, or rebuild target should care about. The substrings
/// are anchored with `/` on both sides where appropriate so they don't
/// false-match (e.g. `/.git/` matches `.../my-repo/.git/HEAD` but not
/// a file literally named `.gitignore`).
pub const DEFAULT_SKIP_SUBSTRINGS: &[&str] = &[
    "/.git/",         // git objects + index churn on any git operation
    "/target/",       // Cargo build artifacts (worst storm offender)
    "/node_modules/", // npm/yarn install storms + cache writes
    "/__pycache__/",  // CPython bytecode dirs
    "/.venv/",        // Python venv internals
    "/build/",        // generic build outputs across many tools
    "/dist/",         // generic build/distribution outputs
    "/.DS_Store",     // macOS Finder metadata churn
];

/// Default file extensions to skip (without the leading dot).
pub const DEFAULT_SKIP_EXTENSIONS: &[&str] = &[
    "pyc", "pyo", // CPython bytecode files
    "swp", "swo", // vim swap files
    "tmp", // atomic-save temp files
];

/// Configuration for a [`watch_with_config`] call. Controls which
/// events reach the callback.
#[derive(Clone, Debug)]
pub struct WatchConfig {
    /// Substrings to skip. A path containing any of these (anywhere)
    /// is dropped before the callback fires. Matching is
    /// case-sensitive and allocation-free.
    pub skip_substrings: Vec<String>,
    /// File extensions (without leading dot) to skip. Matching uses
    /// the path's last extension via [`Path::extension`] and is
    /// case-sensitive.
    pub skip_extensions: Vec<String>,
}

impl Default for WatchConfig {
    /// The recommended default: skip [`DEFAULT_SKIP_SUBSTRINGS`] +
    /// [`DEFAULT_SKIP_EXTENSIONS`]. Most consumers want this — see
    /// [`unfiltered`](Self::unfiltered) for the escape hatch.
    fn default() -> Self {
        Self {
            skip_substrings: DEFAULT_SKIP_SUBSTRINGS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            skip_extensions: DEFAULT_SKIP_EXTENSIONS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        }
    }
}

impl WatchConfig {
    /// Empty path skip set — every debounced mutation or unknown-event path
    /// reaches the callback, including `.git/objects/...` writes. Non-mutating
    /// access events remain filtered before debounce.
    pub fn unfiltered() -> Self {
        Self {
            skip_substrings: Vec::new(),
            skip_extensions: Vec::new(),
        }
    }

    /// Test a path against the active skip set. `true` → skip; `false`
    /// → forward to callback. Public so consumers building their own
    /// orchestration over the same conventions can reuse the predicate
    /// without re-deriving it.
    pub fn is_skipped(&self, path: &Path) -> bool {
        // Substring match against the full path. UTF-8 fallback is
        // lossy: paths that aren't valid UTF-8 skip the substring
        // check (we still run the extension check below). On the
        // platforms we care about (macOS / Linux / Windows) this is
        // never the hot path's bottleneck.
        if let Some(s) = path.to_str() {
            for needle in &self.skip_substrings {
                if s.contains(needle.as_str()) {
                    return true;
                }
            }
        }
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            for skip in &self.skip_extensions {
                if ext == skip {
                    return true;
                }
            }
        }
        false
    }
}

/// Apply a [`WatchConfig`]'s skip filter to a batch of event paths,
/// keeping only those that should reach the callback. The debouncer
/// drops the whole batch (no callback at all) when this returns empty —
/// the pure-noise-storm case (`cargo build`'s `target/` churn, a `git`
/// operation's `.git/` writes). Extracted as a free function so the
/// retention decision is unit-testable without depending on a real
/// watcher's platform-specific event-path semantics.
fn retain_unskipped(
    config: &WatchConfig,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|p| !config.is_skipped(p))
        .collect()
}

/// Prevent non-mutating access notifications from entering the path-only
/// debouncer, which cannot preserve their event kind.
struct MutationWatcher<W> {
    inner: W,
}

impl<W: Watcher> Watcher for MutationWatcher<W> {
    fn new<F: EventHandler>(
        mut event_handler: F,
        config: NotifyConfig,
    ) -> notify_debouncer_mini::notify::Result<Self> {
        let inner = W::new(
            move |result: notify_debouncer_mini::notify::Result<Event>| match result {
                Ok(event) if matches!(event.kind, EventKind::Access(_)) => {}
                result => event_handler.handle_event(result),
            },
            config,
        )?;
        Ok(Self { inner })
    }

    fn watch(
        &mut self,
        path: &Path,
        recursive_mode: RecursiveMode,
    ) -> notify_debouncer_mini::notify::Result<()> {
        self.inner.watch(path, recursive_mode)
    }

    fn unwatch(&mut self, path: &Path) -> notify_debouncer_mini::notify::Result<()> {
        self.inner.unwatch(path)
    }

    fn configure(&mut self, option: NotifyConfig) -> notify_debouncer_mini::notify::Result<bool> {
        self.inner.configure(option)
    }

    fn kind() -> WatcherKind {
        W::kind()
    }
}

fn new_mutation_debouncer<F: DebounceEventHandler, W: Watcher>(
    debounce: Duration,
    event_handler: F,
) -> notify_debouncer_mini::notify::Result<Debouncer<MutationWatcher<W>>> {
    new_debouncer_opt(
        DebounceConfig::default().with_timeout(debounce),
        event_handler,
    )
}

/// Active watcher handle. Drop to stop watching.
pub struct WatchHandle {
    _debouncer: Debouncer<MutationWatcher<RecommendedWatcher>>,
}

/// Spawn a recursive debounced watcher on `dir` using the default
/// [`WatchConfig`] (skips conventional noise paths — `.git/`,
/// `target/`, `node_modules/`, etc.).
///
/// Returns a handle whose `Drop` impl tears the watcher down. Errors
/// surface synchronously if the path is not a directory or the platform
/// watcher refuses to register.
///
/// For control over the skip set, use [`watch_with_config`].
pub fn watch(
    dir: &Path,
    on_change: Option<ChangeHandler>,
    debounce: Option<Duration>,
) -> Result<WatchHandle> {
    watch_with_config(dir, on_change, debounce, WatchConfig::default())
}

/// Spawn a recursive debounced watcher with an explicit
/// [`WatchConfig`]. Behaves like [`watch`] except the skip set is
/// caller-controlled — pass [`WatchConfig::unfiltered`] to retain every
/// debounced mutation or unknown-event path, or build a custom config to add /
/// remove patterns. Non-mutating access events are always filtered first.
pub fn watch_with_config(
    dir: &Path,
    on_change: Option<ChangeHandler>,
    debounce: Option<Duration>,
    config: WatchConfig,
) -> Result<WatchHandle> {
    if !dir.is_dir() {
        anyhow::bail!("--watch path is not a directory: {}", dir.display());
    }
    let debounce = debounce.unwrap_or(DEFAULT_DEBOUNCE);
    let dir_for_log = dir.to_path_buf();
    let on_change = on_change.unwrap_or_else(|| {
        Arc::new(|_| {
            // No-op callback when no downstream consumer is configured.
        })
    });

    let mut debouncer = new_mutation_debouncer::<_, RecommendedWatcher>(
        debounce,
        move |result: DebounceEventResult| match result {
            Ok(events) => {
                // Drop skipped events before they're handed to the
                // callback or counted in the log line. Empty post-filter
                // batches (a pure-noise storm like `cargo build`'s
                // `target/` churn) return without a callback invocation
                // at all.
                let paths = retain_unskipped(&config, events.into_iter().map(|e| e.path));
                if paths.is_empty() {
                    return;
                }
                tracing::info!(
                    root = %dir_for_log.display(),
                    changed = paths.len(),
                    "watch: file change debounced"
                );
                on_change(&paths);
            }
            Err(e) => {
                tracing::warn!(error = %e, "watch: error from notify");
            }
        },
    )
    .context("failed to construct file-system debouncer")?;

    debouncer
        .watcher()
        .watch(dir, RecursiveMode::Recursive)
        .with_context(|| format!("failed to watch {}", dir.display()))?;

    tracing::info!(root = %dir.display(), debounce_ms = debounce.as_millis() as u64, "watch: active");
    Ok(WatchHandle {
        _debouncer: debouncer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify_debouncer_mini::notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind,
        RenameMode,
    };
    use notify_debouncer_mini::notify::{Error as NotifyError, Result as NotifyResult};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    type ScriptStep = (Duration, NotifyResult<Event>);

    fn scripted_events() -> &'static Mutex<Vec<ScriptStep>> {
        static EVENTS: OnceLock<Mutex<Vec<ScriptStep>>> = OnceLock::new();
        EVENTS.get_or_init(|| Mutex::new(Vec::new()))
    }

    fn scripted_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct ScriptedWatcher;

    impl Watcher for ScriptedWatcher {
        fn new<F: EventHandler>(mut event_handler: F, _config: NotifyConfig) -> NotifyResult<Self> {
            let script = std::mem::take(&mut *scripted_events().lock().unwrap());
            std::thread::spawn(move || {
                for (delay, event) in script {
                    std::thread::sleep(delay);
                    event_handler.handle_event(event);
                }
            });
            Ok(Self)
        }

        fn watch(&mut self, _path: &Path, _recursive_mode: RecursiveMode) -> NotifyResult<()> {
            Ok(())
        }

        fn unwatch(&mut self, _path: &Path) -> NotifyResult<()> {
            Ok(())
        }

        fn kind() -> WatcherKind {
            WatcherKind::NullWatcher
        }
    }

    fn event(kind: EventKind, path: &str) -> Event {
        Event::new(kind).add_path(PathBuf::from(path))
    }

    struct BoundedTestDrop<T: Send + 'static> {
        owned: Option<T>,
        deadline: Duration,
        timeout_message: &'static str,
    }

    impl<T: Send + 'static> BoundedTestDrop<T> {
        fn new(owned: T, deadline: Duration, timeout_message: &'static str) -> Self {
            Self {
                owned: Some(owned),
                deadline,
                timeout_message,
            }
        }

        fn finish(mut self) {
            self.stop();
        }

        fn stop(&mut self) {
            let Some(owned) = self.owned.take() else {
                return;
            };
            let (stopped_tx, stopped_rx) = mpsc::channel();
            std::thread::spawn(move || {
                drop(owned);
                let _ = stopped_tx.send(());
            });
            let stopped = stopped_rx.recv_timeout(self.deadline);
            if stopped.is_err() && !std::thread::panicking() {
                panic!("{}", self.timeout_message);
            }
        }
    }

    impl<T: Send + 'static> Drop for BoundedTestDrop<T> {
        fn drop(&mut self) {
            self.stop();
        }
    }

    struct WatcherTestGuard {
        owned: BoundedTestDrop<(WatchHandle, tempfile::TempDir)>,
    }

    impl WatcherTestGuard {
        fn new(handle: WatchHandle, fixture: tempfile::TempDir) -> Self {
            Self {
                owned: BoundedTestDrop::new(
                    (handle, fixture),
                    Duration::from_secs(2),
                    "watcher teardown exceeded two seconds",
                ),
            }
        }

        fn path(&self) -> &Path {
            self.owned.owned.as_ref().unwrap().1.path()
        }

        fn finish(self) {
            self.owned.finish();
        }
    }

    fn run_script(
        script: Vec<ScriptStep>,
        debounce: Duration,
    ) -> (
        mpsc::Receiver<DebounceEventResult>,
        Debouncer<MutationWatcher<ScriptedWatcher>>,
    ) {
        *scripted_events().lock().unwrap() = script;
        let (tx, rx) = mpsc::channel();
        let debouncer = new_mutation_debouncer::<_, ScriptedWatcher>(debounce, tx).unwrap();
        (rx, debouncer)
    }

    fn run_exact_script(
        script: Vec<ScriptStep>,
        debounce: Duration,
    ) -> (
        mpsc::Receiver<DebounceEventResult>,
        Debouncer<MutationWatcher<ScriptedWatcher>>,
    ) {
        *scripted_events().lock().unwrap() = script;
        let (tx, rx) = mpsc::channel();
        let debouncer = new_debouncer_opt::<_, MutationWatcher<ScriptedWatcher>>(
            DebounceConfig::default()
                .with_timeout(debounce)
                .with_batch_mode(false),
            tx,
        )
        .unwrap();
        (rx, debouncer)
    }

    #[test]
    fn raw_access_events_never_enter_the_debouncer() {
        let _guard = scripted_test_lock().lock().unwrap();
        let access_kinds = [
            AccessKind::Any,
            AccessKind::Read,
            AccessKind::Open(AccessMode::Any),
            AccessKind::Open(AccessMode::Execute),
            AccessKind::Open(AccessMode::Read),
            AccessKind::Open(AccessMode::Write),
            AccessKind::Open(AccessMode::Other),
            AccessKind::Close(AccessMode::Any),
            AccessKind::Close(AccessMode::Execute),
            AccessKind::Close(AccessMode::Read),
            AccessKind::Close(AccessMode::Write),
            AccessKind::Close(AccessMode::Other),
            AccessKind::Other,
        ];
        let script = access_kinds
            .into_iter()
            .map(|kind| {
                (
                    Duration::ZERO,
                    Ok(event(EventKind::Access(kind), "/source.rs")),
                )
            })
            .collect();
        let (rx, _debouncer) = run_script(script, Duration::from_millis(30));
        assert!(rx.recv_timeout(Duration::from_millis(150)).is_err());
    }

    #[test]
    fn raw_mutations_unknown_events_and_errors_survive_the_debouncer() {
        let _guard = scripted_test_lock().lock().unwrap();
        let mutation_kinds = [
            EventKind::Create(CreateKind::Any),
            EventKind::Create(CreateKind::File),
            EventKind::Create(CreateKind::Folder),
            EventKind::Create(CreateKind::Other),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            EventKind::Modify(ModifyKind::Data(DataChange::Size)),
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            EventKind::Modify(ModifyKind::Data(DataChange::Other)),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            EventKind::Modify(ModifyKind::Other),
            EventKind::Remove(RemoveKind::Any),
            EventKind::Remove(RemoveKind::File),
            EventKind::Remove(RemoveKind::Folder),
            EventKind::Remove(RemoveKind::Other),
            EventKind::Any,
            EventKind::Other,
        ];
        let mut script: Vec<_> = mutation_kinds
            .into_iter()
            .enumerate()
            .map(|(index, kind)| {
                (
                    Duration::ZERO,
                    Ok(event(kind, &format!("/source-{index}.rs"))),
                )
            })
            .collect();
        script.push((
            Duration::ZERO,
            Err(NotifyError::generic("raw watcher failure")),
        ));
        let expected = mutation_kinds.len();
        let (rx, _debouncer) = run_script(script, Duration::from_millis(20));
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut delivered = 0;
        let mut saw_error = false;
        while delivered < expected || !saw_error {
            let result = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            match result {
                Ok(events) => delivered += events.len(),
                Err(error) => saw_error |= error.to_string().contains("raw watcher failure"),
            }
        }
        assert_eq!(delivered, expected);
        assert!(saw_error);
    }

    #[test]
    fn access_storm_does_not_postpone_a_pending_mutation() {
        let _guard = scripted_test_lock().lock().unwrap();
        let debounce = Duration::from_millis(120);
        let mut script = vec![(
            Duration::ZERO,
            Ok(event(
                EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                "/source.rs",
            )),
        )];
        for _ in 0..5 {
            script.push((
                Duration::from_millis(30),
                Ok(event(
                    EventKind::Access(AccessKind::Open(AccessMode::Read)),
                    "/source.rs",
                )),
            ));
        }
        let started = Instant::now();
        let (rx, _debouncer) = run_exact_script(script, debounce);
        let events = rx
            .recv_timeout(Duration::from_millis(220))
            .unwrap()
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, PathBuf::from("/source.rs"));
        assert!(started.elapsed() < Duration::from_millis(220));
    }

    #[test]
    fn watch_rejects_non_directory() {
        let result = watch(Path::new("/this/does/not/exist"), None, None);
        assert!(result.is_err());
    }

    #[test]
    fn watch_starts_and_drops_clean() {
        let dir = tempfile::tempdir().unwrap();
        let handle = watch(dir.path(), None, Some(Duration::from_millis(100))).unwrap();
        WatcherTestGuard::new(handle, dir).finish();
    }

    #[test]
    fn bounded_cleanup_preserves_the_original_test_body_panic() {
        struct DelayedDrop {
            dropped: mpsc::Sender<()>,
        }

        impl Drop for DelayedDrop {
            fn drop(&mut self) {
                std::thread::sleep(Duration::from_millis(30));
                let _ = self.dropped.send(());
            }
        }

        let (dropped_tx, dropped_rx) = mpsc::channel();
        let panic = std::panic::catch_unwind(|| {
            let _cleanup = BoundedTestDrop::new(
                DelayedDrop {
                    dropped: dropped_tx,
                },
                Duration::from_millis(1),
                "secondary cleanup timeout",
            );
            panic!("original test-body failure");
        })
        .expect_err("test body must panic");

        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"original test-body failure")
        );
        dropped_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cleanup worker did not drop the owned value");
    }

    #[test]
    fn callback_fires_on_file_change() {
        use std::thread::sleep;
        let dir = tempfile::tempdir().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_cb = counter.clone();
        let cb: ChangeHandler = Arc::new(move |_paths: &[PathBuf]| {
            counter_for_cb.fetch_add(1, Ordering::SeqCst);
        });
        let handle = watch(dir.path(), Some(cb), Some(Duration::from_millis(100))).unwrap();
        let watcher = WatcherTestGuard::new(handle, dir);
        sleep(Duration::from_millis(50)); // let watcher settle
        std::fs::write(watcher.path().join("a.txt"), "hi").unwrap();
        sleep(Duration::from_millis(400)); // debounce + buffer
        assert!(
            counter.load(Ordering::SeqCst) >= 1,
            "expected callback to fire at least once after file write"
        );
        watcher.finish();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_watcher_distinguishes_access_from_real_mutations() {
        use std::fs::{self, OpenOptions};
        use std::io::{Read, Write};

        const DEBOUNCE: Duration = Duration::from_millis(100);
        const CALLBACK_DEADLINE: Duration = Duration::from_secs(2);
        const QUIET_WINDOW: Duration = Duration::from_millis(350);

        fn receive_path(rx: &mpsc::Receiver<Vec<PathBuf>>, expected: &Path, operation: &str) {
            let deadline = Instant::now() + CALLBACK_DEADLINE;
            while Instant::now() < deadline {
                let paths = rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap_or_else(|_| {
                        panic!(
                            "{operation} produced no callback for {}",
                            expected.display()
                        )
                    });
                if paths.iter().any(|path| path == expected) {
                    return;
                }
            }
            panic!("{operation} never delivered {}", expected.display());
        }

        fn drain(rx: &mpsc::Receiver<Vec<PathBuf>>) {
            const MAX_BATCHES: usize = 1024;
            let deadline = Instant::now() + Duration::from_millis(100);
            for _ in 0..MAX_BATCHES {
                match rx.try_recv() {
                    Ok(_) if Instant::now() < deadline => {}
                    Ok(_) => panic!("watcher queue drain exceeded 100 ms"),
                    Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => return,
                }
            }
            panic!("watcher queue drain exceeded {MAX_BATCHES} batches");
        }

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.rs");
        fs::write(&source, "initial").unwrap();
        let (tx, rx) = mpsc::channel();
        let callback: ChangeHandler = Arc::new(move |paths| tx.send(paths.to_vec()).unwrap());
        let handle = watch(dir.path(), Some(callback), Some(DEBOUNCE)).unwrap();
        let watcher = WatcherTestGuard::new(handle, dir);

        let mut contents = String::new();
        fs::File::open(&source)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert_eq!(contents, "initial");
        assert!(
            rx.recv_timeout(QUIET_WINDOW).is_err(),
            "read/open triggered callback"
        );

        let writable = OpenOptions::new().write(true).open(&source).unwrap();
        drop(writable);
        assert!(
            rx.recv_timeout(QUIET_WINDOW).is_err(),
            "writable open without a write triggered callback"
        );

        fs::write(&source, "overwritten").unwrap();
        receive_path(&rx, &source, "overwrite");
        drain(&rx);

        let mut truncated = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&source)
            .unwrap();
        truncated.write_all(b"short").unwrap();
        truncated.sync_all().unwrap();
        drop(truncated);
        receive_path(&rx, &source, "truncate");
        drain(&rx);

        let renamed = watcher.path().join("renamed.rs");
        fs::rename(&source, &renamed).unwrap();
        let deadline = Instant::now() + CALLBACK_DEADLINE;
        let mut saw_rename_endpoint = false;
        while Instant::now() < deadline && !saw_rename_endpoint {
            let paths = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("rename produced no callback");
            saw_rename_endpoint = paths.iter().any(|path| path == &source || path == &renamed);
        }
        assert!(saw_rename_endpoint, "rename delivered neither endpoint");
        drain(&rx);

        fs::write(&source, "replace me").unwrap();
        receive_path(&rx, &source, "recreate before atomic save");
        drain(&rx);
        let temporary = watcher.path().join("source.tmp");
        fs::write(&temporary, "atomic replacement").unwrap();
        fs::rename(&temporary, &source).unwrap();
        receive_path(&rx, &source, "atomic replacement");
        drain(&rx);

        fs::remove_file(&source).unwrap();
        receive_path(&rx, &source, "delete");
        watcher.finish();
    }

    // ── skip-pattern coverage ───────────────────────────────────────

    #[test]
    fn default_config_skips_git_dir() {
        let cfg = WatchConfig::default();
        assert!(cfg.is_skipped(Path::new("/repo/.git/HEAD")));
        assert!(cfg.is_skipped(Path::new("/repo/.git/objects/ab/cdef")));
    }

    #[test]
    fn default_config_skips_target_dir() {
        let cfg = WatchConfig::default();
        assert!(cfg.is_skipped(Path::new("/repo/target/debug/foo.rlib")));
        assert!(cfg.is_skipped(Path::new("/repo/target/release/build/x.o")));
    }

    #[test]
    fn default_config_skips_node_modules() {
        let cfg = WatchConfig::default();
        assert!(cfg.is_skipped(Path::new("/repo/node_modules/@scope/package/index.js")));
    }

    #[test]
    fn default_config_skips_python_bytecode() {
        let cfg = WatchConfig::default();
        assert!(cfg.is_skipped(Path::new("/repo/pkg/__pycache__/m.cpython-312.pyc")));
        assert!(cfg.is_skipped(Path::new("/repo/lib.pyc")));
    }

    #[test]
    fn default_config_skips_editor_swap() {
        let cfg = WatchConfig::default();
        assert!(cfg.is_skipped(Path::new("/repo/src/main.rs.swp")));
        assert!(cfg.is_skipped(Path::new("/repo/draft.tmp")));
    }

    #[test]
    fn default_config_passes_source_files() {
        let cfg = WatchConfig::default();
        // Files with these patterns OUTSIDE the skip dirs should pass.
        assert!(!cfg.is_skipped(Path::new("/repo/src/main.rs")));
        assert!(!cfg.is_skipped(Path::new("/repo/lib.py")));
        assert!(!cfg.is_skipped(Path::new("/repo/index.ts")));
        // A literal `.gitignore` (not under `.git/`) should pass.
        assert!(!cfg.is_skipped(Path::new("/repo/.gitignore")));
    }

    #[test]
    fn unfiltered_config_skips_nothing() {
        let cfg = WatchConfig::unfiltered();
        assert!(!cfg.is_skipped(Path::new("/repo/.git/HEAD")));
        assert!(!cfg.is_skipped(Path::new("/repo/target/foo.rlib")));
        assert!(!cfg.is_skipped(Path::new("/repo/lib.pyc")));
    }

    #[test]
    fn custom_config_round_trip() {
        let cfg = WatchConfig {
            skip_substrings: vec!["/secret/".to_string()],
            skip_extensions: vec!["bak".to_string()],
        };
        assert!(cfg.is_skipped(Path::new("/repo/secret/key.txt")));
        assert!(cfg.is_skipped(Path::new("/repo/file.bak")));
        // Substrings from the default set are NOT in this config:
        assert!(!cfg.is_skipped(Path::new("/repo/.git/HEAD")));
        assert!(!cfg.is_skipped(Path::new("/repo/lib.pyc")));
    }

    #[test]
    fn default_skip_substrings_are_anchored() {
        let cfg = WatchConfig::default();
        // `/target/` (not `target/`) so a file literally named `target`
        // at the repo root doesn't false-match.
        assert!(!cfg.is_skipped(Path::new("/repo/target")));
        // But `/repo/target/...` does:
        assert!(cfg.is_skipped(Path::new("/repo/target/foo")));
    }

    // The debouncer fires the callback iff `retain_unskipped` returns a
    // non-empty batch. We test that retention decision directly rather
    // than against a live watcher: a real-FS "noise-only batch" test is
    // inherently flaky across platforms, because inotify (Linux) and
    // FSEvents (macOS) report different event paths for the same writes
    // (e.g. a write inside `target/` can surface a modify event on the
    // bare `target` directory entry on Linux but not on macOS). The
    // positive wiring is covered by `callback_fires_on_file_change`.

    #[test]
    fn noise_only_batch_retains_nothing() {
        let cfg = WatchConfig::default();
        // A pure `cargo build` / `git` storm — every path is noise.
        let batch = vec![
            PathBuf::from("/repo/target/debug/deps/a.rlib"),
            PathBuf::from("/repo/target/release/build/x.o"),
            PathBuf::from("/repo/.git/objects/ab/cdef"),
            PathBuf::from("/repo/pkg/__pycache__/m.cpython-312.pyc"),
            PathBuf::from("/repo/lib.pyc"),
        ];
        // Empty result → the debouncer returns without firing the callback.
        assert!(retain_unskipped(&cfg, batch).is_empty());
    }

    #[test]
    fn mixed_batch_retains_only_non_noise() {
        let cfg = WatchConfig::default();
        let batch = vec![
            PathBuf::from("/repo/target/debug/deps/a.rlib"), // noise
            PathBuf::from("/repo/src/main.rs"),              // source — keep
            PathBuf::from("/repo/.git/HEAD"),                // noise
            PathBuf::from("/repo/lib.py"),                   // source — keep
        ];
        let kept = retain_unskipped(&cfg, batch);
        assert_eq!(
            kept,
            vec![
                PathBuf::from("/repo/src/main.rs"),
                PathBuf::from("/repo/lib.py"),
            ]
        );
    }

    #[test]
    fn unfiltered_config_retains_everything() {
        let cfg = WatchConfig::unfiltered();
        let batch = vec![
            PathBuf::from("/repo/target/debug/a.rlib"),
            PathBuf::from("/repo/.git/HEAD"),
        ];
        // Nothing is dropped → the callback sees the raw batch.
        assert_eq!(retain_unskipped(&cfg, batch.clone()), batch);
    }
}
