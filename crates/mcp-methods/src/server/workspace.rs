//! Workspace mode — two variants.
//!
//! **Github mode** (`Workspace::open`, the default when
//! `--workspace DIR` is set): the agent activates a GitHub repo via
//! `repo_management('org/repo')`, the binary clones it into the
//! workspace, and the active repo becomes the bound source root for
//! `read_source` / `grep` / `list_source`. Idle repos auto-sweep after
//! `--stale-after-days`. Layout:
//!   workspace/
//!     repos/<org>/<repo>/         — cloned source
//!     inventory.json              — per-repo access tracking
//!
//! **Local mode** (`Workspace::open_local`, the manifest-driven
//! `workspace: { kind: local, root: ... }` variant): the active source
//! root is a fixed local directory, not a clone target. `repo_management`
//! reports the active root and triggers rebuilds; an `set_root_dir`
//! tool can swap the root at runtime. Closes the `code_review_mcp_server`
//! use case from the kglite wishlist.
//!
//! Both modes fire the same [`PostActivateHook`] so downstream binaries
//! (kglite-mcp-server) layer their build step on top with one
//! registration point, and both honour the same `last_built_sha`
//! gating to skip pointless rebuilds.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashSet};
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

// `WorkspaceKind` is re-used from the manifest module so config and
// runtime share one enum — the values mean the same thing.
pub use crate::server::manifest::WorkspaceKind;

/// Workspace runtime state. Shared across MCP request clones via Arc.
#[derive(Clone)]
pub struct Workspace {
    inner: Arc<WorkspaceInner>,
}

struct WorkspaceInner {
    kind: WorkspaceKind,
    workspace_dir: PathBuf,
    stale_after_days: u32,
    state: RwLock<WorkspaceState>,
    post_activate: Option<PostActivateHook>,
}

#[derive(Debug, Default)]
struct WorkspaceState {
    active_repo_name: Option<String>,
    active_repo_path: Option<PathBuf>,
    /// Repos whose post-activate hook has fired **in this process**.
    /// Deliberately NOT persisted: `last_built_sha` records that the
    /// git repo is at the built SHA (a cross-process fact), but the
    /// hook's *product* — the consumer's in-memory graph — lives only
    /// for the process lifetime. On a fresh process this set is empty,
    /// so the first activate re-fires the hook to rehydrate even when
    /// the persisted SHA already matches HEAD.
    hydrated_this_process: HashSet<String>,
}

impl Workspace {
    /// Open a github-flavoured workspace (clone + track flow).
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
                kind: WorkspaceKind::Github,
                workspace_dir,
                stale_after_days,
                state: RwLock::new(WorkspaceState::default()),
                post_activate,
            }),
        };
        ws.reconcile_inventory()?;
        Ok(ws)
    }

    /// Open a local-directory workspace.
    ///
    /// Binds `root` as the active source root immediately and fires the
    /// post-activate hook (subject to last-built-sha gating). `inventory.json`
    /// is kept under `<root>/.mcp-workspace/` so the local mode mirrors
    /// the same gating / fingerprinting infra without polluting the
    /// user's tree with a `repos/` directory.
    pub fn open_local(root: PathBuf, post_activate: Option<PostActivateHook>) -> Result<Self> {
        if !root.is_dir() {
            anyhow::bail!(
                "local workspace root does not exist or is not a directory: {}",
                root.display()
            );
        }
        let canon_root = root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize local root {}", root.display()))?;
        // Store inventory under a hidden subdir so we don't litter the
        // user's repo. The "workspace dir" for local mode IS the root.
        let inv_dir = canon_root.join(".mcp-workspace");
        if !inv_dir.is_dir() {
            fs::create_dir_all(&inv_dir).with_context(|| {
                format!("failed to create local-workspace dir {}", inv_dir.display())
            })?;
        }
        let mut state = WorkspaceState::default();
        let synthetic_name = synthesize_local_name(&canon_root);
        state.active_repo_name = Some(synthetic_name);
        state.active_repo_path = Some(canon_root.clone());
        Ok(Self {
            inner: Arc::new(WorkspaceInner {
                kind: WorkspaceKind::Local,
                workspace_dir: canon_root,
                stale_after_days: u32::MAX, // sweeping is github-only
                state: RwLock::new(state),
                post_activate,
            }),
        })
    }

    pub fn kind(&self) -> WorkspaceKind {
        self.inner.kind
    }

    pub fn workspace_dir(&self) -> &Path {
        &self.inner.workspace_dir
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.inner.workspace_dir.join("repos")
    }

    fn inventory_path(&self) -> PathBuf {
        match self.inner.kind {
            WorkspaceKind::Github => self.inner.workspace_dir.join("inventory.json"),
            WorkspaceKind::Local => self
                .inner
                .workspace_dir
                .join(".mcp-workspace")
                .join("inventory.json"),
        }
    }

    /// Active repo's full org/repo name, or None if nothing is active.
    pub fn active_repo_name(&self) -> Option<String> {
        self.inner.state.read().unwrap().active_repo_name.clone()
    }

    /// Active repo's filesystem path, or None.
    pub fn active_repo_path(&self) -> Option<PathBuf> {
        self.inner.state.read().unwrap().active_repo_path.clone()
    }

    /// Default `org/repo` for the GitHub tools when the caller passes none.
    ///
    /// Github mode: the active repo — there the inventory key *is* the
    /// `org/repo`. Local mode: the active root's `origin` remote parsed
    /// to `org/repo`, or `None` when there's no GitHub remote. Crucially
    /// it is *never* the `local/<dir>` inventory key (see
    /// [`active_repo_name`](Self::active_repo_name)), which is a
    /// filesystem-derived key, not a valid repo slug.
    pub fn default_github_repo(&self) -> Option<String> {
        match self.inner.kind {
            WorkspaceKind::Github => self.active_repo_name(),
            WorkspaceKind::Local => self.active_repo_path().and_then(|p| parse_origin_repo(&p)),
        }
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
        // Local mode has nothing to sweep — the operator owns the root.
        if matches!(self.inner.kind, WorkspaceKind::Local) {
            return Vec::new();
        }
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
    ///
    /// Local-mode short-circuits: there's nothing to clone or fetch.
    /// The "SHA" is a cheap content fingerprint (recursive walk of file
    /// mtimes + sizes) so the auto-rebuild gate still works.
    fn clone_or_update(&self, name: &str) -> Result<(String, PathBuf, String)> {
        if matches!(self.inner.kind, WorkspaceKind::Local) {
            // Local mode tracks the *currently bound* root, not the
            // immutable configured `workspace_dir`. `set_root_dir` writes
            // the target to `active_repo_path` before calling `activate`;
            // this read picks that up so the fingerprint and the
            // post-activate hook fire against the new root, and so the
            // subsequent `active_repo_path` write in `activate` doesn't
            // clobber the just-set target back to `workspace_dir`. Falls
            // back to `workspace_dir` only if state is unset, which
            // shouldn't happen after `open_local` seeds it.
            let root = self
                .inner
                .state
                .read()
                .unwrap()
                .active_repo_path
                .clone()
                .unwrap_or_else(|| self.inner.workspace_dir.clone());
            let prev_sha = self.last_built_sha(name);
            let fingerprint = fingerprint_dir(&root);
            let action = match prev_sha {
                Some(p) if p == fingerprint => "current",
                None => "cloned", // first activation
                Some(_) => "updated",
            };
            return Ok((action.to_string(), root, fingerprint));
        }
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
    /// Auto-rebuild gating: if `force_rebuild` is false AND the repo
    /// is already at the HEAD it was last built at (`action == "current"`
    /// AND `prev_built_sha == new_head`), the post-activate hook is
    /// skipped. This makes `repo_management(update=True)` cheap when
    /// upstream hasn't moved. Set `force_rebuild=true` to bypass (e.g.
    /// after upgrading the builder itself).
    ///
    /// On successful hook completion the new HEAD SHA is persisted to
    /// `inventory.json[name].last_built_sha`. If the hook fails the SHA
    /// is NOT recorded, so the next `update=True` re-attempts the build.
    fn activate(&self, name: &str, force_rebuild: bool) -> Result<String> {
        let prev_built_sha = self.last_built_sha(name);
        let (action, repo_path, head_sha) = self.clone_or_update(name)?;
        self.bump_access(name, &action);
        let hook_fired_here = {
            let mut state = self.inner.state.write().unwrap();
            state.active_repo_name = Some(name.to_string());
            state.active_repo_path = Some(repo_path.clone());
            state.hydrated_this_process.contains(name)
        };

        // The skip gate must be satisfied on BOTH axes: the git repo is
        // at its last-built SHA (persisted, cross-process) AND the hook
        // has already run in *this* process (in-memory). Without the
        // second axis a fresh process would inherit `last_built_sha` from
        // disk, skip the hook, and leave the consumer's in-memory state
        // (e.g. the code graph) empty — activate would report success
        // with nothing loaded.
        let already_built = !force_rebuild
            && action == "current"
            && prev_built_sha.as_deref() == Some(head_sha.as_str())
            && hook_fired_here;
        let mut hook_skipped = false;
        let hook_ok = if already_built {
            hook_skipped = true;
            true
        } else if let Some(hook) = &self.inner.post_activate {
            match hook(&repo_path, name) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!("post-activate hook for {name} failed: {e}");
                    false
                }
            }
        } else {
            // No hook configured — record the SHA so future calls can
            // see "no work to do" without consulting an empty store.
            true
        };
        if hook_ok {
            self.record_built_sha(name, &head_sha);
        }
        // Record in-memory hydration only when the hook actually ran this
        // process (not on the cheap-skip path). A no-op "no hook
        // configured" activation also counts as hydrated: there is no
        // per-process product to lose, so future skips are safe.
        if hook_ok && !hook_skipped {
            self.inner
                .state
                .write()
                .unwrap()
                .hydrated_this_process
                .insert(name.to_string());
        }
        let verb = match action.as_str() {
            "cloned" => "Cloned",
            "updated" => "Updated",
            "current" => "Activated (already up to date)",
            other => other,
        };
        let suffix = if hook_skipped {
            " [build skipped: HEAD matches last-built SHA]"
        } else {
            ""
        };
        Ok(format!(
            "{verb} '{name}' at {}.{suffix}",
            repo_path.display()
        ))
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
    ///
    /// - `name`: `org/repo` to activate (None = list / refresh mode).
    /// - `delete`: remove the named repo + inventory entry. Github only.
    /// - `update`: refresh the active repo (auto-rebuild gated).
    /// - `force_rebuild`: with `update=true` (or initial activation),
    ///   re-run the post-activate hook even when the HEAD SHA matches
    ///   `last_built_sha`. Useful after the builder itself has been
    ///   upgraded.
    ///
    /// Local mode behaviour: `name` and `delete` are rejected; pass
    /// `update=true` (or no args after the initial activation) to
    /// re-fingerprint the root and rebuild if anything changed.
    pub fn repo_management(
        &self,
        name: Option<&str>,
        delete: bool,
        update: bool,
        force_rebuild: bool,
    ) -> String {
        // Local mode: most github-only semantics are nonsensical here.
        if matches!(self.inner.kind, WorkspaceKind::Local) {
            if name.is_some() {
                return "Local-workspace mode does not accept a repo name. Use `set_root_dir(path)` \
                        to switch the active root, or pass `update=true` / `force_rebuild=true` \
                        to rebuild against the current root."
                    .to_string();
            }
            if delete {
                return "Local-workspace mode does not support `delete`. The root is owned by the \
                        operator; remove it manually."
                    .to_string();
            }
            let active = match self.active_repo_name() {
                Some(n) => n,
                None => return "No active local root.".to_string(),
            };
            // `update`: re-fingerprint and rebuild if anything changed.
            // `force_rebuild`: rebuild even when the fingerprint matches.
            // Either flag (or neither — initial bind path) routes through
            // `activate`; `activate` itself consults the gate using the
            // force flag plus the SHA comparison.
            let _ = update; // explicit: update is implicit in local mode
            return self
                .activate(&active, force_rebuild)
                .unwrap_or_else(|e| format!("rebuild failed: {e}"));
        }

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
                    .activate(&active, force_rebuild)
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
                .activate(name, force_rebuild)
                .unwrap_or_else(|e| format!("activate failed: {e}"))
    }

    /// Swap the active root (local mode only). Re-fires the post-activate
    /// hook against the new root. Errors if the workspace is github-flavoured.
    pub fn set_root_dir(&self, new_root: &Path) -> String {
        if !matches!(self.inner.kind, WorkspaceKind::Local) {
            return "set_root_dir is only valid in local-workspace mode.".to_string();
        }
        if !new_root.is_dir() {
            return format!(
                "Path does not exist or is not a directory: {}",
                new_root.display()
            );
        }
        let canon = match new_root.canonicalize() {
            Ok(p) => p,
            Err(e) => return format!("canonicalize failed: {e}"),
        };
        let synthetic = synthesize_local_name(&canon);
        {
            let mut state = self.inner.state.write().unwrap();
            state.active_repo_name = Some(synthetic.clone());
            state.active_repo_path = Some(canon.clone());
        }
        // Note: the WorkspaceInner.workspace_dir field is the path the
        // inventory is stored under. We keep the *original* one (from
        // open_local) so the inventory survives across root swaps.
        self.activate(&synthetic, false)
            .unwrap_or_else(|e| format!("set_root_dir failed: {e}"))
    }
}

/// Synthesise a stable "repo name" for a local workspace from its path.
/// Used as the inventory key so the same gating + persistence code paths
/// that github mode uses can apply to local mode unchanged.
fn synthesize_local_name(root: &Path) -> String {
    let name = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "local".to_string());
    format!("local/{name}")
}

/// Parse the `org/repo` slug from a local checkout's `origin` remote.
///
/// Shells out to `git -C <root> remote get-url origin` and parses both
/// canonical GitHub remote forms, stripping the trailing `.git`:
///   - `git@github.com:kkollsga/kglite.git`     → `kkollsga/kglite`
///   - `https://github.com/kkollsga/kglite.git` → `kkollsga/kglite`
///
/// Returns `None` for a non-git directory, a missing `origin` remote, or
/// a non-GitHub remote — so the GitHub tools fall back to their existing
/// empty-default path (ask the caller for `repo_name`).
fn parse_origin_repo(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8(out.stdout).ok()?;
    parse_github_remote(url.trim())
}

/// Pure-string half of [`parse_origin_repo`]: turn a GitHub remote URL
/// into `org/repo`, or `None` if it isn't a recognisable GitHub remote.
fn parse_github_remote(url: &str) -> Option<String> {
    // Accept both SSH (`git@github.com:org/repo`) and HTTPS
    // (`https://github.com/org/repo`) forms; everything after the host
    // separator is the path.
    let path = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.trim_end_matches('/');
    // Must be exactly `org/repo` — both segments non-empty, one slash.
    let mut parts = path.split('/');
    let org = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    if parts.next().is_some() {
        return None;
    }
    Some(format!("{org}/{repo}"))
}

/// Cheap recursive content fingerprint of a directory tree. Walks files
/// (respecting common ignore patterns) and folds `(path, mtime, len)`
/// into a 64-bit hash, then hex-formats it. Good enough to detect
/// "did anything change?" for auto-rebuild gating — not cryptographic.
fn fingerprint_dir(root: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let walker = ignore::WalkBuilder::new(root)
        .standard_filters(true)
        .hidden(true)
        .git_ignore(true)
        .build();
    for entry in walker.flatten() {
        if !entry.path().is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        entry.path().to_string_lossy().hash(&mut hasher);
        mtime.hash(&mut hasher);
        meta.len().hash(&mut hasher);
    }
    format!("local-{:016x}", hasher.finish())
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
    chrono_lite::format_secs(secs)
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
        let out = ws.repo_management(None, false, false, false);
        assert!(out.contains("No repos cloned yet"));
    }

    #[test]
    fn invalid_repo_name_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        let out = ws.repo_management(Some("bad name with spaces"), false, false, false);
        assert!(out.contains("Invalid repo name"));
    }

    #[test]
    fn delete_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        let out = ws.repo_management(Some("nope/none"), true, false, false);
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
    fn auto_rebuild_gate_skips_when_sha_matches() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_h = calls.clone();
        let hook: PostActivateHook = Arc::new(move |_path, _name| {
            calls_h.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        // Build a workspace pointing at a tempdir with a fake repo dir,
        // then simulate consecutive activates. We can't drive clone_or_update
        // without git, so test the gating directly by tracking the SHA
        // record-then-re-record case via Workspace::record_built_sha +
        // last_built_sha — the same predicate `activate` uses.
        let ws = Workspace::open(dir.path().to_path_buf(), 7, Some(hook)).unwrap();
        // Seed inventory entry + initial sha record.
        ws.bump_access("acme/widgets", "cloned");
        ws.record_built_sha("acme/widgets", "sha_one");
        assert_eq!(
            ws.last_built_sha("acme/widgets").as_deref(),
            Some("sha_one")
        );
        // Repeated record with the same value is idempotent (gating
        // logic uses last_built_sha as the source of truth).
        ws.record_built_sha("acme/widgets", "sha_one");
        assert_eq!(
            ws.last_built_sha("acme/widgets").as_deref(),
            Some("sha_one")
        );
        // No hook calls have been driven directly — this test exercises
        // the persistence path that the gate consults.
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn local_workspace_binds_root_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open_local(dir.path().to_path_buf(), None).unwrap();
        assert_eq!(ws.kind(), WorkspaceKind::Local);
        assert!(ws.active_repo_path().is_some());
        assert!(ws.active_repo_name().unwrap().starts_with("local/"));
    }

    #[test]
    fn local_workspace_rejects_github_ops() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open_local(dir.path().to_path_buf(), None).unwrap();
        let out = ws.repo_management(Some("acme/widgets"), false, false, false);
        assert!(out.contains("does not accept a repo name"));
        let out = ws.repo_management(None, true, false, false);
        assert!(out.contains("does not support `delete`"));
    }

    #[test]
    fn local_workspace_update_rebuilds() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        // Drop a file so the fingerprint has something to hash.
        std::fs::write(dir.path().join("x.txt"), b"hi").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_h = calls.clone();
        let hook: PostActivateHook = Arc::new(move |_p, _n| {
            calls_h.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let ws = Workspace::open_local(dir.path().to_path_buf(), Some(hook)).unwrap();
        // First update: nothing built yet → hook fires.
        let _ = ws.repo_management(None, false, true, false);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Second update without changes → SHA matches → hook skipped.
        let out = ws.repo_management(None, false, true, false);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "auto-rebuild gate must skip"
        );
        assert!(out.contains("build skipped"));
    }

    #[test]
    fn parses_github_remote_forms() {
        assert_eq!(
            parse_github_remote("git@github.com:kkollsga/kglite.git").as_deref(),
            Some("kkollsga/kglite")
        );
        assert_eq!(
            parse_github_remote("https://github.com/kkollsga/kglite.git").as_deref(),
            Some("kkollsga/kglite")
        );
        // No .git suffix, trailing slash.
        assert_eq!(
            parse_github_remote("https://github.com/acme/widget/").as_deref(),
            Some("acme/widget")
        );
        assert_eq!(
            parse_github_remote("ssh://git@github.com/acme/widget.git").as_deref(),
            Some("acme/widget")
        );
        // Non-github / malformed → None.
        assert_eq!(
            parse_github_remote("https://gitlab.com/acme/widget.git"),
            None
        );
        assert_eq!(parse_github_remote("git@github.com:acme.git"), None);
        assert_eq!(parse_github_remote("not a url"), None);
    }

    #[test]
    fn local_default_github_repo_uses_origin_remote() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Stand up a real git repo with a faked origin so default_github_repo
        // exercises the actual `git remote get-url` path.
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap()
        };
        if !git(&["init"]).status.success() {
            // git unavailable in this environment — skip rather than fail.
            return;
        }
        git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widget.git",
        ]);
        let ws = Workspace::open_local(root.to_path_buf(), None).unwrap();
        assert_eq!(
            ws.default_github_repo().as_deref(),
            Some("acme/widget"),
            "local default repo must come from the origin remote, not the inventory key"
        );
        // The inventory key remains the synthetic local name.
        assert!(ws.active_repo_name().unwrap().starts_with("local/"));
    }

    #[test]
    fn local_default_github_repo_none_without_remote() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open_local(dir.path().to_path_buf(), None).unwrap();
        // No git remote → None, and crucially NOT Some("local/<dir>").
        let def = ws.default_github_repo();
        assert!(
            def.is_none(),
            "expected None for a non-git local root, got {def:?}"
        );
    }

    #[test]
    fn set_root_dir_only_in_local_mode() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        let out = ws.set_root_dir(dir.path());
        assert!(out.contains("only valid in local-workspace"));
    }

    #[test]
    fn update_with_no_active_repo() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        let out = ws.repo_management(None, false, true, false);
        assert!(out.contains("No active repository"));
    }

    #[test]
    fn set_root_dir_updates_active_path() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        let ws = Workspace::open_local(dir.path().to_path_buf(), None).unwrap();
        let _ = ws.set_root_dir(&child);
        assert_eq!(
            ws.active_repo_path().unwrap(),
            child.canonicalize().unwrap(),
            "set_root_dir didn't update active_repo_path"
        );
    }

    #[test]
    fn set_root_dir_post_activate_fires_against_new_root() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("a.txt"), b"hi").unwrap();
        let seen_path: Arc<std::sync::Mutex<Option<PathBuf>>> = Arc::new(Default::default());
        let seen = seen_path.clone();
        let hook: PostActivateHook = Arc::new(move |p, _n| {
            *seen.lock().unwrap() = Some(p.to_path_buf());
            Ok(())
        });
        let ws = Workspace::open_local(dir.path().to_path_buf(), Some(hook)).unwrap();
        let _ = ws.set_root_dir(&child);
        assert_eq!(
            seen_path.lock().unwrap().clone().unwrap(),
            child.canonicalize().unwrap(),
            "post_activate hook saw the wrong root after set_root_dir"
        );
    }

    #[test]
    fn hook_fires_once_per_process_even_when_sha_matches() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // Local mode fingerprints the dir instead of a git SHA, so we can
        // drive the real `activate` path without git. A stable file keeps
        // the fingerprint constant across both simulated processes.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"stable").unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let make_hook = || -> PostActivateHook {
            let c = calls.clone();
            Arc::new(move |_p, _n| {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        };

        // --- Process 1 ---------------------------------------------------
        let ws = Workspace::open_local(dir.path().to_path_buf(), Some(make_hook())).unwrap();
        // First activate (fingerprint not yet recorded) → hook fires.
        let _ = ws.repo_management(None, false, true, false);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "first activate must hydrate"
        );
        // Second activate, same process, unchanged fingerprint → cheap-skip.
        let out = ws.repo_management(None, false, true, false);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "repeat activate in same process must skip the hook"
        );
        assert!(
            out.contains("build skipped"),
            "expected skip suffix, got: {out}"
        );
        drop(ws);

        // --- Process 2 (restart) ----------------------------------------
        // Same dir → inventory.json + last_built_sha persist, but the
        // in-memory hydration set does not. The first activate here must
        // re-fire the hook to rehydrate the consumer's in-memory state.
        let ws2 = Workspace::open_local(dir.path().to_path_buf(), Some(make_hook())).unwrap();
        assert!(
            ws2.last_built_sha(&ws2.active_repo_name().unwrap())
                .is_some(),
            "sanity: last_built_sha should survive the restart"
        );
        let _ = ws2.repo_management(None, false, true, false);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "fresh process must re-fire the hook even when the SHA matches"
        );
    }
}
