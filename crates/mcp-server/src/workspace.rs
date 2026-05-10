//! Multi-repo workspace mode (`--workspace DIR`).
//!
//! The agent activates a GitHub repo via `repo_management('org/repo')`,
//! the binary clones it into the workspace, and the active repo
//! becomes the bound source root for `read_source` / `grep` /
//! `list_source`. Idle repos auto-sweep after `--stale-after-days`.
//!
//! Layout under the workspace dir:
//!   workspace/
//!     repos/<org>/<repo>/         — cloned source
//!     inventory.json              — per-repo access tracking
//!
//! mcp-methods on its own ships *clone-and-track* — no graph
//! building. Downstream binaries (kglite-mcp-server) layer their
//! build step on top by registering a [`PostActivateHook`] that
//! fires after each successful clone/update.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Repo name format: ``org/repo``. Letters, digits, dots, hyphens, underscores.
fn validate_repo_name(name: &str) -> Result<()> {
    let mut parts = name.split('/');
    let org = parts.next().unwrap_or("");
    let repo = parts.next().unwrap_or("");
    if parts.next().is_some() || org.is_empty() || repo.is_empty() {
        return Err(anyhow!(
            "Invalid repo name {name:?}. Expected 'org/repo' (exactly one slash)."
        ));
    }
    let valid = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    };
    if !valid(org) || !valid(repo) {
        return Err(anyhow!(
            "Invalid repo name {name:?}. Letters/digits/dots/hyphens/underscores only."
        ));
    }
    Ok(())
}

/// Hook fired after a successful clone or update. Receives the absolute
/// path to the cloned repo and the org/repo name. Errors are logged but
/// don't abort the activation — the repo is still registered as active.
pub type PostActivateHook = Arc<dyn Fn(&Path, &str) -> Result<()> + Send + Sync>;

/// Per-repo inventory entry persisted in `inventory.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct InventoryEntry {
    cloned_at: String,
    last_accessed: String,
    #[serde(default)]
    access_count: u64,
    #[serde(default)]
    stale: bool,
    /// HEAD SHA at the time the post-activate hook last completed
    /// successfully. Drives auto-rebuild gating: when an `update=True`
    /// call ends with `action=="current"` AND the new HEAD matches this,
    /// the post-activate hook can be skipped. `serde(default)` keeps
    /// older inventory.json files (without this field) loading cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_built_sha: Option<String>,
}

/// Workspace runtime state. Shared across MCP request clones via Arc.
#[derive(Clone)]
pub struct Workspace {
    inner: Arc<WorkspaceInner>,
}

struct WorkspaceInner {
    workspace_dir: PathBuf,
    stale_after_days: u32,
    state: RwLock<WorkspaceState>,
    post_activate: Option<PostActivateHook>,
}

#[derive(Debug, Default)]
struct WorkspaceState {
    active_repo_name: Option<String>,
    active_repo_path: Option<PathBuf>,
}

impl Workspace {
    pub fn open(
        workspace_dir: PathBuf,
        stale_after_days: u32,
        post_activate: Option<PostActivateHook>,
    ) -> Result<Self> {
        if !workspace_dir.is_dir() {
            fs::create_dir_all(&workspace_dir).with_context(|| {
                format!("failed to create workspace dir {}", workspace_dir.display())
            })?;
        }
        let repos_dir = workspace_dir.join("repos");
        if !repos_dir.is_dir() {
            fs::create_dir_all(&repos_dir)
                .with_context(|| format!("failed to create repos dir {}", repos_dir.display()))?;
        }
        let ws = Self {
            inner: Arc::new(WorkspaceInner {
                workspace_dir,
                stale_after_days,
                state: RwLock::new(WorkspaceState::default()),
                post_activate,
            }),
        };
        ws.reconcile_inventory()?;
        Ok(ws)
    }

    pub fn workspace_dir(&self) -> &Path {
        &self.inner.workspace_dir
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.inner.workspace_dir.join("repos")
    }

    fn inventory_path(&self) -> PathBuf {
        self.inner.workspace_dir.join("inventory.json")
    }

    /// Active repo's full org/repo name, or None if nothing is active.
    pub fn active_repo_name(&self) -> Option<String> {
        self.inner.state.read().unwrap().active_repo_name.clone()
    }

    /// Active repo's filesystem path, or None.
    pub fn active_repo_path(&self) -> Option<PathBuf> {
        self.inner.state.read().unwrap().active_repo_path.clone()
    }

    // ------------------------------------------------------------------
    // Inventory management
    // ------------------------------------------------------------------

    fn load_inventory(&self) -> BTreeMap<String, InventoryEntry> {
        let path = self.inventory_path();
        let Ok(text) = fs::read_to_string(&path) else {
            return BTreeMap::new();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    fn save_inventory(&self, inv: &BTreeMap<String, InventoryEntry>) -> Result<()> {
        let path = self.inventory_path();
        let body = serde_json::to_string_pretty(inv).context("failed to serialise inventory")?;
        fs::write(&path, body).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    fn reconcile_inventory(&self) -> Result<()> {
        let mut inv = self.load_inventory();
        let mut on_disk: Vec<String> = Vec::new();
        if self.repos_dir().is_dir() {
            for org_entry in fs::read_dir(self.repos_dir())? {
                let Ok(org_entry) = org_entry else { continue };
                if !org_entry.path().is_dir() {
                    continue;
                }
                let org = org_entry.file_name().to_string_lossy().into_owned();
                if org.starts_with('.') {
                    continue;
                }
                for repo_entry in fs::read_dir(org_entry.path())? {
                    let Ok(repo_entry) = repo_entry else { continue };
                    if !repo_entry.path().is_dir() {
                        continue;
                    }
                    let repo = repo_entry.file_name().to_string_lossy().into_owned();
                    if repo.starts_with('.') {
                        continue;
                    }
                    let rname = format!("{org}/{repo}");
                    on_disk.push(rname.clone());
                    inv.entry(rname).or_insert_with(|| {
                        let mtime = repo_entry
                            .metadata()
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .map(format_iso)
                            .unwrap_or_else(now_iso);
                        InventoryEntry {
                            cloned_at: mtime.clone(),
                            last_accessed: mtime,
                            access_count: 0,
                            stale: false,
                            last_built_sha: None,
                        }
                    });
                }
            }
        }
        for (rname, entry) in inv.iter_mut() {
            if !on_disk.contains(rname) && !entry.stale {
                entry.stale = true;
            }
        }
        self.save_inventory(&inv)?;
        Ok(())
    }

    fn bump_access(&self, name: &str, action: &str) {
        let mut inv = self.load_inventory();
        let now = now_iso();
        let entry = inv
            .entry(name.to_string())
            .or_insert_with(|| InventoryEntry {
                cloned_at: now.clone(),
                last_accessed: now.clone(),
                access_count: 0,
                stale: false,
                last_built_sha: None,
            });
        entry.last_accessed = now.clone();
        entry.access_count += 1;
        entry.stale = false;
        if action == "cloned" || entry.cloned_at.is_empty() {
            entry.cloned_at = now;
        }
        let _ = self.save_inventory(&inv);
    }

    fn mark_stale(&self, name: &str) {
        let mut inv = self.load_inventory();
        if let Some(entry) = inv.get_mut(name) {
            entry.stale = true;
            let _ = self.save_inventory(&inv);
        }
    }

    fn sweep_stale(&self) -> Vec<String> {
        let mut inv = self.load_inventory();
        let cutoff = SystemTime::now()
            - std::time::Duration::from_secs(self.inner.stale_after_days as u64 * 86_400);
        let active = self.active_repo_name();
        let mut swept: Vec<String> = Vec::new();
        for (rname, entry) in inv.iter_mut() {
            if entry.stale {
                continue;
            }
            if Some(rname.as_str()) == active.as_deref() {
                continue;
            }
            let last = parse_iso(&entry.last_accessed).unwrap_or(SystemTime::UNIX_EPOCH);
            if last >= cutoff {
                continue;
            }
            let parts: Vec<&str> = rname.splitn(2, '/').collect();
            if parts.len() != 2 {
                continue;
            }
            let repo_path = self.repos_dir().join(parts[0]).join(parts[1]);
            if repo_path.exists() {
                let _ = fs::remove_dir_all(&repo_path);
            }
            entry.stale = true;
            swept.push(rname.clone());
        }
        if !swept.is_empty() {
            let _ = self.save_inventory(&inv);
            self.prune_empty_org_dirs();
        }
        swept
    }

    fn prune_empty_org_dirs(&self) {
        let Ok(entries) = fs::read_dir(self.repos_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Ok(children) = fs::read_dir(&path) {
                let real: Vec<_> = children
                    .flatten()
                    .filter(|c| !c.file_name().to_string_lossy().starts_with('.'))
                    .collect();
                if real.is_empty() {
                    let _ = fs::remove_dir_all(&path);
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Git operations
    // ------------------------------------------------------------------

    /// Clone (if missing) or fast-forward (if cloned). Returns the
    /// action label, the repo path, and the new HEAD SHA after the op.
    fn clone_or_update(&self, name: &str) -> Result<(String, PathBuf, String)> {
        let parts: Vec<&str> = name.splitn(2, '/').collect();
        let repo_path = self.repos_dir().join(parts[0]).join(parts[1]);
        if !repo_path.exists() {
            fs::create_dir_all(repo_path.parent().unwrap()).ok();
            let url = format!("https://github.com/{name}.git");
            let out = Command::new("git")
                .args(["clone", "--depth", "1", &url, repo_path.to_str().unwrap()])
                .output()
                .context("failed to spawn `git clone`")?;
            if !out.status.success() {
                anyhow::bail!(
                    "git clone failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            let sha = git_rev_parse(&repo_path, "HEAD")?;
            return Ok(("cloned".to_string(), repo_path, sha));
        }

        // Fetch + check head delta
        Command::new("git")
            .args(["fetch", "--depth", "1", "origin"])
            .current_dir(&repo_path)
            .output()
            .context("git fetch failed")?;
        let local = git_rev_parse(&repo_path, "HEAD")?;
        let remote = git_rev_parse(&repo_path, "FETCH_HEAD")?;
        if local != remote {
            Command::new("git")
                .args(["reset", "--hard", "FETCH_HEAD"])
                .current_dir(&repo_path)
                .output()
                .context("git reset failed")?;
            let sha = git_rev_parse(&repo_path, "HEAD")?;
            return Ok(("updated".to_string(), repo_path, sha));
        }
        Ok(("current".to_string(), repo_path, local))
    }

    /// Activate a repo: clone if needed, fast-forward, fire post-activate hook.
    ///
    /// On successful hook completion the new HEAD SHA is persisted to
    /// `inventory.json[name].last_built_sha`. If the hook fails the SHA
    /// is NOT recorded, so the next `update=True` re-attempts the build.
    fn activate(&self, name: &str) -> Result<String> {
        let (action, repo_path, head_sha) = self.clone_or_update(name)?;
        self.bump_access(name, &action);
        {
            let mut state = self.inner.state.write().unwrap();
            state.active_repo_name = Some(name.to_string());
            state.active_repo_path = Some(repo_path.clone());
        }
        let hook_ok = if let Some(hook) = &self.inner.post_activate {
            match hook(&repo_path, name) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!("post-activate hook for {name} failed: {e}");
                    false
                }
            }
        } else {
            // No hook configured → nothing built, but nothing failed either.
            // Recording the SHA still helps the gating logic in #C1.
            true
        };
        if hook_ok {
            self.record_built_sha(name, &head_sha);
        }
        let verb = match action.as_str() {
            "cloned" => "Cloned",
            "updated" => "Updated",
            "current" => "Activated (already up to date)",
            other => other,
        };
        Ok(format!("{verb} '{name}' at {}.", repo_path.display()))
    }

    fn record_built_sha(&self, name: &str, sha: &str) {
        let mut inv = self.load_inventory();
        if let Some(entry) = inv.get_mut(name) {
            entry.last_built_sha = Some(sha.to_string());
            let _ = self.save_inventory(&inv);
        }
    }

    /// Read the SHA recorded after the last successful post-activate hook
    /// for the named repo. `None` if the repo was never built (or the
    /// hook last failed). Useful for downstream consumers gating
    /// "is the active graph up to date with the repo HEAD?" checks.
    pub fn last_built_sha(&self, name: &str) -> Option<String> {
        self.load_inventory()
            .get(name)
            .and_then(|e| e.last_built_sha.clone())
    }

    fn delete(&self, name: &str) -> Result<String> {
        let parts: Vec<&str> = name.splitn(2, '/').collect();
        if parts.len() != 2 {
            anyhow::bail!("Invalid repo name");
        }
        let repo_path = self.repos_dir().join(parts[0]).join(parts[1]);
        let mut deleted = Vec::new();
        if repo_path.exists() {
            fs::remove_dir_all(&repo_path).context("failed to remove repo dir")?;
            deleted.push("repo");
        }
        self.mark_stale(name);
        self.prune_empty_org_dirs();
        if deleted.is_empty() {
            return Ok(format!("Nothing to delete — '{name}' not found."));
        }
        let mut state = self.inner.state.write().unwrap();
        if state.active_repo_name.as_deref() == Some(name) {
            state.active_repo_name = None;
            state.active_repo_path = None;
            return Ok(format!(
                "Deleted {}. Active repo cleared.",
                deleted.join(", ")
            ));
        }
        Ok(format!("Deleted {}.", deleted.join(", ")))
    }

    fn list(&self) -> String {
        let inv = self.load_inventory();
        if inv.is_empty() {
            return "No repos cloned yet. Call repo_management('org/repo') to clone one."
                .to_string();
        }
        let active = self.active_repo_name();
        let mut live: Vec<String> = Vec::new();
        let mut stale_lines: Vec<String> = Vec::new();
        for (rname, entry) in &inv {
            let marker = if Some(rname.as_str()) == active.as_deref() {
                " [active]"
            } else {
                ""
            };
            let access = format!(
                "{} access{}, last {}",
                entry.access_count,
                if entry.access_count == 1 { "" } else { "es" },
                relative_time(&entry.last_accessed)
            );
            if entry.stale {
                stale_lines.push(format!(
                    "  {rname}  [STALE — re-fetch with repo_management('{rname}')]  ({access})"
                ));
            } else {
                live.push(format!("  {rname}{marker}  ({access})"));
            }
        }
        let mut out = String::new();
        if !live.is_empty() {
            out.push_str(&format!(
                "{} live repo(s):\n{}",
                live.len(),
                live.join("\n")
            ));
        }
        if !stale_lines.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&format!(
                "{} stale repo(s):\n{}",
                stale_lines.len(),
                stale_lines.join("\n")
            ));
        }
        out
    }

    /// Public entry for the `repo_management` MCP tool.
    pub fn repo_management(&self, name: Option<&str>, delete: bool, update: bool) -> String {
        let swept = self.sweep_stale();
        let prefix = if swept.is_empty() {
            String::new()
        } else {
            format!(
                "[Swept {} idle repo(s) (>{}d): {}]\n\n",
                swept.len(),
                self.inner.stale_after_days,
                swept.join(", ")
            )
        };

        if name.is_none() && !update {
            return prefix + &self.list();
        }

        if update {
            let Some(active) = self.active_repo_name() else {
                return prefix + "No active repository. Call repo_management('org/repo') first.";
            };
            return prefix
                + &self
                    .activate(&active)
                    .unwrap_or_else(|e| format!("update failed: {e}"));
        }

        let Some(name) = name else {
            return prefix + "Provide a repo name (e.g. repo_management('org/repo')).";
        };
        if let Err(e) = validate_repo_name(name) {
            return prefix + &e.to_string();
        }
        if delete {
            return prefix
                + &self
                    .delete(name)
                    .unwrap_or_else(|e| format!("delete failed: {e}"));
        }
        prefix
            + &self
                .activate(name)
                .unwrap_or_else(|e| format!("activate failed: {e}"))
    }
}

fn git_rev_parse(repo_path: &Path, refspec: &str) -> Result<String> {
    let out = Command::new("git")
        .args(["rev-parse", refspec])
        .current_dir(repo_path)
        .output()
        .context("git rev-parse failed")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn now_iso() -> String {
    format_iso(SystemTime::now())
}

fn format_iso(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Lightweight RFC3339-ish formatter. Drop sub-second precision; matches Python isoformat(timespec=seconds).
    let datetime = chrono_lite::format_secs(secs);
    datetime
}

fn parse_iso(s: &str) -> Option<SystemTime> {
    let secs = chrono_lite::parse_secs(s)?;
    SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(secs))
}

fn relative_time(iso: &str) -> String {
    let Some(t) = parse_iso(iso) else {
        return "unknown".to_string();
    };
    let now = SystemTime::now();
    let delta = now.duration_since(t).unwrap_or_default().as_secs();
    if delta < 3600 {
        "just now".to_string()
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86_400)
    }
}

/// Tiny self-contained ISO-8601 (seconds-precision) formatter so we
/// don't pull in `chrono` for a handful of timestamps.
mod chrono_lite {
    pub fn format_secs(secs: u64) -> String {
        // Civil-from-days algorithm (Howard Hinnant). Output: YYYY-MM-DDTHH:MM:SS.
        let days = (secs / 86_400) as i64;
        let time = secs % 86_400;
        let (y, mo, d) = days_to_civil(days + 719_468);
        let h = time / 3600;
        let m = (time / 60) % 60;
        let s = time % 60;
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}")
    }

    pub fn parse_secs(s: &str) -> Option<u64> {
        // Accept "YYYY-MM-DDTHH:MM:SS" (no zone) — same shape as format_secs output
        // and Python's datetime.isoformat(timespec="seconds").
        let bytes = s.as_bytes();
        if bytes.len() < 19 {
            return None;
        }
        let y: i64 = s.get(0..4)?.parse().ok()?;
        let mo: u32 = s.get(5..7)?.parse().ok()?;
        let d: u32 = s.get(8..10)?.parse().ok()?;
        let h: u64 = s.get(11..13)?.parse().ok()?;
        let m: u64 = s.get(14..16)?.parse().ok()?;
        let sc: u64 = s.get(17..19)?.parse().ok()?;
        let days = civil_to_days(y, mo, d) - 719_468;
        Some((days * 86_400) as u64 + h * 3600 + m * 60 + sc)
    }

    fn days_to_civil(z: i64) -> (i64, u32, u32) {
        let z = z;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = (yoe as i64) + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
        let y = if m <= 2 { y + 1 } else { y };
        (y, m, d)
    }

    fn civil_to_days(y: i64, m: u32, d: u32) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = (y - era * 400) as u64;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as u64 + 2) / 5 + d as u64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe as i64
    }
}

// silences unused-import-when-helper-only-via-json! macro check.
#[allow(dead_code)]
fn _json_keepalive() {
    let _ = json!({});
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_repo_names() {
        assert!(validate_repo_name("pydata/xarray").is_ok());
        assert!(validate_repo_name("my-org.x/repo_v2").is_ok());
        assert!(validate_repo_name("xarray").is_err());
        assert!(validate_repo_name("a/b/c").is_err());
        assert!(validate_repo_name("foo/bar; rm -rf").is_err());
    }

    #[test]
    fn open_creates_layout() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        assert!(ws.repos_dir().is_dir());
    }

    #[test]
    fn empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        let out = ws.repo_management(None, false, false);
        assert!(out.contains("No repos cloned yet"));
    }

    #[test]
    fn invalid_repo_name_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        let out = ws.repo_management(Some("bad name with spaces"), false, false);
        assert!(out.contains("Invalid repo name"));
    }

    #[test]
    fn delete_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        let out = ws.repo_management(Some("nope/none"), true, false);
        assert!(out.contains("Nothing to delete"));
    }

    #[test]
    fn iso_round_trip() {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let s = chrono_lite::format_secs(now);
        let back = chrono_lite::parse_secs(&s).unwrap();
        assert_eq!(now, back);
    }

    #[test]
    fn last_built_sha_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        // Seed an inventory entry directly (clone_or_update needs git).
        ws.bump_access("acme/widgets", "cloned");
        assert_eq!(ws.last_built_sha("acme/widgets"), None);
        ws.record_built_sha("acme/widgets", "abc1234deadbeef");
        assert_eq!(
            ws.last_built_sha("acme/widgets").as_deref(),
            Some("abc1234deadbeef")
        );
        // Survives an Workspace::open re-read (proves persistence).
        let ws2 = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        assert_eq!(
            ws2.last_built_sha("acme/widgets").as_deref(),
            Some("abc1234deadbeef")
        );
    }

    #[test]
    fn inventory_loads_legacy_entries_without_sha_field() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        // Hand-craft an old-style inventory.json without `last_built_sha`.
        let legacy = r#"{
            "old/repo": {
                "cloned_at": "2024-01-01T00:00:00",
                "last_accessed": "2024-01-01T00:00:00",
                "access_count": 5,
                "stale": false
            }
        }"#;
        std::fs::write(dir.path().join("inventory.json"), legacy).unwrap();
        // Re-open and confirm graceful read.
        let ws2 = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        assert_eq!(ws2.last_built_sha("old/repo"), None);
        let _ = ws;
    }

    #[test]
    fn update_with_no_active_repo() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        let out = ws.repo_management(None, false, true);
        assert!(out.contains("No active repository"));
    }
}
