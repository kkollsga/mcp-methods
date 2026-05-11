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
/// the debounce window. The callback runs on a background thread; keep
/// it non-blocking or push work onto a channel.
pub type ChangeHandler = Arc<dyn Fn(&[PathBuf]) + Send + Sync>;

/// Default debounce window — short enough to feel responsive, long
/// enough to coalesce noisy editor saves and IDE temp-file dance.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(500);

/// Active watcher handle. Drop to stop watching.
pub struct WatchHandle {
    _debouncer: Debouncer<notify_debouncer_mini::notify::RecommendedWatcher>,
}

/// Spawn a recursive debounced watcher on ``dir``.
///
/// Returns a handle whose `Drop` impl tears the watcher down. Errors
/// surface synchronously if the path is not a directory or the platform
/// watcher refuses to register.
pub fn watch(
    dir: &Path,
    on_change: Option<ChangeHandler>,
    debounce: Option<Duration>,
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
            let paths: Vec<PathBuf> = events.into_iter().map(|e| e.path).collect();
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
}
