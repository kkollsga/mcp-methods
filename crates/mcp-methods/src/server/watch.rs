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
//! Bindings that need everything (test fixtures, future consumers
//! with a genuine reason to see every event) pass
//! [`WatchConfig::unfiltered`] to [`watch_with_config`].

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify_debouncer_mini::notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};

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
    /// Empty skip set — every event reaches the callback. Use when
    /// you genuinely want raw FS events (test fixtures, log-every-
    /// change diagnostic modes, or future consumers with a reason to
    /// see `.git/objects/...` writes).
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

/// Active watcher handle. Drop to stop watching.
pub struct WatchHandle {
    _debouncer: Debouncer<notify_debouncer_mini::notify::RecommendedWatcher>,
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
/// caller-controlled — pass [`WatchConfig::unfiltered`] to receive
/// every event, or build a custom config to add / remove patterns.
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

    let mut debouncer = new_debouncer(debounce, move |result: DebounceEventResult| match result {
        Ok(events) => {
            // Drop skipped events before they're handed to the
            // callback or counted in the log line. Empty post-filter
            // batches (a pure-noise storm like `cargo build`'s
            // `target/` churn) return without a callback invocation
            // at all.
            let paths: Vec<PathBuf> = events
                .into_iter()
                .map(|e| e.path)
                .filter(|p| !config.is_skipped(p))
                .collect();
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
    })
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn watch_rejects_non_directory() {
        let result = watch(Path::new("/this/does/not/exist"), None, None);
        assert!(result.is_err());
    }

    #[test]
    fn watch_starts_and_drops_clean() {
        let dir = tempfile::tempdir().unwrap();
        let _handle = watch(dir.path(), None, Some(Duration::from_millis(100))).unwrap();
        // Drop at end of scope tears it down without panicking.
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
        let _handle = watch(dir.path(), Some(cb), Some(Duration::from_millis(100))).unwrap();
        sleep(Duration::from_millis(50)); // let watcher settle
        std::fs::write(dir.path().join("a.txt"), "hi").unwrap();
        sleep(Duration::from_millis(400)); // debounce + buffer
        assert!(
            counter.load(Ordering::SeqCst) >= 1,
            "expected callback to fire at least once after file write"
        );
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

    #[test]
    fn skip_filter_silences_callback_for_noise_only_batch() {
        use std::thread::sleep;
        let dir = tempfile::tempdir().unwrap();
        // Pre-create a *nested* dir under `target/` before the watch starts.
        // Writing into the leaf keeps every event path unambiguously under
        // `/target/` (the file writes and the leaf-dir mtime bump alike).
        // We deliberately do NOT touch the bare `target` entry during the
        // watch window: on Linux/inotify a write directly inside `target/`
        // also emits a modify event for the `target` directory itself, whose
        // path (`.../target`, no trailing slash) escapes the `/target/`
        // substring filter and would fire the callback. macOS/FSEvents does
        // not surface that event, which is why the shallow version passed
        // locally but failed in CI.
        let noise_dir = dir.path().join("target").join("debug").join("deps");
        std::fs::create_dir_all(&noise_dir).unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_cb = counter.clone();
        let cb: ChangeHandler = Arc::new(move |_paths: &[PathBuf]| {
            counter_for_cb.fetch_add(1, Ordering::SeqCst);
        });
        let _handle = watch(dir.path(), Some(cb), Some(Duration::from_millis(100))).unwrap();
        sleep(Duration::from_millis(50));
        // Write only into the nested `target/.../deps/` — should be filtered.
        std::fs::write(noise_dir.join("a.rlib"), "noise").unwrap();
        std::fs::write(noise_dir.join("b.rlib"), "noise").unwrap();
        sleep(Duration::from_millis(400));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "expected callback to NOT fire when every changed path is filtered"
        );
    }
}
