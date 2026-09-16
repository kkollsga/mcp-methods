//! Lazy skill delivery over the real JSON-RPC boundary: the
//! `skill(name)` loader, the per-session unloaded-skill footer, and
//! the surfaces the footer must stay off.
//!
//! These live outside the unit tests because the thing under test is
//! session identity — which only exists once a client has completed
//! `initialize` against a served instance.
#![cfg(feature = "server")]
use mcp_methods::server::{serve_prompts, McpServer, ServerOptions};
use rmcp::ServiceExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

struct Client {
    read: BufReader<ReadHalf<DuplexStream>>,
    write: WriteHalf<DuplexStream>,
    id: usize,
}
impl Client {
    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let message = json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params});
        self.write
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.read.read_line(&mut line),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(n > 0, "server disconnected");
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame["id"] == self.id {
                return frame;
            }
        }
    }
    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name":name,"arguments":arguments}))
            .await
    }
}

async fn boot(server: McpServer, client_name: &str) -> Client {
    let (a, b) = tokio::io::duplex(1024 * 1024);
    tokio::spawn(async move {
        let service = server.serve(a).await.unwrap();
        let _ = service.waiting().await;
    });
    let (read, write) = tokio::io::split(b);
    let mut client = Client {
        read: BufReader::new(read),
        write,
        id: 0,
    };
    let init = client
        .request(
            "initialize",
            json!({"protocolVersion":"2024-11-05","capabilities":{},
                   "clientInfo":{"name":client_name,"version":"1"}}),
        )
        .await;
    assert!(init.get("error").is_none(), "{init}");
    client
        .write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    client
}

fn text(frame: &Value) -> String {
    frame["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content in {frame}"))
        .to_string()
}

fn is_error(frame: &Value) -> bool {
    frame["result"]["isError"].as_bool().unwrap_or(false)
}

const FOOTER_PREFIX: &str = "has not been loaded this session";

/// A registry with one lazy skill on `ping`, one eager skill on
/// `list_source`, and one `applies_when`-suppressed skill.
fn test_registry(dir: &std::path::Path) -> mcp_methods::server::ResolvedRegistry {
    use mcp_methods::server::SkillRegistry;
    let yaml = dir.join("test_mcp.yaml");
    std::fs::write(&yaml, "name: t\nskills: true\n").unwrap();
    let skills = dir.join("test_mcp.skills");
    std::fs::create_dir(&skills).unwrap();
    std::fs::write(
        skills.join("lazy_ping.md"),
        "---\nname: lazy_ping\ndescription: Lazy routing.\nreferences_tools: [ping]\n\
         ---\n\nLAZY-BODY-SENTINEL\n",
    )
    .unwrap();
    std::fs::write(
        skills.join("eager_list.md"),
        "---\nname: eager_list\ndescription: Eager routing.\ndelivery: eager\n\
         references_tools: [list_source]\n---\n\nEAGER-BODY-SENTINEL\n",
    )
    .unwrap();
    std::fs::write(
        skills.join("gated.md"),
        "---\nname: gated\ndescription: d.\nreferences_tools: [ping]\n\
         applies_when:\n  tool_registered: not_a_tool\n---\n\nGATED-BODY\n",
    )
    .unwrap();
    SkillRegistry::new()
        .auto_detect_project_layer(&yaml)
        .finalise()
        .unwrap()
}

fn skilled_server(dir: &std::path::Path, options: ServerOptions) -> McpServer {
    let registry = test_registry(dir);
    let mut server = McpServer::new(options);
    let active = serve_prompts(&registry, &mut server);
    let names: Vec<&str> = active.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["eager_list", "lazy_ping"], "{active:?}");
    server
}

#[derive(Default, Deserialize, Serialize, schemars::JsonSchema)]
struct BulkArgs {
    #[serde(default)]
    value: String,
}

#[tokio::test]
async fn skill_tool_serves_active_bodies_and_refuses_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let server = skilled_server(dir.path(), ServerOptions::default());
    let mut client = boot(server, "loader").await;

    let ok = client.call("skill", json!({"name":"lazy_ping"})).await;
    assert!(!is_error(&ok), "{ok}");
    assert!(text(&ok).contains("LAZY-BODY-SENTINEL"), "{ok}");

    // Unknown name: an error envelope the client can branch on, whose
    // text names the set the agent may actually ask for.
    let unknown = client.call("skill", json!({"name":"nope"})).await;
    assert!(is_error(&unknown), "{unknown}");
    let body = text(&unknown);
    assert!(
        body.contains("eager_list") && body.contains("lazy_ping"),
        "{body}"
    );

    // A skill suppressed by `applies_when` is not "active in this
    // session", so it reads exactly like an unknown name — the body
    // never leaks through the loader.
    let gated = client.call("skill", json!({"name":"gated"})).await;
    assert!(is_error(&gated), "{gated}");
    assert!(!text(&gated).contains("GATED-BODY"), "{gated}");
}

#[tokio::test]
async fn footer_nudges_once_per_session_and_stays_off_the_wrong_surfaces() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = skilled_server(dir.path(), ServerOptions::default());
    server.register_typed_tool("bulk", "Returns a large payload", |_: BulkArgs| {
        json!((0..400)
            .map(|i| json!({"id": i, "text": "x".repeat(200)}))
            .collect::<Vec<_>>())
        .to_string()
    });
    let other = server.clone();
    let mut client = boot(server, "first").await;
    let mut second = boot(other, "second").await;

    // First call of a tool carrying an unloaded lazy skill.
    let first = client.call("ping", json!({})).await;
    let body = text(&first);
    assert!(
        body.contains(
            "Skill \"lazy_ping\" applies to this tool and has not been loaded \
                       this session — call skill(\"lazy_ping\")."
        ),
        "{body}"
    );

    // An eager-only tool says nothing: its body is already in the
    // description, so there is nothing to fetch.
    let listed = client.call("list_source", json!({})).await;
    assert!(!text(&listed).contains(FOOTER_PREFIX), "{listed}");

    // Loading the skill silences it — and the loader's own result is
    // never the place to be told to call the loader.
    let loaded = client.call("skill", json!({"name":"lazy_ping"})).await;
    assert!(!text(&loaded).contains(FOOTER_PREFIX), "{loaded}");
    let after = client.call("ping", json!({})).await;
    assert!(!text(&after).contains(FOOTER_PREFIX), "{after}");

    // Mutation: drop the session key from `LoadedSkills` (a single
    // global set) — this assertion fails, because one client's load
    // would silence every other client.
    let elsewhere = second.call("ping", json!({})).await;
    assert!(text(&elsewhere).contains(FOOTER_PREFIX), "{elsewhere}");

    // `expand_response` returns before the router dispatch, so it can
    // carry no footer of its own.
    let bulk = client.call("bulk", json!({"value":"x"})).await;
    let preview: Value = serde_json::from_str(&text(&bulk)).unwrap();
    let result_id = preview["response_budget"]["result_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no result_id in {preview}"))
        .to_string();
    let expanded = client
        .call(
            "expand_response",
            json!({"result_id": result_id, "path": "/0/id"}),
        )
        .await;
    assert!(expanded.get("error").is_none(), "{expanded}");
    assert!(!text(&expanded).contains(FOOTER_PREFIX), "{expanded}");
}

#[tokio::test]
async fn footer_composes_with_a_downstream_result_postprocess_hook() {
    let dir = tempfile::tempdir().unwrap();
    // Mutation: have the framework footer replace the hook's output
    // instead of composing — HOOK-FOOTER disappears and this fails.
    let options = ServerOptions::default().with_result_postprocess(Arc::new(
        |_tool: &str, _args: &Value, body: &str, _ctx: &mcp_methods::server::ResultCtx| {
            assert!(
                body.contains(FOOTER_PREFIX),
                "the hook must see the composed body: {body}"
            );
            Some("HOOK-FOOTER".to_string())
        },
    ));
    let server = skilled_server(dir.path(), options);
    let mut client = boot(server, "composed").await;

    let frame = client.call("ping", json!({})).await;
    let body = text(&frame);
    let framework = body.find(FOOTER_PREFIX).unwrap_or_else(|| panic!("{body}"));
    let downstream = body.find("HOOK-FOOTER").unwrap_or_else(|| panic!("{body}"));
    assert!(
        framework < downstream,
        "the framework footer must lead the consumer's: {body}"
    );
}

// ─── Post-serve re-resolve (Phase 4) ──────────────────────────────

/// A registry rooted at `dir`, every skill lazy and pointing at `ping`.
fn registry_at(
    dir: &std::path::Path,
    skills: &[(&str, &str, &str)],
) -> mcp_methods::server::ResolvedRegistry {
    use mcp_methods::server::SkillRegistry;
    std::fs::create_dir_all(dir).unwrap();
    let yaml = dir.join("test_mcp.yaml");
    std::fs::write(&yaml, "name: t\nskills: true\n").unwrap();
    let skills_dir = dir.join("test_mcp.skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    for (name, description, body) in skills {
        std::fs::write(
            skills_dir.join(format!("{name}.md")),
            format!(
                "---\nname: {name}\ndescription: {description}\n\
                 references_tools: [ping]\n---\n\n{body}\n"
            ),
        )
        .unwrap();
    }
    SkillRegistry::new()
        .auto_detect_project_layer(&yaml)
        .finalise()
        .unwrap()
}

async fn tool_description(client: &mut Client, tool: &str) -> String {
    let listed = client.request("tools/list", json!({})).await;
    listed["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tools in {listed}"))
        .iter()
        .find(|t| t["name"] == tool)
        .unwrap_or_else(|| panic!("tool {tool} not listed in {listed}"))["description"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

async fn prompt_names(client: &mut Client) -> Vec<String> {
    let listed = client.request("prompts/list", json!({})).await;
    listed["result"]["prompts"]
        .as_array()
        .unwrap_or_else(|| panic!("no prompts in {listed}"))
        .iter()
        .map(|p| p["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn a_tool_handler_rebuilds_the_skill_layer_and_the_next_tools_list_sees_it() {
    // The intended use: kglite's `reload_graph` swaps the graph, then
    // rebuilds the registry the new graph carries. The handler is a
    // plain `Fn(T) -> Result<String, String>` with no `&self`, so it
    // captures a `SkillReloader` taken before `serve`.
    //
    // Mutation: have `reinject_skills` return `Ok` without running
    // phase B — `tools/list` keeps advertising `alpha` and every
    // assertion after the reload fails.
    let dir = tempfile::tempdir().unwrap();
    let first = registry_at(
        &dir.path().join("first"),
        &[("alpha", "Alpha routing.", "ALPHA-BODY")],
    );
    let second = registry_at(
        &dir.path().join("second"),
        &[("beta", "Beta routing.", "BETA-BODY")],
    );

    let mut server = McpServer::new(ServerOptions::default());
    serve_prompts(&first, &mut server);
    let reloader = server.skill_reloader();
    server.register_typed_tool_fallible(
        "reload",
        "Rebuild the skill layer from a different registry.",
        move |_: BulkArgs| {
            let active = reloader.reinject_skills(&second)?;
            Ok(format!(
                "reloaded: {}",
                active
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ))
        },
    );
    let mut client = boot(server, "rebuilder").await;

    let before = tool_description(&mut client, "ping").await;
    assert!(before.contains("skill(\"alpha\")"), "{before}");
    assert_eq!(prompt_names(&mut client).await, vec!["alpha".to_string()]);

    let reloaded = client.call("reload", json!({})).await;
    assert!(!is_error(&reloaded), "{reloaded}");
    assert!(text(&reloaded).contains("reloaded: beta"), "{reloaded}");

    let after = tool_description(&mut client, "ping").await;
    assert!(
        !after.contains("<!-- mcp-skill:alpha -->") && !after.contains("skill(\"alpha\")"),
        "the removed skill must be gone from the description: {after}"
    );
    assert!(after.contains("skill(\"beta\")"), "{after}");
    assert_eq!(prompt_names(&mut client).await, vec!["beta".to_string()]);

    // The loader serves the new set, and only the new set.
    let beta = client.call("skill", json!({"name":"beta"})).await;
    assert!(!is_error(&beta), "{beta}");
    assert!(text(&beta).contains("BETA-BODY"), "{beta}");
    let alpha = client.call("skill", json!({"name":"alpha"})).await;
    assert!(is_error(&alpha), "{alpha}");
    assert!(!text(&alpha).contains("ALPHA-BODY"), "{alpha}");
}

#[tokio::test]
async fn a_changed_body_re_nudges_the_session_and_an_unchanged_one_does_not() {
    // The loaded-set rule. Mutation A: forget every name on rebuild —
    // `stable` reappears in the footer and the last assertion fails.
    // Mutation B: never forget — `mutable` stays silent and the
    // assertion before it fails.
    let dir = tempfile::tempdir().unwrap();
    let before = registry_at(
        &dir.path().join("before"),
        &[
            ("stable", "Stable routing.", "STABLE-BODY"),
            ("mutable", "Mutable routing.", "MUTABLE-BODY-V1"),
        ],
    );
    let after = registry_at(
        &dir.path().join("after"),
        &[
            ("stable", "Stable routing.", "STABLE-BODY"),
            ("mutable", "Mutable routing.", "MUTABLE-BODY-V2"),
        ],
    );

    let mut server = McpServer::new(ServerOptions::default());
    serve_prompts(&before, &mut server);
    let handle = server.clone();
    let mut client = boot(server, "sessions").await;

    let first = text(&client.call("ping", json!({})).await);
    assert!(first.contains("\"stable\""), "{first}");
    assert!(first.contains("\"mutable\""), "{first}");

    for name in ["stable", "mutable"] {
        let loaded = client.call("skill", json!({"name": name})).await;
        assert!(!is_error(&loaded), "{loaded}");
    }
    let silent = text(&client.call("ping", json!({})).await);
    assert!(!silent.contains(FOOTER_PREFIX), "{silent}");

    let active = handle.reinject_skills(&after).unwrap();
    assert_eq!(active.len(), 2, "{active:?}");

    let nudged = text(&client.call("ping", json!({})).await);
    assert!(
        nudged.contains("Skill \"mutable\" applies to this tool and has not been loaded"),
        "a body that changed under an agent must be re-nudged: {nudged}"
    );
    assert!(
        !nudged.contains("\"stable\""),
        "a body that did not change must stay loaded: {nudged}"
    );

    // And the loader now serves the new body.
    let body = client.call("skill", json!({"name":"mutable"})).await;
    assert!(text(&body).contains("MUTABLE-BODY-V2"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_tool_call_completes_while_a_rebuild_lands() {
    // No lock is held across an awaited tool handler, so a rebuild can
    // land mid-call and the call still finishes.
    //
    // Mutation: hold the read guard across `tools.call(tcc).await` in
    // `call_tool` (bind `self.skill_state()` instead of taking the
    // `Arc` snapshot) — `reinject_skills` blocks on the writer, the
    // release below never happens and both timeouts here fire.
    let dir = tempfile::tempdir().unwrap();
    let first = registry_at(
        &dir.path().join("first"),
        &[("alpha", "Alpha routing.", "ALPHA-BODY")],
    );
    let second = registry_at(
        &dir.path().join("second"),
        &[("beta", "Beta routing.", "BETA-BODY")],
    );

    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Arc::new(std::sync::Mutex::new(release_rx));

    let mut server = McpServer::new(ServerOptions::default());
    serve_prompts(&first, &mut server);
    server.register_typed_tool("slow", "Blocks until released.", move |_: BulkArgs| {
        entered_tx.send(()).unwrap();
        release_rx.lock().unwrap().recv().unwrap();
        "SLOW-DONE".to_string()
    });
    let handle = server.clone();
    let mut client = boot(server, "concurrent").await;

    let call = tokio::spawn(async move {
        let frame = client.call("slow", json!({})).await;
        (client, frame)
    });
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the slow handler never started");

    let rebuilt = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            let active = handle.reinject_skills(&second);
            (handle, active)
        }),
    )
    .await
    .expect("reinject_skills blocked behind an in-flight tool call")
    .unwrap();
    let (handle, active) = rebuilt;
    assert_eq!(active.unwrap().len(), 1);

    release_tx.send(()).unwrap();
    let (mut client, frame) = tokio::time::timeout(std::time::Duration::from_secs(10), call)
        .await
        .expect("the in-flight call never completed across the rebuild")
        .unwrap();
    assert!(text(&frame).contains("SLOW-DONE"), "{frame}");

    // The rebuild that landed mid-call is what the next list serves.
    let desc = tool_description(&mut client, "ping").await;
    assert!(desc.contains("skill(\"beta\")"), "{desc}");
    drop(handle);
}

#[tokio::test]
async fn the_skill_loader_returns_a_near_limit_body_whole() {
    // A SKILL.md just under the 16 KB hard limit is legal at load, but
    // its result serializes past the 16,384-byte default response
    // budget. Without the framework loader's exemption it comes back as
    // a ~2 KB preview excerpt — and `skill()` has already marked the
    // skill loaded, so the footer goes quiet and the agent never learns
    // it is holding a fragment, while the eager tier injects the same
    // body whole.
    //
    // Mutation: drop the `exempt` branch in `call_tool` — the body is
    // replaced by a `response_budget` preview envelope and both the
    // sentinel and the byte-for-byte assertions fail.
    use mcp_methods::server::skills::HARD_SIZE_LIMIT_BYTES;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path()).unwrap();
    let yaml = dir.path().join("test_mcp.yaml");
    std::fs::write(&yaml, "name: t\nskills: true\n").unwrap();
    let skills_dir = dir.path().join("test_mcp.skills");
    std::fs::create_dir_all(&skills_dir).unwrap();

    let frontmatter =
        "---\nname: big\ndescription: Big routing.\nreferences_tools: [ping]\n---\n\n";
    // Short lines: each newline costs one extra byte once JSON-escaped,
    // so the serialized result clears the budget while the file stays
    // under the load-time limit.
    let mut body = String::from("BODY-HEAD-SENTINEL\n");
    while frontmatter.len() + body.len() + 80 < HARD_SIZE_LIMIT_BYTES {
        body.push_str(&"x".repeat(53));
        body.push('\n');
    }
    body.push_str("BODY-TAIL-SENTINEL");
    let file = format!("{frontmatter}{body}\n");
    assert!(
        file.len() <= HARD_SIZE_LIMIT_BYTES,
        "the fixture must be legal at load: {} bytes",
        file.len()
    );
    std::fs::write(skills_dir.join("big.md"), &file).unwrap();

    let registry = mcp_methods::server::SkillRegistry::new()
        .auto_detect_project_layer(&yaml)
        .finalise()
        .unwrap();
    let mut server = McpServer::new(ServerOptions::default());
    let active = serve_prompts(&registry, &mut server);
    assert_eq!(active.len(), 1, "the fixture must have loaded: {active:?}");
    let mut client = boot(server, "big-body").await;

    let frame = client.call("skill", json!({"name":"big"})).await;
    assert!(!is_error(&frame), "{frame}");
    let served = text(&frame);
    assert!(
        serde_json::to_vec(&frame["result"]).unwrap().len() > 16_384,
        "precondition: the result must exceed the default budget, else the \
         exemption is not what this test is measuring"
    );
    assert!(
        served.contains("BODY-HEAD-SENTINEL") && served.contains("BODY-TAIL-SENTINEL"),
        "the loader must return the whole body, not a preview excerpt: \
         {} bytes served",
        served.len()
    );
    assert_eq!(
        served,
        registry.get("big").expect("the fixture resolved").body,
        "the loader must serve the resolved body byte-for-byte"
    );
    assert!(
        frame["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|t| !t.contains("response_budget")),
        "{frame}"
    );

    // The exemption is the loader's alone: an ordinary tool returning
    // the same bytes is still budgeted.
    server_echoing_the_same_body_is_still_budgeted(&body).await;
}

/// A downstream tool returning the same oversized text must still get
/// the preview envelope — the exemption is bounded to the framework
/// loader, whose result is a size-capped skill body by construction.
async fn server_echoing_the_same_body_is_still_budgeted(body: &str) {
    let payload = body.to_string();
    let mut server = McpServer::new(ServerOptions::default());
    server.register_typed_tool("echo_big", "Returns the same bytes", move |_: BulkArgs| {
        payload.clone()
    });
    let mut client = boot(server, "budgeted").await;
    let frame = client.call("echo_big", json!({})).await;
    let served = text(&frame);
    assert!(
        served.len() < body.len(),
        "an ordinary tool must still be budgeted; got {} bytes",
        served.len()
    );
    assert!(served.contains("response_budget"), "{served}");
}
