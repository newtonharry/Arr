//! The MCP adapter as an MCP client sees it. The official rmcp client drives the real
//! `arr mcp` binary over its stdin/stdout against a real API instance, or against
//! a recording stand-in where the API's own behaviour is not the subject. Every child also
//! gets HTTP(S)/ALL proxy variables pointing at a sentinel, so any request not sent to the
//! configured API would be seen.
#![recursion_limit = "256"]

mod common;

use arr::fsutil;
use common::*;
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig, ProtocolVersion};
use rmcp::service::RunningService;
use rmcp::{ClientLifecycleMode, ClientServiceExt, RoleClient, ServiceExt};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

const BIN: &str = env!("CARGO_BIN_EXE_arr");
const TOOLS: [&str; 8] = [
    "search_titles",
    "add_media",
    "stop",
    "media_status",
    "requests",
    "library",
    "release_choices",
    "start_download",
];

/// LEGACY stack: the tests of the Sonarr/Radarr native-handoff gate need an imported snapshot.
fn coexistence() -> ExecOptions {
    ExecOptions {
        mode: "coexistence",
        categories: Some(("um-movies", "um-tv")),
        ..ExecOptions::legacy()
    }
}

/// The managed stack with the dedicated download categories the MCP tests expect.
fn managed() -> ExecOptions {
    ExecOptions {
        categories: Some(("um-movies", "um-tv")),
        ..Default::default()
    }
}

fn category_of(e: &Exec, hash: &str) -> String {
    match e.stack {
        Stack::Managed => e.fake.lock().unwrap().torrents[hash].category.clone(),
        Stack::Legacy => e.qb.lock().unwrap().torrents[hash].category.clone(),
    }
}

fn adds(e: &Exec) -> usize {
    match e.stack {
        Stack::Managed => e.fake.lock().unwrap().add_fields.len(),
        Stack::Legacy => e.qb.lock().unwrap().add_fields.len(),
    }
}

/// Counts every connection made to it: the proxy every child is pointed at.
async fn sentinel() -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            h.fetch_add(1, Ordering::SeqCst);
            drop(sock);
        }
    });
    (format!("http://{addr}"), hits)
}

/// Copies everything the adapter writes to stdout while the SDK client reads it.
struct Tee<R> {
    inner: R,
    copy: Arc<Mutex<Vec<u8>>>,
}

impl<R: AsyncRead + Unpin> AsyncRead for Tee<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &r {
            let fresh = buf.filled()[before..].to_vec();
            self.copy.lock().unwrap().extend_from_slice(&fresh);
        }
        r
    }
}

enum Lifecycle {
    /// `server/discover` with the SDK's newest revision (no `initialize`).
    Discover,
    /// The `initialize` handshake, requesting this version.
    Initialize(ProtocolVersion),
}

struct Launch<'a> {
    api: &'a str,
    token_file: &'a Path,
    lifecycle: Lifecycle,
    timeout_secs: u64,
    rust_log: Option<&'a str>,
}

impl<'a> Launch<'a> {
    fn new(api: &'a str, token_file: &'a Path) -> Self {
        Launch {
            api,
            token_file,
            lifecycle: Lifecycle::Discover,
            timeout_secs: 10,
            rust_log: None,
        }
    }
}

struct Session {
    client: RunningService<RoleClient, ClientConfig>,
    child: tokio::process::Child,
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: tokio::task::JoinHandle<Vec<u8>>,
    proxy_hits: Arc<AtomicUsize>,
}

fn command(proxy: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(BIN);
    for k in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        cmd.env(k, proxy);
    }
    cmd.env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .env_remove("RUST_LOG");
    cmd
}

async fn launch(o: Launch<'_>) -> Session {
    let (proxy, proxy_hits) = sentinel().await;
    let mut cmd = command(&proxy);
    cmd.arg("mcp")
        .args(["--api-url", o.api, "--token-file"])
        .arg(o.token_file)
        .args(["--timeout-secs", &o.timeout_secs.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(l) = o.rust_log {
        cmd.env("RUST_LOG", l);
    }
    let mut child = cmd.spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    let out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let stderr = tokio::spawn(async move {
        let mut b = vec![];
        let _ = err.read_to_end(&mut b).await;
        b
    });
    let stdout = Arc::new(Mutex::new(vec![]));
    let transport = (
        Tee {
            inner: out,
            copy: stdout.clone(),
        },
        stdin,
    );
    let mut info = ClientConfig::default();
    let client = match o.lifecycle {
        Lifecycle::Discover => info
            .serve_with_lifecycle(
                transport,
                ClientLifecycleMode::Discover {
                    preferred_versions: vec![ProtocolVersion::LATEST],
                },
            )
            .await
            .unwrap(),
        Lifecycle::Initialize(v) => {
            info.protocol_version = v;
            info.serve(transport).await.unwrap()
        }
    };
    Session {
        client,
        child,
        stdout,
        stderr,
        proxy_hits,
    }
}

impl Session {
    async fn call(&self, tool: &str, args: Value) -> CallToolResult {
        let params = CallToolRequestParams::new(tool.to_string())
            .with_arguments(args.as_object().unwrap().clone());
        self.client.call_tool(params).await.unwrap()
    }

    /// Ends the session the way a client does (closing stdin) and returns the raw stdout and
    /// stderr. The adapter must exit cleanly and must never have used a proxy.
    async fn finish(mut self) -> (String, String) {
        let _ = self.client.cancel().await;
        let status = tokio::time::timeout(Duration::from_secs(10), self.child.wait())
            .await
            .expect("the adapter exits when its client goes away")
            .unwrap();
        assert!(status.success(), "{status:?}");
        let stderr = String::from_utf8_lossy(&self.stderr.await.unwrap()).into_owned();
        let stdout = String::from_utf8_lossy(&self.stdout.lock().unwrap()).into_owned();
        assert_eq!(self.proxy_hits.load(Ordering::SeqCst), 0, "proxy used");
        for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(line)
                .unwrap_or_else(|_| panic!("stdout carries only MCP messages: {line:.200}"));
            assert_eq!(v["jsonrpc"], "2.0", "{line:.200}");
        }
        (stdout, stderr)
    }
}

fn ok(r: &CallToolResult) -> &Value {
    let v = r.structured_content.as_ref().expect("structured result");
    assert_eq!(r.is_error, Some(false), "{v}");
    v
}

fn failed(r: &CallToolResult) -> (&str, &Value) {
    let v = r.structured_content.as_ref().expect("structured error");
    assert_eq!(r.is_error, Some(true), "{v}");
    (v["error"]["code"].as_str().expect("typed error"), v)
}

fn token_file(dir: &Path) -> PathBuf {
    let p = dir.join("mcp-token");
    fsutil::write_new_private_file(&p, format!("{TOKEN}\n").as_bytes()).unwrap();
    p
}

/// Every property name anywhere in a schema.
fn property_names(schema: &Value, out: &mut BTreeSet<String>) {
    match schema {
        Value::Object(m) => {
            if let Some(Value::Object(props)) = m.get("properties") {
                out.extend(props.keys().cloned());
            }
            m.values().for_each(|v| property_names(v, out));
        }
        Value::Array(a) => a.iter().for_each(|v| property_names(v, out)),
        _ => {}
    }
}

#[tokio::test]
async fn tools_are_listed_with_strict_schemas_under_every_negotiated_protocol() {
    let e = start_exec(coexistence()).await;
    let token = e.dir.join("secrets/api_token");
    // The SDK negotiates; the expected versions are the SDK's own constants.
    let cases = [
        (Lifecycle::Discover, ProtocolVersion::LATEST),
        (
            Lifecycle::Initialize(ProtocolVersion::LATEST),
            ProtocolVersion::LATEST_WITH_INITIALIZE,
        ),
        (
            Lifecycle::Initialize(ProtocolVersion::V_2025_06_18),
            ProtocolVersion::V_2025_06_18,
        ),
        (
            Lifecycle::Initialize(ProtocolVersion::V_2024_11_05),
            ProtocolVersion::V_2024_11_05,
        ),
    ];
    assert_eq!(ProtocolVersion::LATEST.as_str(), "2026-07-28");
    let mut listings = vec![];
    for (lifecycle, expected) in cases {
        let s = launch(Launch {
            lifecycle,
            ..Launch::new(&e.base, &token)
        })
        .await;
        let info = s.client.peer_info().expect("negotiated");
        assert_eq!(info.protocol_version, expected);
        assert_eq!(
            info.server_info.as_ref().map(|i| i.name.as_str()),
            Some("arr")
        );
        assert!(info.capabilities.tools.is_some());
        let instructions = info.instructions.as_deref().unwrap_or_default();
        assert!(
            instructions.contains("default, automatic path"),
            "{instructions}"
        );
        assert!(instructions.contains("manual mode"), "{instructions}");
        let tools = s.client.list_all_tools().await.unwrap();
        // A call works under the negotiated lifecycle too.
        ok(&s.call("requests", json!({})).await);
        listings.push(serde_json::to_value(&tools).unwrap());
        s.finish().await;
    }
    assert!(listings.windows(2).all(|w| w[0] == w[1]));

    let tools = listings[0].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, TOOLS);
    let expected: [(&str, &[&str], &[&str], bool); 8] = [
        (
            "search_titles",
            &["kind", "q", "year"],
            &["kind", "q"],
            true,
        ),
        ("add_media", &["media_id"], &["media_id"], false),
        ("stop", &["media_id"], &["media_id"], false),
        ("media_status", &["media_id"], &["media_id"], true),
        ("requests", &["limit"], &[], true),
        (
            "library",
            &["kind", "limit", "offset", "q"],
            &["kind"],
            true,
        ),
        ("release_choices", &["media_id"], &["media_id"], true),
        (
            "start_download",
            &["media_id", "release_ref"],
            &["media_id", "release_ref"],
            false,
        ),
    ];
    let mut all = BTreeSet::new();
    for (t, (name, props, required, read_only)) in tools.iter().zip(expected) {
        assert_eq!(t["name"], name);
        let s = &t["inputSchema"];
        assert_eq!(s["type"], "object", "{name}");
        assert_eq!(s["additionalProperties"], false, "{name}: {s}");
        let keys: Vec<&str> = s["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, props, "{name}");
        let mut req: Vec<&str> = s["required"]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_str().unwrap()).collect())
            .unwrap_or_default();
        req.sort();
        assert_eq!(req, required, "{name}");
        assert_eq!(t["annotations"]["readOnlyHint"], read_only, "{name}");
        assert_eq!(t["annotations"]["destructiveHint"], false, "{name}");
        property_names(s, &mut all);
    }
    assert_eq!(
        all.into_iter().collect::<Vec<_>>(),
        [
            "kind",
            "limit",
            "media_id",
            "offset",
            "q",
            "release_ref",
            "year"
        ],
        "no handoff, adoption, monitoring, policy, path, URL, deletion or torrent arguments"
    );
    // The published patterns accept exactly the canonical ids each tool takes.
    let pattern = |tool: &str| {
        let t = tools.iter().find(|t| t["name"] == tool).unwrap();
        regex::Regex::new(
            t["inputSchema"]["properties"]["media_id"]["pattern"]
                .as_str()
                .unwrap(),
        )
        .unwrap()
    };
    for (tool, id, accepted) in [
        ("add_media", "movie:tmdb:603", true),
        ("add_media", "season:tvdb:372264:1", true),
        ("add_media", "episode:tvmaze:9001:1:2", true),
        ("add_media", "series:tvdb:372264", false),
        ("add_media", "season:tvdb:372264:0", false),
        ("add_media", "movie:tmdb:0603", false),
        ("media_status", "series:tvdb:372264", true),
        ("release_choices", "series:tvdb:372264", false),
        ("start_download", "episode:tvdb:372264:1:2", true),
    ] {
        assert_eq!(pattern(tool).is_match(id), accepted, "{tool} {id}");
    }
    let t = tools.iter().find(|t| t["name"] == "search_titles").unwrap();
    assert_eq!(
        t["inputSchema"]["properties"]["kind"]["enum"],
        json!(["movie", "series"])
    );
    // Automatic add is the default; choosing a release is an explicit manual mode.
    let description = |tool: &str| {
        tools.iter().find(|t| t["name"] == tool).unwrap()["description"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(description("add_media").starts_with("The default way"));
    for tool in ["release_choices", "start_download"] {
        assert!(description(tool).starts_with("Manual mode only"), "{tool}");
    }
    assert!(description("start_download").contains("explicitly chose"));
    assert!(description("stop").starts_with("Stop getting a movie, season or episode"));
    assert!(description("stop").contains("Never deletes anything already in Plex"));
    for tool in TOOLS {
        assert!(!description(tool).contains("changes nothing"), "{tool}");
    }
}

#[tokio::test]
async fn a_new_title_is_found_added_and_tracked_and_repeating_creates_no_duplicate() {
    let e = start_exec(ExecOptions {
        tmdb: true,
        ..managed()
    })
    .await;
    let title = "Leviticus.2025.1080p.WEB-DL.DDP5.1.H.264-GRP";
    let (hash, rel) = e.publish(
        title,
        title,
        vec![(format!("{title}.mkv"), mkv(2))],
        4.0,
        false,
    );
    e.search_results("Leviticus 2025", vec![rel]);
    let token = e.dir.join("secrets/api_token");
    let s = launch(Launch::new(&e.base, &token)).await;

    let r = s
        .call(
            "search_titles",
            json!({ "kind": "movie", "q": "Leviticus" }),
        )
        .await;
    let v = ok(&r);
    assert_eq!(v["results"][0]["media_id"], "movie:tmdb:900001", "{v}");

    // One call: metadata from TMDB, monitored, searched and grabbed (a title new to the
    // native apps needs no handoff).
    let r = s
        .call("add_media", json!({ "media_id": "movie:tmdb:900001" }))
        .await;
    let v = ok(&r);
    assert_eq!(v["created"], true, "{v}");
    assert_eq!(v["adoption"]["native_app"], Value::Null, "{v}");
    assert!(
        v["adoption"].get("native_handoff_confirmed").is_none(),
        "{v}"
    );
    assert!(
        v["adoption"].get("handoff").is_none(),
        "not a native title: {v}"
    );
    assert_eq!(v["automatic"]["status"], "download_queued", "{v}");
    assert_eq!(
        v["plan"]["execution"]["grabs"].as_array().map(Vec::len),
        Some(1),
        "{v}"
    );
    e.tick().await;
    assert_eq!(adds(&e), 1);
    assert_eq!(category_of(&e, &hash), "um-movies");

    let r = s
        .call("media_status", json!({ "media_id": "movie:tmdb:900001" }))
        .await;
    let v = ok(&r);
    assert_eq!(v["acquisition"], "adopted", "{v}");
    assert_eq!(v["downloads"].as_array().map(Vec::len), Some(1), "{v}");
    // The library lists the new title compactly, downloading and not yet owned.
    let r = s
        .call("library", json!({ "kind": "movie", "q": "Leviticus" }))
        .await;
    let v = ok(&r);
    assert_eq!(
        (v["kind"].as_str(), v["total"].as_u64()),
        (Some("movie"), Some(1)),
        "{v}"
    );
    let m = &v["items"][0];
    assert_eq!(m["media_id"], "movie:tmdb:900001", "{v}");
    assert_eq!(
        (m["owned"].as_bool(), m["downloading"].as_bool()),
        (Some(false), Some(true)),
        "{v}"
    );
    assert!(
        m.get("alternate_titles").is_none(),
        "compact items only: {v}"
    );
    assert_eq!(v["next_offset"], Value::Null, "{v}");
    let r = s
        .call("library", json!({ "kind": "movie", "limit": 0 }))
        .await;
    assert_eq!(failed(&r).0, "invalid_arguments");
    let r = s.call("requests", json!({ "limit": 5 })).await;
    let v = ok(&r);
    let items = v["queue"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{v}");
    assert_eq!(items[0]["media_id"], "movie:tmdb:900001");
    let grab = items[0]["id"].clone();
    let release_ref = items[0]["release_ref"].clone();

    // Repeating the add, or the download of the same release, adds nothing.
    let r = s
        .call("add_media", json!({ "media_id": "movie:tmdb:900001" }))
        .await;
    let v = ok(&r);
    assert_eq!(v["created"], false, "{v}");
    let r = s
        .call(
            "release_choices",
            json!({ "media_id": "movie:tmdb:900001" }),
        )
        .await;
    let v = ok(&r);
    assert_eq!(v["counts"]["approved"], 0, "{v}");
    assert_eq!(
        v["releases"][0]["decision"]["rejections"][0]["code"],
        "covers_queued"
    );
    let r = s
        .call(
            "start_download",
            json!({ "media_id": "movie:tmdb:900001", "release_ref": release_ref }),
        )
        .await;
    let (code, v) = failed(&r);
    assert_eq!(
        (code, v["http_status"].as_u64()),
        ("already_queued", Some(409)),
        "{v}"
    );
    assert_eq!(v["outcome"], "not_applied");
    e.tick().await;
    assert_eq!(adds(&e), 1);
    let (_, q, _) = e.json(e.get("/api/v1/queue?all=true")).await;
    assert_eq!(q["items"].as_array().map(Vec::len), Some(1), "{q}");
    assert_eq!(q["items"][0]["id"], grab);
    let (stdout, stderr) = s.finish().await;
    assert!(!stdout.contains(TOKEN) && !stderr.contains(TOKEN));
    assert_no_leaks(&stdout);
}

#[tokio::test]
async fn an_add_committed_before_its_search_failed_is_reported_as_unknown() {
    let e = start_exec(ExecOptions {
        tmdb: true,
        ..managed()
    })
    .await;
    e.mock
        .lock()
        .unwrap()
        .failing_queries
        .push("Leviticus 2025".into());
    let token = e.dir.join("secrets/api_token");
    let s = launch(Launch::new(&e.base, &token)).await;
    // The API answers with a typed error although the add was committed.
    let r = s
        .call("add_media", json!({ "media_id": "movie:tmdb:900001" }))
        .await;
    let (code, v) = failed(&r);
    assert_eq!(
        (code, v["http_status"].as_u64(), v["outcome"].as_str()),
        ("upstream_unavailable", Some(502), Some("unknown")),
        "{v}"
    );
    assert!(
        v["guidance"]
            .as_str()
            .unwrap()
            .contains("Check media_status")
    );
    let r = s
        .call("media_status", json!({ "media_id": "movie:tmdb:900001" }))
        .await;
    assert_eq!(ok(&r)["acquisition"], "adopted");
    // Once the indexer answers, repeating the add searches again and grabs once.
    let title = "Leviticus.2025.1080p.WEB-DL.DDP5.1.H.264-GRP";
    let (_, rel) = e.publish(
        title,
        title,
        vec![(format!("{title}.mkv"), mkv(2))],
        4.0,
        false,
    );
    {
        let mut m = e.mock.lock().unwrap();
        m.failing_queries.clear();
        m.releases.insert("Leviticus 2025".into(), vec![rel]);
    }
    let r = s
        .call("add_media", json!({ "media_id": "movie:tmdb:900001" }))
        .await;
    let v = ok(&r);
    assert_eq!(v["created"], false, "{v}");
    e.tick().await;
    assert_eq!(adds(&e), 1);
    s.finish().await;
}

#[tokio::test]
async fn an_operator_adopted_scope_is_acquired_through_mcp_and_cannot_be_widened() {
    let e = start_exec(coexistence()).await;
    let title = "Slow.Horses.S01E02.1080p.WEB-DL.x264-UM";
    let (hash, ep2) = e.publish(
        title,
        title,
        vec![(format!("{title}.mkv"), mkv(2))],
        1.5,
        false,
    );
    e.search_results("Slow Horses S01E02", vec![ep2]);
    // Out of band: the operator records the native handoff for exactly this episode.
    let (status, _, text) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": "episode:tvdb:372264:1:2", "native_handoff_confirmed": true, "search": false
        })))
        .await;
    assert_eq!(status, 201, "{text}");
    let token = e.dir.join("secrets/api_token");
    let s = launch(Launch::new(&e.base, &token)).await;

    // The conversational add needs no confirmation for the adopted scope, and acquires it.
    let r = s
        .call(
            "add_media",
            json!({ "media_id": "episode:tvdb:372264:1:2" }),
        )
        .await;
    let v = ok(&r);
    assert_eq!(v["created"], false, "{v}");
    assert_eq!(v["adoption"]["handoff"], "on_record", "{v}");
    assert_eq!(v["automatic"]["status"], "download_queued", "{v}");
    assert_eq!(
        v["plan"]["execution"]["grabs"].as_array().map(Vec::len),
        Some(1),
        "{v}"
    );
    e.tick().await;
    assert_eq!(adds(&e), 1);
    assert_eq!(category_of(&e, &hash), "um-tv");
    // Anything wider is still Sonarr's.
    for wider in ["episode:tvdb:372264:1:4", "season:tvdb:372264:1"] {
        let r = s.call("add_media", json!({ "media_id": wider })).await;
        let (code, v) = failed(&r);
        assert_eq!(
            (code, v["outcome"].as_str()),
            ("native_handoff_required", Some("not_applied")),
            "{wider}: {v}"
        );
    }
    let r = s.call("requests", json!({})).await;
    let v = ok(&r);
    let items = v["queue"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{v}");
    assert_eq!(items[0]["target"], "episode:tvdb:372264:1:2");

    // A confirmed season covers its episodes whatever their current monitoring.
    let (status, _, text) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": "season:tvdb:372264:1", "native_handoff_confirmed": true, "search": false
        })))
        .await;
    assert_eq!(status, 201, "{text}");
    let (status, _, text) = e
        .json(
            e.patch("/api/v1/library/episode:tvdb:372264:1:5")
                .json(&json!({ "monitored": false })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    let r = s
        .call(
            "add_media",
            json!({ "media_id": "episode:tvdb:372264:1:5" }),
        )
        .await;
    ok(&r);
    // The confirmed scopes are visible for reconciliation with the native ledger.
    let (_, o, _) = e.json(e.get("/api/v1/ownership?state=adopted")).await;
    let item = o["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["media_id"] == "series:tvdb:372264")
        .unwrap_or_else(|| panic!("{o}"));
    let mut scopes: Vec<&str> = item["handoffs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["scope"].as_str().unwrap())
        .collect();
    scopes.sort();
    assert_eq!(
        scopes,
        ["episode:tvdb:372264:1:2", "season:tvdb:372264:1"],
        "{item}"
    );
    s.finish().await;
}

#[tokio::test]
async fn an_automatic_add_without_a_release_reads_as_waiting_and_a_real_shadow_plan_stays_shadow() {
    // Execution enabled, an operator-adopted episode, and an indexer with nothing for it.
    let e = start_exec(coexistence()).await;
    let episode = json!({ "media_id": "episode:tvdb:372264:1:2" });
    let (status, _, text) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": "episode:tvdb:372264:1:2", "native_handoff_confirmed": true, "search": false
        })))
        .await;
    assert_eq!(status, 201, "{text}");
    let token = e.dir.join("secrets/api_token");
    let s = launch(Launch::new(&e.base, &token)).await;
    let r = s.call("add_media", episode.clone()).await;
    let v = ok(&r);
    let adoption = v["adoption"].as_object().unwrap();
    let keys: BTreeSet<&str> = adoption.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "acquisition",
            "handoff",
            "mode",
            "native_app",
            "operator_note"
        ]),
        "{v}"
    );
    assert_eq!(
        (
            &adoption["acquisition"],
            &adoption["mode"],
            &adoption["native_app"],
            &adoption["handoff"]
        ),
        (
            &json!("adopted"),
            &json!("coexistence"),
            &json!("Sonarr"),
            &json!("on_record")
        )
    );
    assert_eq!(v["plan"]["state"], "blocked", "{v}");
    let ex = &v["plan"]["execution"];
    assert_eq!(
        (
            &ex["download_submission"],
            &ex["mode"],
            &ex["submitted"],
            &ex["grabs"]
        ),
        (
            &json!("enabled"),
            &json!("not_executed"),
            &json!(false),
            &json!([])
        ),
        "{v}"
    );
    assert_eq!(v["automatic"]["status"], "not_downloaded_yet", "{v}");
    let message = v["automatic"]["message"].as_str().unwrap();
    assert!(message.contains("execution is enabled") && message.contains("check requests"));
    let text = v.to_string();
    for misleading in ["native_handoff_confirmed", "shadow plan", "/execute"] {
        assert!(!text.contains(misleading), "{misleading}: {text}");
    }
    // The waiting request the message points to exists (the core recorded it).
    let r = s.call("requests", json!({})).await;
    let v = ok(&r);
    assert!(
        v["requests"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["target"] == "episode:tvdb:372264:1:2" && i["state"] == "active"),
        "{v}"
    );
    e.tick().await;
    assert_eq!(adds(&e), 0);
    s.finish().await;
    // The REST response itself is unchanged.
    let (status, v, text) = e.json(e.post("/api/v1/library").json(&episode)).await;
    assert_eq!(status, 201, "{text}");
    assert_eq!(v["plan"]["execution"]["mode"], "shadow", "{v}");
    assert_eq!(v["adoption"]["native_handoff_confirmed"], false, "{v}");

    // Execution disabled: the shadow plan is reported as one.
    let (addr, _mock) = start_mock().await;
    let app = start_app(&snapshot(), "", Some(addr), false).await;
    let token = app.dir.join("secrets/api_token");
    let s = launch(Launch::new(&app.base, &token)).await;
    let r = s
        .call("add_media", json!({ "media_id": "movie:tmdb:603" }))
        .await;
    let v = ok(&r);
    let ex = &v["plan"]["execution"];
    assert_eq!(
        (&ex["download_submission"], &ex["mode"]),
        (&json!("disabled"), &json!("shadow")),
        "{v}"
    );
    assert_eq!(ex["note"], "shadow plan only: [execution] enabled = false");
    assert_eq!(v["automatic"]["status"], "execution_disabled", "{v}");
    assert!(v["adoption"].get("handoff").is_none(), "legacy mode: {v}");
    s.finish().await;
}

#[tokio::test]
async fn an_unadopted_native_title_is_refused_with_the_core_error_and_nothing_happens() {
    let e = start_exec(coexistence()).await;
    let (_, matrix) = e.publish(
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        vec![("The.Matrix.1999.1080p.WEB-DL.x264-UM.mkv".into(), mkv(2))],
        0.05,
        false,
    );
    e.search_results("The Matrix 1999", vec![matrix]);
    let token = e.dir.join("secrets/api_token");
    let s = launch(Launch::new(&e.base, &token)).await;

    for id in [
        "movie:tmdb:603",
        "season:tvdb:372264:1",
        "episode:tvdb:372264:1:2",
    ] {
        let r = s.call("add_media", json!({ "media_id": id })).await;
        let (code, v) = failed(&r);
        assert_eq!(
            (code, v["http_status"].as_u64()),
            ("native_handoff_required", Some(409)),
            "{id}: {v}"
        );
        assert_eq!(v["outcome"], "not_applied");
        assert!(
            v["routing"]
                .as_str()
                .unwrap()
                .contains("does not fall back")
        );
    }
    // The assistant cannot confirm the handoff itself.
    let r = s
        .call(
            "add_media",
            json!({ "media_id": "movie:tmdb:603", "native_handoff_confirmed": true }),
        )
        .await;
    let (code, v) = failed(&r);
    assert_eq!(
        (code, v["outcome"].as_str()),
        ("invalid_arguments", Some("not_sent")),
        "{v}"
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("native_handoff_confirmed")
    );
    // Releases can be listed (a read), but downloading one is refused by the core.
    let r = s
        .call("release_choices", json!({ "media_id": "movie:tmdb:603" }))
        .await;
    let v = ok(&r);
    let release_ref = v["releases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["decision"]["approved"] == true)
        .unwrap_or_else(|| panic!("an approved release: {v}"))["release_ref"]
        .clone();
    let r = s
        .call(
            "start_download",
            json!({ "media_id": "movie:tmdb:603", "release_ref": release_ref }),
        )
        .await;
    let (code, v) = failed(&r);
    assert_eq!(
        (code, v["http_status"].as_u64()),
        ("not_adopted", Some(409)),
        "{v}"
    );
    assert_eq!(v["outcome"], "not_applied");
    assert!(v["routing"].is_string());

    e.tick().await;
    assert_eq!(adds(&e), 0);
    assert!(
        e.mock.lock().unwrap().downloads.is_empty(),
        "no torrent fetched"
    );
    let (_, q, _) = e.json(e.get("/api/v1/queue?all=true")).await;
    assert_eq!(q["items"], json!([]));
    let (_, r, _) = e.json(e.get("/api/v1/requests")).await;
    assert_eq!(r["items"], json!([]));
    let (_, o, _) = e.json(e.get("/api/v1/ownership")).await;
    assert_eq!(o["counts"]["adopted"], Value::Null, "{o}");
    let r = s
        .call("media_status", json!({ "media_id": "movie:tmdb:603" }))
        .await;
    assert_eq!(ok(&r)["acquisition"], "observed");
    s.finish().await;
}

type Seen = Arc<Mutex<Vec<(String, String, Option<String>, String)>>>;

/// A stand-in API that records every request (method, path and query, Authorization,
/// body) and answers `{}`.
async fn recorder() -> (String, Seen) {
    use axum::extract::Request;
    let seen: Seen = Arc::default();
    let s = seen.clone();
    let app = axum::Router::new().fallback(move |req: Request| {
        let s = s.clone();
        async move {
            let (parts, body) = req.into_parts();
            let body = axum::body::to_bytes(body, 1 << 16).await.unwrap();
            s.lock().unwrap().push((
                parts.method.to_string(),
                parts.uri.to_string(),
                parts
                    .headers
                    .get("authorization")
                    .map(|v| v.to_str().unwrap().to_string()),
                String::from_utf8_lossy(&body).into_owned(),
            ));
            axum::Json(json!({ "items": [] }))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/um"), seen)
}

#[tokio::test]
async fn malformed_or_overreaching_arguments_never_reach_the_api() {
    let (api, seen) = recorder().await;
    let tmp = tempfile::tempdir().unwrap();
    let token = token_file(tmp.path());
    let s = launch(Launch::new(&api, &token)).await;
    let rel = format!("rel_{}", "ab".repeat(12));
    let long = "x".repeat(101);
    let refused = [
        ("search_titles", json!({ "kind": "movie" })),
        ("search_titles", json!({ "kind": "book", "q": "x" })),
        ("search_titles", json!({ "kind": "movie", "q": "  " })),
        ("search_titles", json!({ "kind": "movie", "q": long })),
        ("search_titles", json!({ "kind": "movie", "q": "a\u{7}b" })),
        (
            "search_titles",
            json!({ "kind": "movie", "q": "x", "year": 1700 }),
        ),
        (
            "search_titles",
            json!({ "kind": "movie", "q": "x", "url": "http://elsewhere/" }),
        ),
        ("add_media", json!({})),
        ("add_media", json!({ "media_id": 603 })),
        ("add_media", json!({ "media_id": "movie:tmdb:0603" })),
        ("add_media", json!({ "media_id": "movie:tmdb:603 " })),
        ("add_media", json!({ "media_id": "MOVIE:tmdb:603" })),
        ("add_media", json!({ "media_id": "tmdb:603" })),
        ("add_media", json!({ "media_id": "series:tvdb:372264" })),
        ("add_media", json!({ "media_id": "season:tvdb:372264:0" })),
        (
            "add_media",
            json!({ "media_id": "movie:tmdb:603", "search": false }),
        ),
        (
            "add_media",
            json!({ "media_id": "movie:tmdb:603", "monitored": true }),
        ),
        (
            "add_media",
            json!({ "media_id": "movie:tmdb:603", "quality_profile": "Any" }),
        ),
        (
            "add_media",
            json!({ "media_id": "movie:tmdb:603", "root_folder": "/movies" }),
        ),
        (
            "add_media",
            json!({ "media_id": "movie:tmdb:603", "preserve_monitoring": true }),
        ),
        (
            "add_media",
            json!({ "media_id": "movie:tmdb:603", "metadata": { "title": "x" } }),
        ),
        ("media_status", json!({ "media_id": "../../health" })),
        ("requests", json!({ "limit": 0 })),
        ("requests", json!({ "limit": 51 })),
        ("requests", json!({ "all": true })),
        (
            "release_choices",
            json!({ "media_id": "series:tvdb:372264" }),
        ),
        (
            "release_choices",
            json!({ "media_id": "movie:tmdb:603", "refresh": true }),
        ),
        ("start_download", json!({ "media_id": "movie:tmdb:603" })),
        (
            "start_download",
            json!({ "media_id": "movie:tmdb:603", "release_ref": "rel_x" }),
        ),
        (
            "start_download",
            json!({ "media_id": "movie:tmdb:603", "release_ref": rel.to_uppercase().replace("REL_", "rel_") }),
        ),
        (
            "start_download",
            json!({ "media_id": "movie:tmdb:603", "release_ref": rel, "magnet": "magnet:?xt=x" }),
        ),
        (
            "start_download",
            json!({ "media_id": "movie:tmdb:603", "release_ref": rel, "category": "radarr" }),
        ),
    ];
    for (tool, args) in refused {
        let r = s.call(tool, args.clone()).await;
        let (code, v) = failed(&r);
        assert_eq!(code, "invalid_arguments", "{tool} {args}: {v}");
        assert_eq!(v["outcome"], "not_sent");
    }
    let unknown = s
        .client
        .call_tool(CallToolRequestParams::new("delete_media"))
        .await;
    assert!(unknown.is_err(), "unknown tools are protocol errors");
    // A message over the adapter's 256 KiB line limit is dropped unanswered; the session
    // stays usable.
    let huge = s.call(
        "search_titles",
        json!({ "kind": "movie", "q": "x".repeat(300 * 1024) }),
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(2), huge)
            .await
            .is_err()
    );
    let r = s.call("add_media", json!({})).await;
    assert_eq!(failed(&r).0, "invalid_arguments");
    assert!(
        seen.lock().unwrap().is_empty(),
        "{:?}",
        seen.lock().unwrap()
    );

    // Valid calls map to exactly these REST requests under the configured base path.
    let accepted = [
        (
            "search_titles",
            json!({ "kind": "series", "q": " slow horses ", "year": 2022 }),
        ),
        (
            "add_media",
            json!({ "media_id": "episode:tvdb:372264:1:2" }),
        ),
        ("media_status", json!({ "media_id": "series:tvmaze:9001" })),
        ("requests", json!({})),
        (
            "release_choices",
            json!({ "media_id": "season:tvdb:372264:1" }),
        ),
        (
            "start_download",
            json!({ "media_id": "movie:tmdb:603", "release_ref": rel }),
        ),
    ];
    for (tool, args) in accepted {
        ok(&s.call(tool, args).await);
    }
    let bearer = format!("Bearer {TOKEN}");
    let seen = seen.lock().unwrap().clone();
    let got: Vec<(&str, &str, Option<Value>)> = seen
        .iter()
        .map(|(m, u, auth, body)| {
            assert_eq!(auth.as_deref(), Some(bearer.as_str()));
            (m.as_str(), u.as_str(), serde_json::from_str(body).ok())
        })
        .collect();
    assert_eq!(
        got,
        [
            (
                "GET",
                "/um/api/v1/discover?kind=series&q=slow+horses&year=2022",
                None
            ),
            (
                "POST",
                "/um/api/v1/library",
                Some(json!({ "media_id": "episode:tvdb:372264:1:2" }))
            ),
            ("GET", "/um/api/v1/status/series:tvmaze:9001", None),
            ("GET", "/um/api/v1/queue?limit=20", None),
            ("GET", "/um/api/v1/requests", None),
            (
                "GET",
                "/um/api/v1/media/season:tvdb:372264:1/releases",
                None
            ),
            (
                "POST",
                "/um/api/v1/grabs",
                Some(json!({ "media_id": "movie:tmdb:603", "release_ref": rel }))
            ),
        ]
    );
    s.finish().await;
}

#[tokio::test]
async fn an_unreachable_hanging_or_broken_api_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let token = token_file(tmp.path());
    let rel = format!("rel_{}", "ab".repeat(12));
    let every_tool = [
        (
            "search_titles",
            json!({ "kind": "movie", "q": "The Matrix" }),
        ),
        ("add_media", json!({ "media_id": "movie:tmdb:603" })),
        ("media_status", json!({ "media_id": "movie:tmdb:603" })),
        ("requests", json!({})),
        ("release_choices", json!({ "media_id": "movie:tmdb:603" })),
        (
            "start_download",
            json!({ "media_id": "movie:tmdb:603", "release_ref": rel }),
        ),
    ];

    let write = |tool: &str| matches!(tool, "add_media" | "start_download");

    // Nothing listens: no request was delivered.
    let closed = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    let api = format!("http://{closed}/");
    let s = launch(Launch::new(&api, &token)).await;
    for (tool, args) in &every_tool {
        let r = s.call(tool, args.clone()).await;
        let (code, v) = failed(&r);
        assert_eq!(code, "api_unreachable", "{tool}: {v}");
        let outcome = if write(tool) {
            "not_sent"
        } else {
            "nothing_changed"
        };
        assert_eq!(v["outcome"], outcome, "{tool}: {v}");
        assert!(v["guidance"].as_str().unwrap().contains("no fallback"));
    }
    assert_eq!(
        s.client.list_all_tools().await.unwrap().len(),
        TOOLS.len(),
        "session stays usable"
    );
    s.finish().await;

    // Accepts connections and never answers: a write's outcome is unknown.
    let hang = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}/", hang.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = vec![];
        while let Ok((sock, _)) = hang.accept().await {
            held.push(sock);
        }
    });
    let s = launch(Launch {
        timeout_secs: 1,
        ..Launch::new(&api, &token)
    })
    .await;
    let started = Instant::now();
    let r = s
        .call("add_media", json!({ "media_id": "movie:tmdb:603" }))
        .await;
    let (code, v) = failed(&r);
    assert_eq!(
        (code, v["outcome"].as_str()),
        ("api_timeout", Some("unknown")),
        "{v}"
    );
    assert!(
        v["guidance"]
            .as_str()
            .unwrap()
            .contains("Check media_status")
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    s.finish().await;

    // Answers, but not with the API's JSON.
    let app = axum::Router::new().fallback(|| async {
        (
            axum::http::StatusCode::BAD_GATEWAY,
            "<html>bad gateway</html>",
        )
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let s = launch(Launch::new(&api, &token)).await;
    for (tool, args) in &every_tool {
        let r = s.call(tool, args.clone()).await;
        let (code, v) = failed(&r);
        assert_eq!(code, "api_bad_response", "{tool}: {v}");
        assert!(v["error"]["message"].as_str().unwrap().contains("HTTP 502"));
        let outcome = if write(tool) {
            "unknown"
        } else {
            "nothing_changed"
        };
        assert_eq!(v["outcome"], outcome, "{tool}: {v}");
    }
    s.finish().await;

    // Redirects elsewhere are not followed.
    let (elsewhere, hits) = sentinel().await;
    let app = axum::Router::new().fallback(move || {
        let to = format!("{elsewhere}/api/v1/library");
        async move {
            (
                axum::http::StatusCode::TEMPORARY_REDIRECT,
                [("location", to)],
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let s = launch(Launch::new(&api, &token)).await;
    let r = s
        .call("add_media", json!({ "media_id": "movie:tmdb:603" }))
        .await;
    let (code, v) = failed(&r);
    assert_eq!(
        (code, v["outcome"].as_str()),
        ("api_bad_response", Some("unknown")),
        "{v}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0, "redirect followed");
    s.finish().await;
}

#[cfg(unix)] // Unix permissions or symlinks
#[tokio::test]
async fn the_token_never_leaves_through_stdout_stderr_or_results() {
    // A stand-in API that echoes the Authorization header back, in a success body and in a
    // typed error: the adapter redacts both.
    let app = axum::Router::new().fallback(|req: axum::extract::Request| async move {
        let auth = req
            .headers()
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        if req.method() == axum::http::Method::POST {
            (
                axum::http::StatusCode::CONFLICT,
                axum::Json(
                    json!({ "error": { "code": "internal", "message": format!("saw {auth}") } }),
                ),
            )
        } else {
            (
                axum::http::StatusCode::OK,
                axum::Json(json!({ "echo": auth })),
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let tmp = tempfile::tempdir().unwrap();
    let token = token_file(tmp.path());
    let s = launch(Launch {
        rust_log: Some("trace"),
        ..Launch::new(&api, &token)
    })
    .await;
    let r = s
        .call("media_status", json!({ "media_id": "movie:tmdb:603" }))
        .await;
    assert_eq!(ok(&r)["echo"], "Bearer [redacted]");
    let r = s
        .call("add_media", json!({ "media_id": "movie:tmdb:603" }))
        .await;
    let (code, v) = failed(&r);
    assert_eq!(code, "internal");
    assert_eq!(v["error"]["message"], "saw Bearer [redacted]");
    let (stdout, stderr) = s.finish().await;
    assert!(stdout.contains("[redacted]"));
    assert!(!stderr.is_empty(), "trace logging was on");
    for (stream, text) in [("stdout", &stdout), ("stderr", &stderr)] {
        assert!(!text.contains(TOKEN), "{stream} leaked the token");
    }

    // Refused before serving: nothing on stdout, nothing secret on stderr.
    let open = tmp.path().join("open-token");
    std::fs::write(&open, TOKEN).unwrap();
    std::fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    let cases = [
        (api.clone(), open.clone(), "chmod 600"),
        (
            format!("http://user:{FAKE_PASSWORD}@127.0.0.1:9/"),
            token.clone(),
            "credentials must not be embedded",
        ),
        (
            format!("http://127.0.0.1:9/?token={FAKE_PASSWORD}"),
            token.clone(),
            "must not contain a query",
        ),
        (
            "file:///etc/".to_string(),
            token.clone(),
            "scheme must be http or https",
        ),
    ];
    for (url, file, why) in cases {
        let (proxy, hits) = sentinel().await;
        let out = command(&proxy)
            .arg("mcp")
            .args(["--api-url", &url, "--token-file"])
            .arg(&file)
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{url}");
        assert!(out.stdout.is_empty(), "{url}");
        assert!(stderr.contains(why), "{url}: {stderr}");
        assert!(
            !stderr.contains(TOKEN) && !stderr.contains(FAKE_PASSWORD),
            "{stderr}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn stop_ends_the_search_for_a_wanted_movie_and_says_what_happened() {
    let e = start_exec(ExecOptions {
        tmdb: true,
        ..coexistence()
    })
    .await;
    let token = e.dir.join("secrets/api_token");
    let s = launch(Launch::new(&e.base, &token)).await;
    // No release exists: the movie is wanted and Arr keeps looking.
    let r = s
        .call("add_media", json!({ "media_id": "movie:tmdb:900001" }))
        .await;
    ok(&r);
    let r = s
        .call("stop", json!({ "media_id": "movie:tmdb:900001" }))
        .await;
    let v = ok(&r);
    assert_eq!(
        (v["stopped"].clone(), v["cancelled"].clone()),
        (json!(true), json!(0)),
        "{v}"
    );
    let message = v["message"].as_str().unwrap();
    assert!(
        message.contains("Cancelled 0 downloads") && message.contains("Nothing already in Plex"),
        "{message}"
    );
    let r = s
        .call("media_status", json!({ "media_id": "movie:tmdb:900001" }))
        .await;
    assert_eq!(ok(&r)["monitored"], false);
    // Whole series are not stopped here, and something Arr never had changed nothing.
    let r = s
        .call("stop", json!({ "media_id": "series:tvdb:372264" }))
        .await;
    assert_eq!(failed(&r).0, "invalid_arguments");
    let r = s
        .call("stop", json!({ "media_id": "movie:tmdb:777" }))
        .await;
    let (code, v) = failed(&r);
    assert_eq!(code, "not_managed", "{v}");
    assert_eq!(v["outcome"], "not_applied");
    s.finish().await;
}
