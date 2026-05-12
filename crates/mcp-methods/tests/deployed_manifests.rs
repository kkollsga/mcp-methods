//! Regression suite — load YAML shapes matching the 5 manifests this
//! framework's current production users (kglite + 4 derived servers)
//! deploy today. Each fixture is the *schema* of a real manifest at
//! `/Volumes/EksternalHome/Koding/MCP servers/`, with instructions /
//! overview_prefix trimmed to keep the fixtures readable.
//!
//! If any of these starts failing after a schema change, you have
//! broken a production manifest — add the missing key to
//! `ALLOWED_TOP_KEYS` (or fix the test if the breakage is intentional
//! and the deployment can be migrated).

#![cfg(feature = "server")]

use std::io::Write;

use mcp_methods::server::manifest::{load, TempCleanup, WorkspaceKind};

fn write_tmp(text: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(text.as_bytes()).unwrap();
    f
}

/// `kglite/kglite_codebase_mcp.yaml` — graph + embedder + multi-root.
#[test]
fn loads_kglite_codebase_shape() {
    let f = write_tmp(
        r#"
name: KGLite Codebase

source_roots:
  - /tmp/kglite/src
  - /tmp/kglite/python
  - /tmp/kglite/tests

trust:
  allow_embedder: true

embedder:
  module: ../embedder.py
  class: GraphEmbedder
  kwargs:
    cooldown: 900

builtins:
  temp_cleanup: on_overview

instructions: "trimmed"
overview_prefix: "trimmed"
"#,
    );
    let m = load(f.path()).unwrap();
    assert_eq!(m.name.as_deref(), Some("KGLite Codebase"));
    assert_eq!(m.source_roots.len(), 3);
    assert!(m.trust.allow_embedder);
    let e = m.embedder.unwrap();
    assert_eq!(e.class, "GraphEmbedder");
    assert_eq!(e.kwargs.get("cooldown").unwrap().as_i64(), Some(900));
    assert_eq!(m.builtins.temp_cleanup, TempCleanup::OnOverview);
}

/// `prospect_mcp/sodir_graph_mcp.yaml` — minimal manifest with builtins.
#[test]
fn loads_sodir_prospect_shape() {
    let f = write_tmp(
        r#"
name: Sodir Prospect

builtins:
  save_graph: true
  temp_cleanup: on_overview

instructions: "trimmed"
overview_prefix: "trimmed"
"#,
    );
    let m = load(f.path()).unwrap();
    assert!(m.builtins.save_graph);
    assert_eq!(m.builtins.temp_cleanup, TempCleanup::OnOverview);
}

/// `open_source/workspace_mcp.yaml` — minimal, intended for workspace mode.
#[test]
fn loads_open_source_workspace_shape() {
    let f = write_tmp(
        r#"
name: Open Source Explorer

instructions: "trimmed"
overview_prefix: "trimmed"
"#,
    );
    let m = load(f.path()).unwrap();
    assert_eq!(m.name.as_deref(), Some("Open Source Explorer"));
    // No `workspace:` block means the operator passes --workspace DIR;
    // the absence of the block is the regression to guard against.
    assert!(m.workspace.is_none());
}

/// `petrel_mcp/petrel_help_mcp.yaml` — embedder + single source_root.
#[test]
fn loads_petrel_help_shape() {
    let f = write_tmp(
        r#"
name: Petrel Help

source_root: source

trust:
  allow_embedder: true

embedder:
  module: ../embedder.py
  class: GraphEmbedder
  kwargs:
    cooldown: 900

builtins:
  temp_cleanup: on_overview

instructions: "trimmed"
overview_prefix: "trimmed"
"#,
    );
    let m = load(f.path()).unwrap();
    assert_eq!(m.source_roots, vec!["source".to_string()]);
    assert!(m.trust.allow_embedder);
}

/// `legal/norwegian_law_mcp.yaml` — same shape as petrel.
#[test]
fn loads_norwegian_law_shape() {
    let f = write_tmp(
        r#"
name: Norwegian Law

source_root: source

trust:
  allow_embedder: true

embedder:
  module: ../embedder.py
  class: GraphEmbedder
  kwargs:
    cooldown: 900

builtins:
  temp_cleanup: on_overview

instructions: "trimmed"
overview_prefix: "trimmed"
"#,
    );
    let m = load(f.path()).unwrap();
    assert_eq!(m.name.as_deref(), Some("Norwegian Law"));
    let e = m.embedder.unwrap();
    assert_eq!(e.kwargs.get("cooldown").unwrap().as_i64(), Some(900));
}

/// Forward-compatibility check: the new keys we added in phases A–C
/// (`env_file:`, `workspace:`) must parse cleanly alongside everything
/// the existing manifests already use.
#[test]
fn new_keys_compose_with_existing_shapes() {
    let f = write_tmp(
        r#"
name: Combined
source_root: source
env_file: ../.env
workspace:
  kind: local
  root: ./src
  watch: true
trust:
  allow_embedder: true
embedder:
  module: ../embedder.py
  class: GraphEmbedder
  kwargs:
    cooldown: 600
builtins:
  save_graph: true
  temp_cleanup: on_overview
"#,
    );
    let m = load(f.path()).unwrap();
    assert_eq!(m.env_file.as_deref(), Some("../.env"));
    let w = m.workspace.unwrap();
    assert_eq!(w.kind, WorkspaceKind::Local);
    assert!(w.watch);
    assert!(m.builtins.save_graph);
}

/// Sanity check: real deployed YAMLs use keys that the current schema
/// must continue to accept. Listing them here makes the dependency
/// explicit so a future schema PR has to either keep them or migrate
/// the deployments.
#[test]
fn allowed_top_keys_cover_deployed_manifests() {
    let f = write_tmp(
        r#"
name: x
instructions: x
overview_prefix: x
source_root: x
trust:
  allow_embedder: true
embedder:
  module: ./e.py
  class: GraphEmbedder
builtins:
  save_graph: false
  temp_cleanup: never
tools: []
"#,
    );
    load(f.path()).unwrap();
}
