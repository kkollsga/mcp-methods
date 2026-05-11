//! Embedder lifecycle handle.
//!
//! Wraps a manifest-loaded Python embedder instance and tracks its idle
//! time so the framework can call `unload()` after a configurable
//! cooldown. The protocol below is the contract a user-supplied
//! embedder class must satisfy.
//!
//! # Embedder protocol (duck-typed)
//!
//! A class declared via `embedder.class` in a manifest YAML must
//! expose the following attributes — all accessed by name under the
//! Python GIL:
//!
//! - `embed(texts: list[str]) -> list[list[float]]` — required.
//! - `dimension: int` — required; queried once at handle construction.
//! - `load(self) -> None` — optional; called before `embed` to bring
//!   the model online. The wrapper invokes it lazily on first `embed`,
//!   and again after every `unload()`.
//! - `unload(self) -> None` — optional; called when the idle timer
//!   fires. Implementations should drop large tensors / model weights.
//!   Python heap itself won't shrink back to the OS (Python interns
//!   freed memory), but the actual measurable win is the model
//!   tensors being released back to a CUDA allocator / mmap'd file.
//!
//! # Cooldown ownership
//!
//! The framework owns the idle timer. The embedder wrapper just tracks
//! `last_used: Instant`. A tokio task spawned by [`spawn_idle_watch`]
//! ticks periodically and calls [`EmbedderHandle::maybe_unload_after`].
//! This keeps embedder code declarative and lets `cooldown_secs` move
//! freely between deployments via YAML.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use pyo3::prelude::*;
use pyo3::types::PyList;

/// Wraps a user-supplied embedder class instance and tracks its idle time.
pub struct EmbedderHandle {
    instance: Py<PyAny>,
    last_used: Mutex<Instant>,
    loaded: Mutex<bool>,
}

impl EmbedderHandle {
    /// Wrap a freshly-instantiated embedder. The instance is expected to
    /// have been validated (e.g. `dimension: int` exists) at construction
    /// time by the caller — this constructor doesn't re-validate.
    pub fn new(instance: Py<PyAny>) -> Self {
        Self {
            instance,
            last_used: Mutex::new(Instant::now()),
            loaded: Mutex::new(false),
        }
    }

    /// Embed a batch of strings. Returns the embeddings as `Vec<Vec<f32>>`.
    /// Calls `load()` first if the embedder is not currently loaded;
    /// also bumps `last_used` so the idle timer resets.
    pub fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.ensure_loaded()?;
        let out = Python::attach(|py| -> PyResult<Vec<Vec<f32>>> {
            let py_texts = PyList::new(py, texts)?;
            let result = self.instance.call_method1(py, "embed", (py_texts,))?;
            let outer: Vec<Vec<f32>> = result.extract(py)?;
            Ok(outer)
        })
        .map_err(|e| anyhow!("embedder.embed failed: {e}"))?;
        *self.last_used.lock().unwrap() = Instant::now();
        Ok(out)
    }

    /// Bump the last-used timestamp without calling embed. Useful when a
    /// downstream consumer accessed the model directly (e.g. via the
    /// underlying `Py<PyAny>` instance) and wants to defer eviction.
    pub fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    /// Borrow the underlying Python instance. Calls bypass the
    /// `last_used` tracking — use [`touch`](Self::touch) explicitly.
    pub fn instance(&self) -> &Py<PyAny> {
        &self.instance
    }

    /// If the embedder has been idle longer than `cooldown` AND is
    /// currently loaded, invoke `unload()` on it. Returns `true` if the
    /// unload was performed.
    pub fn maybe_unload_after(&self, cooldown: Duration) -> bool {
        if !*self.loaded.lock().unwrap() {
            return false;
        }
        let idle = self.last_used.lock().unwrap().elapsed();
        if idle < cooldown {
            return false;
        }
        let res = Python::attach(|py| -> PyResult<()> {
            // unload is optional — silently ignore if missing.
            if self.instance.bind(py).hasattr("unload").unwrap_or(false) {
                self.instance.call_method0(py, "unload")?;
            }
            Ok(())
        });
        if let Err(e) = res {
            tracing::warn!("embedder.unload() failed: {e}");
            return false;
        }
        *self.loaded.lock().unwrap() = false;
        tracing::info!("embedder unloaded after {}s idle", idle.as_secs());
        true
    }

    /// Call `unload()` unconditionally (e.g. on graceful shutdown).
    pub fn unload_now(&self) {
        if !*self.loaded.lock().unwrap() {
            return;
        }
        let _ = Python::attach(|py| -> PyResult<()> {
            if self.instance.bind(py).hasattr("unload").unwrap_or(false) {
                self.instance.call_method0(py, "unload")?;
            }
            Ok(())
        });
        *self.loaded.lock().unwrap() = false;
    }

    fn ensure_loaded(&self) -> Result<()> {
        let mut guard = self.loaded.lock().unwrap();
        if *guard {
            return Ok(());
        }
        Python::attach(|py| -> PyResult<()> {
            if self.instance.bind(py).hasattr("load").unwrap_or(false) {
                self.instance.call_method0(py, "load")?;
            }
            Ok(())
        })
        .context("embedder.load() failed")?;
        *guard = true;
        Ok(())
    }
}

/// Spawn a tokio background task that periodically calls
/// [`EmbedderHandle::maybe_unload_after`] on the supplied handle.
///
/// Returns an `AbortHandle` you can drop to stop the watcher. The task
/// ticks at `cooldown / 4` (clamped to 30s..=300s) — small enough to
/// react quickly, big enough not to thrash.
pub fn spawn_idle_watch(
    handle: Arc<EmbedderHandle>,
    cooldown: Duration,
) -> tokio::task::AbortHandle {
    let tick = cooldown
        .checked_div(4)
        .unwrap_or(Duration::from_secs(60))
        .clamp(Duration::from_secs(30), Duration::from_secs(300));
    let task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // skip the immediate tick
        loop {
            ticker.tick().await;
            handle.maybe_unload_after(cooldown);
        }
    });
    task.abort_handle()
}

/// Extract a cooldown from an embedder config's `kwargs`. Looks for an
/// integer or float `cooldown` key (seconds). Returns `None` if absent
/// or non-numeric, meaning "no idle eviction".
pub fn extract_cooldown(kwargs: &serde_json::Map<String, serde_json::Value>) -> Option<Duration> {
    let v = kwargs.get("cooldown")?;
    let secs = v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64))?;
    if secs == 0 {
        return None;
    }
    Some(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_int_cooldown() {
        let mut k = serde_json::Map::new();
        k.insert("cooldown".to_string(), serde_json::json!(900));
        assert_eq!(extract_cooldown(&k), Some(Duration::from_secs(900)));
    }

    #[test]
    fn missing_cooldown_returns_none() {
        let k = serde_json::Map::new();
        assert_eq!(extract_cooldown(&k), None);
    }

    #[test]
    fn zero_cooldown_returns_none() {
        let mut k = serde_json::Map::new();
        k.insert("cooldown".to_string(), serde_json::json!(0));
        assert_eq!(extract_cooldown(&k), None);
    }
}
