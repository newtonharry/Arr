//! A standalone deployment from zero, with no Radarr/Sonarr snapshot, no Prowlarr and no
//! qBittorrent anywhere: the real CLI creates the state (`init`, `setup`, `check`), the API
//! discovers the Plex library and bootstraps owned titles from it (read-only), then movies
//! and a season are added and found through the native indexers (The Pirate Bay's JSON API and
//! a Torznab endpoint), downloaded by the embedded engine from a controlled seeding peer,
//! validated and imported, Plex is refreshed for the imported folder, RSS picks up a release
//! from the TPB recent feed, and everything survives a restart.
//!
//! Every upstream is one recording mock; any request to a path it does not serve (a Prowlarr,
//! qBittorrent, Sonarr or Radarr endpoint included) is recorded as forbidden and fails the test.
#![recursion_limit = "256"]

mod common;

use arr::fixture::TorrentFixture;
use arr::{api, config, fsutil};
use axum::Router;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use common::{mkv, playable};
use librqbit::{
    AddTorrent, AddTorrentOptions, ListenerMode, ListenerOptions, Session, SessionOptions,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_arr");
const TZ_KEY: &str = "FAKE-TORZNAB-KEY-STANDALONE-0001";
const PLEX_TOKEN: &str = "FAKE-PLEX-TOKEN-STANDALONE-0001";
const TMDB_KEY: &str = "FAKE-TMDB-KEY-STANDALONE-0001";
const MCP_TOKEN: &str = "mcp-token-standalone-0123456789abcdef";

// ------------------------------------------------------------------ the recording mock

#[derive(Default)]
struct World {
    /// Raw query strings of `q.php` requests, as received.
    tpb_queries: Vec<String>,
    /// Keyword text -> answer body (default: the site's "no results" row).
    tpb_results: HashMap<String, Value>,
    tpb_status: HashMap<String, u16>,
    tpb_recent: Vec<Value>,
    tpb_recent_hits: usize,
    /// Raw query strings of Torznab API requests.
    tz_queries: Vec<String>,
    /// Keyword text -> `<item>` XML.
    tz_items: HashMap<String, String>,
    tz_errors: HashMap<String, String>,
    tz_feed: String,
    /// Info-hash -> `.torrent` served at `/tz/dl/{hash}` (only with the API key).
    torrents: HashMap<String, Vec<u8>>,
    tz_downloads: Vec<String>,
    plex_refreshes: Vec<String>,
    plex_movies: Vec<Value>,
    plex_shows: Vec<Value>,
    plex_leaves: HashMap<String, Vec<Value>>,
    plex_pages: Vec<String>,
    tmdb: HashMap<String, Value>,
    tmdb_find: HashMap<String, u32>,
    tvmaze_shows: HashMap<u32, Value>,
    tvmaze_episodes: HashMap<u32, Vec<Value>>,
    tvmaze_akas: HashMap<u32, Vec<Value>>,
    /// TVmaze show ids whose AKA list was requested.
    tvmaze_akas_hits: Vec<u32>,
    forbidden: Vec<String>,
}

type Mock = Arc<Mutex<World>>;

fn sentinel_row() -> Value {
    json!([{ "id": "0", "name": "No results returned", "info_hash": "0000000000000000000000000000000000000000",
        "leechers": "0", "seeders": "0", "num_files": "0", "size": "0", "username": "", "added": "0",
        "status": "member", "category": "0", "imdb": "" }])
}

fn query_param(uri: &Uri, name: &str) -> Option<String> {
    reqwest::Url::parse(&format!("http://x{uri}"))
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

async fn tpb_search(State(m): State<Mock>, uri: Uri) -> Response {
    let mut w = m.lock().unwrap();
    w.tpb_queries.push(uri.query().unwrap_or("").to_string());
    // The live API answers a comma category list with `+`-encoded spaces with its "no results"
    // row; the mock does the same, so only `%20` encoding finds anything.
    let raw = uri.query().unwrap_or("");
    let q = query_param(&uri, "q").unwrap_or_default();
    if raw.contains('+') && raw.contains(',') {
        return axum::Json(sentinel_row()).into_response();
    }
    if let Some(s) = w.tpb_status.get(&q) {
        return StatusCode::from_u16(*s).unwrap().into_response();
    }
    axum::Json(w.tpb_results.get(&q).cloned().unwrap_or_else(sentinel_row)).into_response()
}

async fn tpb_recent(State(m): State<Mock>) -> Response {
    let mut w = m.lock().unwrap();
    w.tpb_recent_hits += 1;
    axum::Json(Value::Array(w.tpb_recent.clone())).into_response()
}

const CAPS: &str = r#"<?xml version="1.0" encoding="UTF-8"?><caps><server title="Mock"/><limits max="100" default="50"/><searching><search available="yes" supportedParams="q"/><tv-search available="yes" supportedParams="q,season,ep"/><movie-search available="yes" supportedParams="q"/></searching><categories><category id="2000" name="Movies"/><category id="5000" name="TV"><subcat id="5040" name="TV/HD"/></category></categories></caps>"#;

fn rss(items: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed"><channel><title>Mock</title>{items}</channel></rss>"#
    )
}

async fn tz_api(State(m): State<Mock>, uri: Uri) -> Response {
    let mut w = m.lock().unwrap();
    w.tz_queries.push(uri.query().unwrap_or("").to_string());
    let xml = |s: String| ([("content-type", "application/rss+xml")], s).into_response();
    if query_param(&uri, "apikey").as_deref() != Some(TZ_KEY) {
        return xml(r#"<error code="100" description="Incorrect user credentials"/>"#.into());
    }
    match query_param(&uri, "t").as_deref() {
        Some("caps") => xml(CAPS.into()),
        Some("search") => {
            let q = query_param(&uri, "q").unwrap_or_default();
            if let Some(code) = w.tz_errors.get(&q) {
                return xml(format!(
                    r#"<error code="{code}" description="apikey={TZ_KEY}"/>"#
                ));
            }
            if q.is_empty() {
                return xml(rss(&w.tz_feed));
            }
            xml(rss(w.tz_items.get(&q).map(String::as_str).unwrap_or("")))
        }
        _ => xml(r#"<error code="202" description="No such function"/>"#.into()),
    }
}

async fn tz_download(State(m): State<Mock>, AxPath(hash): AxPath<String>, uri: Uri) -> Response {
    let mut w = m.lock().unwrap();
    w.tz_downloads.push(uri.query().unwrap_or("").to_string());
    if query_param(&uri, "apikey").as_deref() != Some(TZ_KEY) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match w.torrents.get(&hash) {
        Some(t) => ([("content-type", "application/x-bittorrent")], t.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn plex_ok(h: &HeaderMap) -> bool {
    h.get("x-plex-token").and_then(|v| v.to_str().ok()) == Some(PLEX_TOKEN)
}

async fn plex_sections(headers: HeaderMap) -> Response {
    if !plex_ok(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    axum::Json(json!({ "MediaContainer": { "size": 3, "Directory": [
        { "key": "1", "title": "Movies", "type": "movie", "Location": [{ "id": 1, "path": "/media_library/movies" }] },
        { "key": "2", "title": "TV Shows", "type": "show", "Location": [{ "id": 2, "path": "/media_library/tvshows" }] },
        { "key": "3", "title": "Music", "type": "artist", "Location": [{ "id": 3, "path": "/media_library/music" }] },
    ] } }))
    .into_response()
}

fn paged(items: &[Value], uri: &Uri) -> Value {
    let start: usize = query_param(uri, "X-Plex-Container-Start")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let size: usize = query_param(uri, "X-Plex-Container-Size")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let page: Vec<Value> = items.iter().skip(start).take(size).cloned().collect();
    json!({ "MediaContainer": { "size": page.len(), "totalSize": items.len(), "offset": start, "Metadata": page } })
}

async fn plex_all(
    State(m): State<Mock>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    uri: Uri,
) -> Response {
    if !plex_ok(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut w = m.lock().unwrap();
    w.plex_pages
        .push(format!("{id}?{}", uri.query().unwrap_or("")));
    let items = match id.as_str() {
        "1" => w.plex_movies.clone(),
        "2" => w.plex_shows.clone(),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    axum::Json(paged(&items, &uri)).into_response()
}

async fn plex_leaves(
    State(m): State<Mock>,
    headers: HeaderMap,
    AxPath(key): AxPath<String>,
    uri: Uri,
) -> Response {
    if !plex_ok(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let w = m.lock().unwrap();
    axum::Json(paged(
        w.plex_leaves.get(&key).map(Vec::as_slice).unwrap_or(&[]),
        &uri,
    ))
    .into_response()
}

async fn plex_refresh(
    State(m): State<Mock>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    uri: Uri,
) -> Response {
    if !plex_ok(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let path = query_param(&uri, "path").unwrap_or_default();
    m.lock()
        .unwrap()
        .plex_refreshes
        .push(format!("{id}:{path}"));
    StatusCode::OK.into_response()
}

fn tmdb_ok(h: &HeaderMap, uri: &Uri) -> bool {
    query_param(uri, "api_key").as_deref() == Some(TMDB_KEY)
        || h.get("authorization").and_then(|v| v.to_str().ok())
            == Some(&format!("Bearer {TMDB_KEY}"))
}

async fn tmdb_movie(
    State(m): State<Mock>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    uri: Uri,
) -> Response {
    if !tmdb_ok(&headers, &uri) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match m.lock().unwrap().tmdb.get(&id) {
        Some(v) => axum::Json(v.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn tmdb_find(
    State(m): State<Mock>,
    headers: HeaderMap,
    AxPath(imdb): AxPath<String>,
    uri: Uri,
) -> Response {
    if !tmdb_ok(&headers, &uri) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let hit = m.lock().unwrap().tmdb_find.get(&imdb).copied();
    axum::Json(
        json!({ "movie_results": hit.map(|id| vec![json!({ "id": id })]).unwrap_or_default() }),
    )
    .into_response()
}

async fn tmdb_search(State(m): State<Mock>, headers: HeaderMap, uri: Uri) -> Response {
    if !tmdb_ok(&headers, &uri) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let q = query_param(&uri, "query")
        .unwrap_or_default()
        .to_lowercase();
    let w = m.lock().unwrap();
    let results: Vec<Value> = w
        .tmdb
        .values()
        .filter(|v| v["title"].as_str().unwrap_or("").to_lowercase().contains(&q))
        .map(|v| json!({ "id": v["id"], "title": v["title"], "release_date": v["release_date"], "original_language": "en" }))
        .collect();
    axum::Json(json!({ "page": 1, "results": results, "total_results": results.len() }))
        .into_response()
}

/// Redirects of the Torznab endpoint's download proxy (one per case).
async fn tz_redirect(State(m): State<Mock>, AxPath(kind): AxPath<String>) -> Response {
    let hash = "c12fe1c06bba254a9dc9f519b335aa7c1367a88a";
    let location = match kind.as_str() {
        "foreign" => "http://127.0.0.2:9/stolen.torrent".to_string(),
        "magnet_leak" => format!(
            "magnet:?xt=urn:btih:{hash}&dn=The.Matrix.1999.1080p.WEB-DL.x264-UM&tr=https%3A%2F%2Ftracker.example%2Fannounce%3Fpasskey%3D{TZ_KEY}"
        ),
        "announce_leak" => {
            let t = m
                .lock()
                .unwrap()
                .torrents
                .get("announce_leak")
                .cloned()
                .unwrap_or_default();
            return ([("content-type", "application/x-bittorrent")], t).into_response();
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (StatusCode::FOUND, [("location", location)]).into_response()
}

async fn tvmaze_lookup(State(m): State<Mock>, uri: Uri) -> Response {
    let tvdb: u32 = query_param(&uri, "thetvdb")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let w = m.lock().unwrap();
    match w
        .tvmaze_shows
        .values()
        .find(|s| s["externals"]["thetvdb"] == json!(tvdb))
    {
        Some(s) => axum::Json(s.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn tvmaze_show(State(m): State<Mock>, AxPath(id): AxPath<u32>) -> Response {
    match m.lock().unwrap().tvmaze_shows.get(&id) {
        Some(s) => axum::Json(s.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn tvmaze_episodes(State(m): State<Mock>, AxPath(id): AxPath<u32>) -> Response {
    axum::Json(
        m.lock()
            .unwrap()
            .tvmaze_episodes
            .get(&id)
            .cloned()
            .unwrap_or_default(),
    )
    .into_response()
}

async fn tvmaze_akas(State(m): State<Mock>, AxPath(id): AxPath<u32>) -> Response {
    let mut w = m.lock().unwrap();
    w.tvmaze_akas_hits.push(id);
    match w.tvmaze_akas.get(&id) {
        Some(a) => axum::Json(a.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn forbidden(State(m): State<Mock>, uri: Uri, method: axum::http::Method) -> Response {
    m.lock()
        .unwrap()
        .forbidden
        .push(format!("{method} {}", uri.path()));
    StatusCode::NOT_FOUND.into_response()
}

async fn start_mock() -> (SocketAddr, Mock) {
    let m: Mock = Arc::new(Mutex::new(World::default()));
    let app = Router::new()
        .route("/tpb/q.php", get(tpb_search))
        .route("/tpb/precompiled/data_top100_recent.json", get(tpb_recent))
        .route("/tz/api", get(tz_api))
        .route("/tz/dl/{hash}", get(tz_download))
        .route("/plex/library/sections", get(plex_sections))
        .route("/plex/library/sections/{id}/all", get(plex_all))
        .route("/plex/library/metadata/{key}/allLeaves", get(plex_leaves))
        .route("/plex/library/sections/{id}/refresh", post(plex_refresh))
        .route("/tmdb/3/movie/{id}", get(tmdb_movie))
        .route("/tmdb/3/find/{imdb}", get(tmdb_find))
        .route("/tmdb/3/search/movie", get(tmdb_search))
        .route("/tz/go/{kind}", get(tz_redirect))
        .route("/tvmaze/lookup/shows", get(tvmaze_lookup))
        .route("/tvmaze/shows/{id}", get(tvmaze_show))
        .route("/tvmaze/shows/{id}/episodes", get(tvmaze_episodes))
        .route("/tvmaze/shows/{id}/akas", get(tvmaze_akas))
        .fallback(forbidden)
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, m)
}

// ------------------------------------------------------------------ fixtures

fn tmdb_details(id: u32, title: &str, year: u16, runtime: u32) -> Value {
    json!({ "id": id, "title": title, "original_language": "en", "release_date": format!("{year}-03-31"),
        "runtime": runtime, "imdb_id": "tt0133093", "status": "Released",
        "alternative_titles": { "titles": [] },
        "release_dates": { "results": [{ "iso_3166_1": "US", "release_dates": [{ "type": 4, "release_date": format!("{year}-09-21T00:00:00.000Z") }] }] } })
}

fn tvmaze_slow_horses() -> (Value, Vec<Value>) {
    let show = json!({ "id": 9001, "name": "Slow Horses", "premiered": "2022-04-01", "language": "English",
        "runtime": 50, "status": "Running", "type": "Scripted", "externals": { "thetvdb": 372264, "imdb": "tt5875444" } });
    let eps = (1..=6)
        .map(|n| json!({ "season": 1, "number": n, "name": format!("Episode {n}"),
            "airstamp": format!("2022-04-{:02}T02:00:00+00:00", n + 1), "runtime": 50, "type": "regular" }))
        .collect();
    (show, eps)
}

fn tpb_row(id: u32, t: &TorrentFixture, category: u32, added: i64) -> Value {
    json!({ "id": id.to_string(), "name": t.name, "info_hash": t.info_hash.to_uppercase(), "leechers": "2", "seeders": "40",
        "num_files": "1", "size": t.size.to_string(), "username": "uploader", "added": added.to_string(),
        "status": "member", "category": category.to_string(), "imdb": "" })
}

fn tz_item(t: &TorrentFixture, base: &str, category: u32) -> String {
    // The endpoint signs its own download links; this one omits the key, which the native
    // client adds for the endpoint's own origin only.
    format!(
        r#"<item><title>{}</title><guid>tz-{}</guid><link>{base}/tz/dl/{}</link><pubDate>Thu, 01 Oct 2026 00:00:00 +0000</pubDate><size>{}</size><enclosure url="{base}/tz/dl/{}" length="{}" type="application/x-bittorrent"/><torznab:attr name="category" value="{category}"/><torznab:attr name="seeders" value="25"/><torznab:attr name="peers" value="30"/><torznab:attr name="infohash" value="{}"/></item>"#,
        t.name, t.info_hash, t.info_hash, t.size, t.info_hash, t.size, t.info_hash
    )
}

/// A controlled seeding peer (its own librqbit session on loopback, explicit addresses only).
struct Seeder {
    session: Arc<Session>,
    dir: PathBuf,
}

impl Seeder {
    async fn start(dir: &Path) -> Seeder {
        std::fs::create_dir_all(dir).unwrap();
        let session = Session::new_with_opts(
            dir.to_path_buf(),
            SessionOptions {
                dht: None,
                disable_trackers: true,
                listen: Some(ListenerOptions {
                    mode: ListenerMode::TcpOnly,
                    listen_addr: "127.0.0.1:0".parse().unwrap(),
                    ipv4_only: true,
                    ..Default::default()
                }),
                disable_local_service_discovery: true,
                ipv4_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        Seeder {
            session,
            dir: dir.to_path_buf(),
        }
    }

    fn addr(&self) -> SocketAddr {
        self.session.listen_addr().unwrap()
    }

    async fn seed(&self, t: &TorrentFixture, files: &[(String, Vec<u8>)]) {
        let out = self.dir.join(&t.info_hash);
        for (rel, bytes) in files {
            let p = out.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, bytes).unwrap();
        }
        let h = self
            .session
            .add_torrent(
                AddTorrent::from_bytes(t.torrent.clone()),
                Some(AddTorrentOptions {
                    output_folder: Some(out.to_string_lossy().into()),
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), h.wait_until_completed())
            .await
            .unwrap()
            .unwrap();
    }
}

fn authored(name: &str, seconds: u32) -> (TorrentFixture, Vec<(String, Vec<u8>)>) {
    let files = vec![(format!("{name}.mkv"), mkv(seconds))];
    (arr::fixture::build_torrent(name, &files), files)
}

// ------------------------------------------------------------------ the app

struct Paths {
    state: PathBuf,
    downloads: PathBuf,
    movies: PathBuf,
    tv: PathBuf,
}

fn config_text(
    p: &Paths,
    mock: SocketAddr,
    seeder: Option<SocketAddr>,
    execution: bool,
    dht: bool,
) -> String {
    let peers = seeder
        .map(|s| format!("initial_peers = [\"{s}\"]\n"))
        .unwrap_or_default();
    format!(
        r#"[server]
listen = "127.0.0.1:8787"
api_token_file = "secrets/api_token"
[state]
dir = "."
[[indexer]]
id = 1
name = "The Pirate Bay"
kind = "thepiratebay"
base_url = "http://{mock}/tpb/"
min_interval_ms = 0
cache_ttl_secs = 0
timeout_secs = 5
[[indexer]]
id = 2
name = "Torznab Mock"
kind = "torznab"
base_url = "http://{mock}/tz/"
api_key_file = "secrets/tz_key"
min_interval_ms = 0
cache_ttl_secs = 0
timeout_secs = 5
[tvmaze]
enabled = true
base_url = "http://{mock}/tvmaze"
[tmdb]
enabled = true
base_url = "http://{mock}/tmdb"
credential_file = "secrets/tmdb"
[execution]
enabled = {execution}
download_cleanup = true
[embedded]
download_dir = "{dl}"
listen = "127.0.0.1:0"
trackers = false
dht = {dht}
local_discovery = false
{peers}[migration]
mode = "coexistence"
[library]
movies = "{movies}"
tv = "{tv}"
sample_detection = false
probe_timeout_secs = 30
[automation]
poll_seconds = 3600
[plex]
base_url = "http://{mock}/plex"
token_file = "secrets/plex"
movie_section = 1
tv_section = 2
movies_path = "/media_library/movies"
tv_path = "/media_library/tvshows"
[mcp]
enabled = true
token_file = "secrets/mcp_token"
"#,
        dl = p.downloads.display(),
        movies = p.movies.display(),
        tv = p.tv.display(),
    )
}

const SETUP_DOC: &str = r#"
[[size_limits]]
media = "both"
quality = "webdl_1080p"
min_mib_per_minute = 0.0

[download]
movie_category = "um-movies"
tv_category = "um-tv"
"#;

fn cli(args: &[&str]) -> (bool, Value, String) {
    let out = std::process::Command::new(BIN)
        .args(args)
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (
        out.status.success(),
        serde_json::from_str(&stdout).unwrap_or(Value::Null),
        format!("{stdout}\n{stderr}"),
    )
}

struct Running {
    state: Arc<api::AppState>,
    base: String,
    server: tokio::task::JoinHandle<()>,
    client: reqwest::Client,
    token: String,
}

async fn serve(dir: &Path) -> Running {
    let cfg = config::load(&dir.join("config.toml")).unwrap();
    let state = Arc::new(api::AppState::new(cfg).unwrap());
    arr::worker::recover(&state, time::OffsetDateTime::now_utc()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (app, _stop) = api::app(state.clone()).unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let token = std::fs::read_to_string(dir.join("secrets/api_token"))
        .unwrap()
        .trim()
        .to_string();
    Running {
        state,
        base: format!("http://{addr}"),
        server,
        client: reqwest::Client::new(),
        token,
    }
}

impl Running {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value, String) {
        let mut rb = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let r = rb.send().await.unwrap();
        let s = r.status().as_u16();
        let t = r.text().await.unwrap();
        (s, serde_json::from_str(&t).unwrap_or(Value::Null), t)
    }

    async fn get(&self, path: &str) -> (u16, Value, String) {
        self.call(reqwest::Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value, String) {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }

    async fn tick(&self) {
        arr::worker::tick(&self.state, time::OffsetDateTime::now_utc())
            .await
            .unwrap();
    }

    fn grab(&self, id: &str) -> arr::downloads::Grab {
        let id = id.to_string();
        self.state
            .store
            .blocking(|c| arr::downloads::get(c, &id))
            .unwrap()
            .unwrap()
    }

    async fn until_imported(&self, id: &str) {
        let started = Instant::now();
        loop {
            self.tick().await;
            let g = self.grab(id);
            if g.state == "imported" {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(90),
                "grab {id}: state {} client {:?} error {:?}",
                g.state,
                g.client_state,
                g.last_error
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn stop(self) {
        if let Some(e) = self.state.client.as_deref().and_then(|c| c.embedded()) {
            e.shutdown().await;
        }
        self.server.abort();
        drop(self.state);
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

fn no_leaks(body: &str) {
    for s in [
        TZ_KEY, PLEX_TOKEN, TMDB_KEY, MCP_TOKEN, "magnet:", "btih", "/tz/dl/",
    ] {
        assert!(
            !body.contains(s),
            "response leaked {s:?}: {}",
            &body[..body.len().min(300)]
        );
    }
}

fn write_secret(dir: &Path, name: &str, value: &str) {
    fsutil::write_new_private_file(&dir.join("secrets").join(name), value.as_bytes()).unwrap();
}

/// A fresh state made by the real CLI (`init`, then the test's config), with secrets.
fn fresh(root: &Path, mock: SocketAddr, seeder: Option<SocketAddr>, dht: bool) -> Paths {
    let p = Paths {
        state: root.join("state"),
        downloads: root.join("downloads"),
        movies: root.join("library/movies"),
        tv: root.join("library/tv"),
    };
    for d in [&p.movies, &p.tv] {
        std::fs::create_dir_all(d).unwrap();
    }
    let (ok, v, text) = cli(&["init", "--state-dir", p.state.to_str().unwrap()]);
    assert!(ok, "{text}");
    assert!(v["next"][1].as_str().unwrap().contains("arr setup"), "{v}");
    // The generated template is the standalone one and loads as is.
    let template = std::fs::read_to_string(p.state.join("config.toml")).unwrap();
    assert!(
        template.contains("kind = \"thepiratebay\"") && template.contains("dht = true"),
        "{template}"
    );
    config::load(&p.state.join("config.toml")).unwrap();
    fsutil::atomic_replace_private(
        &p.state.join("config.toml"),
        config_text(&p, mock, seeder, false, dht).as_bytes(),
    )
    .unwrap();
    for (n, v) in [
        ("tz_key", TZ_KEY),
        ("plex", PLEX_TOKEN),
        ("tmdb", TMDB_KEY),
        ("mcp_token", MCP_TOKEN),
    ] {
        write_secret(&p.state, n, v);
    }
    p
}

fn enable_execution(p: &Paths, mock: SocketAddr, seeder: Option<SocketAddr>, dht: bool) {
    fsutil::atomic_replace_private(
        &p.state.join("config.toml"),
        config_text(p, mock, seeder, true, dht).as_bytes(),
    )
    .unwrap();
}

// ------------------------------------------------------------------ tests

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_standalone_deployment_end_to_end_with_native_indexers_only() {
    let (mock_addr, mock) = start_mock().await;
    let base = format!("http://{mock_addr}");
    let tmp = tempfile::tempdir().unwrap();
    let seeder = Seeder::start(&tmp.path().join("seeder")).await;
    let p = fresh(tmp.path(), mock_addr, Some(seeder.addr()), false);

    // --- The existing library (owned before this server existed), as Plex reports it.
    let menu_rel = "The Menu (2022)/The.Menu.2022.1080p.WEB-DL.x264-OLD.mkv";
    std::fs::create_dir_all(p.movies.join("The Menu (2022)")).unwrap();
    std::fs::write(p.movies.join(menu_rel), mkv(3)).unwrap();
    std::fs::create_dir_all(p.tv.join("Slow Horses/Season 1")).unwrap();
    for n in [1, 2] {
        std::fs::write(
            p.tv.join(format!(
                "Slow Horses/Season 1/Slow.Horses.S01E0{n}.1080p.WEB-DL.x264-OLD.mkv"
            )),
            mkv(3),
        )
        .unwrap();
    }
    {
        let mut w = mock.lock().unwrap();
        w.plex_movies = vec![
            json!({ "ratingKey": "101", "type": "movie", "title": "The Menu", "year": 2022, "duration": 6420000,
                "Guid": [{ "id": "imdb://tt9764362" }, { "id": "tmdb://593643" }],
                "Media": [{ "Part": [{ "file": format!("/media_library/movies/{menu_rel}"), "size": 1 }] }] }),
            // Only an IMDB id: TMDB's find resolves it; the file is not on this host.
            json!({ "ratingKey": "102", "type": "movie", "title": "Elsewhere", "year": 2001,
                "Guid": [{ "id": "imdb://tt0000102" }],
                "Media": [{ "Part": [{ "file": "/media_library/movies/Elsewhere (2001)/Elsewhere.mkv" }] }] }),
            // Only an IMDB id TMDB does not know: reported unresolved, nothing invented.
            json!({ "ratingKey": "103", "type": "movie", "title": "Unknown Film", "year": 1990,
                "Guid": [{ "id": "imdb://tt0000103" }], "Media": [] }),
        ];
        w.plex_shows = vec![
            json!({ "ratingKey": "201", "type": "show", "title": "Slow Horses", "year": 2022,
            "Guid": [{ "id": "tvdb://372264" }, { "id": "tmdb://95480" }, { "id": "imdb://tt5875444" }] }),
        ];
        let leaf = |season: u32, n: u32, file: &str| {
            json!({ "ratingKey": format!("3{season}{n}"), "type": "episode",
            "parentIndex": season, "index": n, "title": format!("E{n}"),
            "Media": [{ "Part": [{ "file": format!("/media_library/tvshows/Slow Horses/{file}") }] }] })
        };
        w.plex_leaves.insert(
            "201".into(),
            vec![
                leaf(
                    1,
                    1,
                    "Season 1/Slow.Horses.S01E01.1080p.WEB-DL.x264-OLD.mkv",
                ),
                leaf(
                    1,
                    2,
                    "Season 1/Slow.Horses.S01E02.1080p.WEB-DL.x264-OLD.mkv",
                ),
                leaf(0, 1, "Specials/Slow.Horses.S00E01.mkv"),
            ],
        );
        w.tmdb
            .insert("593643".into(), tmdb_details(593643, "The Menu", 2022, 107));
        w.tmdb
            .insert("603".into(), tmdb_details(603, "The Matrix", 1999, 136));
        w.tmdb
            .insert("1102".into(), tmdb_details(1102, "Elsewhere", 2001, 90));
        w.tmdb_find.insert("tt0000102".into(), 1102);
        let (show, eps) = tvmaze_slow_horses();
        w.tvmaze_shows.insert(9001, show);
        w.tvmaze_episodes.insert(9001, eps);
    }

    // --- `setup` (CLI): managed settings and an empty catalog, no snapshot. Execution stays off.
    let setup_file = tmp.path().join("setup.toml");
    std::fs::write(&setup_file, SETUP_DOC).unwrap();
    let cfg_path = p.state.join("config.toml");
    let (ok, v, text) = cli(&[
        "setup",
        "--config",
        cfg_path.to_str().unwrap(),
        "--settings",
        setup_file.to_str().unwrap(),
        "--dry-run",
    ]);
    assert!(ok, "{text}");
    assert_eq!(v["status"], "dry_run");
    assert!(
        !p.state.join("active-import.json").exists(),
        "a dry run writes nothing"
    );
    let (ok, v, text) = cli(&[
        "setup",
        "--config",
        cfg_path.to_str().unwrap(),
        "--settings",
        setup_file.to_str().unwrap(),
    ]);
    assert!(ok, "{text}");
    assert_eq!(v["status"], "created", "{v}");
    assert_eq!(v["preflight"]["secret_files_missing"], json!([]), "{v}");
    assert_eq!(v["preflight"]["ffprobe"]["status"], "ok", "{v}");
    assert_eq!(v["preflight"]["execution_enabled"], false);
    let ids: Vec<&str> = v["indexers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["media"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 4, "two indexers, movies and TV each: {v}");
    let import_id = v["import_id"].as_str().unwrap().to_string();
    let (ok, again, _) = cli(&[
        "setup",
        "--config",
        cfg_path.to_str().unwrap(),
        "--settings",
        setup_file.to_str().unwrap(),
    ]);
    assert!(ok && again["status"] == "unchanged" && again["import_id"] == import_id.as_str());
    no_leaks(&text);
    let (ok, v, text) = cli(&["check", "--config", cfg_path.to_str().unwrap()]);
    assert!(ok, "{text}");
    assert_eq!(v["search_backend"], "native");
    assert_eq!(v["prowlarr_configured"], false);
    assert_eq!(v["qbittorrent_configured"], false);
    assert_eq!(v["active_import"]["origin"], "managed_setup");
    assert!(
        v["indexer_mapping"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["status"] == "usable"),
        "{v}"
    );

    // --- Shadow mode first: discovery and the read-only bootstrap (execution is off).
    let app = serve(&p.state).await;
    let (s, v, text) = app.get("/api/v1/setup").await;
    assert_eq!(s, 200, "{text}");
    assert_eq!(v["settings"]["origin"], "managed_setup");
    assert_eq!(v["search_backend"], "native");
    assert_eq!(v["execution_enabled"], false);
    let (s, v, text) = app.get("/api/v1/setup/plex").await;
    assert_eq!(s, 200, "{text}");
    no_leaks(&text);
    let mapping = v["mapping"].as_array().unwrap();
    assert!(mapping.iter().all(|m| m["status"] == "ok"), "{v}");
    assert_eq!(mapping[0]["covered_by"], "/media_library/movies");
    let (s, v, text) = app
        .post("/api/v1/setup/plex/bootstrap", json!({ "dry_run": true }))
        .await;
    assert_eq!(s, 200, "{text}");
    assert_eq!(v["runs"][0]["dry_run"], true);
    assert!(
        app.state.index().movie(593643).is_none(),
        "a dry run records nothing"
    );
    let mut done = false;
    for _ in 0..20 {
        let (s, v, text) = app
            .post("/api/v1/setup/plex/bootstrap", json!({ "batch": 2 }))
            .await;
        assert_eq!(s, 200, "{text}");
        no_leaks(&text);
        if v["done"] == true {
            done = true;
            let movies = &v["runs"][0];
            assert_eq!(movies["status"], "done", "{v}");
            assert_eq!(movies["counts"]["titles_created"], 1, "{v}");
            assert_eq!(movies["counts"]["unresolved"], 1, "{v}");
            assert_eq!(movies["counts"]["missing_locally"], 1, "{v}");
            let tv = &v["runs"][1];
            assert_eq!(tv["counts"]["episodes_owned"], 2, "{v}");
            assert_eq!(tv["counts"]["unsupported"], 1, "the special: {v}");
            break;
        }
    }
    assert!(done, "the bootstrap finishes");
    let index = app.state.index();
    let menu = index
        .movie(593643)
        .expect("the owned movie is in the catalog");
    assert!(menu.has_file && !menu.monitored, "owned, not monitored");
    assert_eq!(menu.file.as_ref().unwrap().path.as_deref(), Some(menu_rel));
    assert_eq!(menu.folder.as_deref(), Some("The Menu (2022)"));
    let horses = index.series(arr::ids::SeriesKey::Tvdb(372264)).unwrap();
    assert!(!horses.monitored);
    let eps = horses
        .episodes
        .as_ref()
        .expect("the full TVmaze episode list");
    assert_eq!(eps.len(), 6);
    let owned: Vec<u16> = eps
        .iter()
        .filter(|e| e.has_file)
        .map(|e| e.number)
        .collect();
    assert_eq!(
        owned,
        [1, 2],
        "exactly the episodes Plex numbers, nothing inferred"
    );
    assert!(
        mock.lock().unwrap().tpb_queries.is_empty(),
        "the bootstrap searches nothing"
    );
    let (_, q, _) = app.get("/api/v1/queue?all=true").await;
    assert_eq!(q["total"], 0, "nothing queued");
    app.stop().await;

    // --- The operator enables execution deliberately.
    enable_execution(&p, mock_addr, Some(seeder.addr()), false);
    let app = serve(&p.state).await;
    let (s, v, _) = app.get("/api/v1/readiness").await;
    assert_eq!(s, 200);
    let check = |name: &str| {
        v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .cloned()
            .unwrap()
    };
    assert_eq!(check("search_backend")["status"], "ok", "{v}");
    assert_eq!(
        check("peer_discovery")["status"],
        "ok",
        "initial peers: {v}"
    );
    assert_eq!(check("prowlarr")["status"], "not_configured");

    // An owned title is not downloaded again when it is requested.
    let (s, v, text) = app
        .post(
            "/api/v1/library",
            json!({ "media_id": "movie:tmdb:593643" }),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(v["plan"]["state"], "already_satisfied", "{v}");

    // --- A movie: TMDB metadata, native search, embedded download, validated import.
    let (matrix, matrix_files) = authored("The.Matrix.1999.1080p.WEB-DL.x264-UM", 4);
    seeder.seed(&matrix, &matrix_files).await;
    mock.lock().unwrap().tpb_results.insert(
        "The Matrix 1999".into(),
        json!([tpb_row(5001, &matrix, 207, 1_700_000_000)]),
    );
    let (s, v, text) = app
        .post("/api/v1/library", json!({ "media_id": "movie:tmdb:603" }))
        .await;
    assert_eq!(s, 201, "{text}");
    no_leaks(&text);
    let plan = &v["plan"];
    assert_eq!(plan["state"], "download_queued", "{v}");
    let comp = &plan["search"]["completeness"];
    assert_eq!(
        comp["provider"], "complete",
        "both native indexers answered: {v}"
    );
    let queries = plan["search"]["queries"].as_array().unwrap();
    assert!(
        queries
            .iter()
            .any(|q| q["indexer"] == "Torznab Mock" && q["empty"] == "confirmed"),
        "{v}"
    );
    let grab_id = plan["execution"]["grabs"][0].as_str().unwrap().to_string();
    {
        let w = mock.lock().unwrap();
        assert!(
            w.tpb_queries
                .contains(&"q=The%20Matrix%201999&cat=201,202,207,209,210,211".to_string()),
            "raw TPB query with %20 spaces and a literal comma category list: {:?}",
            w.tpb_queries
        );
        assert!(
            w.tz_queries.iter().any(|q| q.contains("t=caps"))
                && w.tz_queries.iter().any(|q| q.contains("t=search"))
        );
    }
    let g = app.grab(&grab_id);
    assert_eq!(g.info_hash, matrix.info_hash);
    assert_eq!(g.category, "um-movies");
    assert_eq!(g.seed_ratio, Some(1.0), "the setup's explicit seed goal");
    app.until_imported(&grab_id).await;
    let imported = p
        .movies
        .join("The Matrix (1999)/The.Matrix.1999.1080p.WEB-DL.x264-UM.mkv");
    assert!(playable(&imported).await, "a validated, playable import");
    for _ in 0..5 {
        app.tick().await;
    }
    assert!(
        mock.lock()
            .unwrap()
            .plex_refreshes
            .contains(&"1:/media_library/movies/The Matrix (1999)".to_string()),
        "Plex refreshes the imported folder: {:?}",
        mock.lock().unwrap().plex_refreshes
    );

    // --- A season of a bootstrapped show: only the missing episodes are wanted.
    let (e3, e3_files) = authored("Slow.Horses.S01E03.1080p.WEB-DL.x264-UM", 3);
    seeder.seed(&e3, &e3_files).await;
    {
        let mut w = mock.lock().unwrap();
        w.torrents.insert(e3.info_hash.clone(), e3.torrent.clone());
        w.tz_items
            .insert("Slow Horses S01E03".into(), tz_item(&e3, &base, 5040));
    }
    let (s, v, text) = app
        .post(
            "/api/v1/library",
            json!({ "media_id": "season:tvdb:372264:1" }),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    no_leaks(&text);
    let grabs = v["plan"]["execution"]["grabs"].as_array().unwrap().clone();
    assert_eq!(grabs.len(), 1, "{v}");
    let e3_grab = grabs[0].as_str().unwrap().to_string();
    let g = app.grab(&e3_grab);
    assert_eq!(g.info_hash, e3.info_hash);
    let covered: Vec<String> = g.targets.iter().map(|t| t.target.to_string()).collect();
    assert_eq!(
        covered,
        ["episode:tvdb:372264:1:3"],
        "owned episodes are never re-grabbed"
    );
    {
        let w = mock.lock().unwrap();
        let fetch = w.tz_downloads.last().expect("the .torrent was fetched");
        assert!(
            fetch.contains(&format!("apikey={TZ_KEY}")),
            "the key is added for the endpoint's own origin"
        );
    }
    app.until_imported(&e3_grab).await;
    assert!(
        playable(
            &p.tv
                .join("Slow Horses/Season 1/Slow.Horses.S01E03.1080p.WEB-DL.x264-UM.mkv")
        )
        .await
    );

    // --- RSS: the TPB recent feed carries the next episode; it is picked up directly.
    let (e4, e4_files) = authored("Slow.Horses.S01E04.1080p.WEB-DL.x264-UM", 3);
    seeder.seed(&e4, &e4_files).await;
    let recent = time::OffsetDateTime::now_utc().unix_timestamp() - 600;
    mock.lock().unwrap().tpb_recent = vec![
        tpb_row(6001, &e4, 208, recent),
        // Unrelated categories in the shared window are dropped, not matched.
        json!({ "id": "6002", "name": "Some Album 2026 FLAC", "info_hash": "a".repeat(40), "seeders": "5", "leechers": "0",
            "size": "100", "username": "x", "added": recent.to_string(), "category": "104" }),
    ];
    let report = arr::rss::sync(&app.state, time::OffsetDateTime::now_utc(), true)
        .await
        .unwrap();
    assert_eq!(report.grabbed, 1, "{report:?}");
    assert_eq!(
        report.provider_completeness, "complete_window",
        "{report:?}"
    );
    let tpb = report.indexers.iter().find(|i| i.indexer_id == 1).unwrap();
    assert_eq!(
        (tpb.status.as_str(), tpb.window, tpb.dropped),
        ("ok", Some(2), 1),
        "{report:?}"
    );
    assert!(mock.lock().unwrap().tpb_recent_hits >= 1);
    let e4_grab = app
        .state
        .store
        .blocking(|c| {
            Ok(c.query_row(
                "SELECT id FROM grabs WHERE title LIKE '%S01E04%'",
                [],
                |r| r.get::<_, String>(0),
            )?)
        })
        .unwrap();
    app.until_imported(&e4_grab).await;

    let (_, d, text) = app.get("/api/v1/diagnostics").await;
    no_leaks(&text);
    assert_eq!(d["search_backend"]["backend"], "native");
    assert_eq!(
        d["search_backend"]["indexers"][0]["health"]["status"], "ok",
        "{d}"
    );
    assert_eq!(d["search_backend"]["indexers"][1]["api_key"], true);

    // --- Restart: the managed settings, catalog and downloads persist.
    app.stop().await;
    let app = serve(&p.state).await;
    let l = app.state.current().unwrap();
    assert_eq!(
        (l.origin, l.import_id.as_str()),
        ("managed_setup", import_id.as_str())
    );
    let index = app.state.index();
    assert!(index.movie(603).unwrap().has_file && index.movie(593643).unwrap().has_file);
    let owned: Vec<u16> = index
        .series(arr::ids::SeriesKey::Tvdb(372264))
        .unwrap()
        .episodes
        .iter()
        .flatten()
        .filter(|e| e.has_file)
        .map(|e| e.number)
        .collect();
    assert_eq!(owned, [1, 2, 3, 4]);
    assert_eq!(app.grab(&grab_id).state, "imported");
    app.stop().await;

    // --- Nothing else was ever contacted: no Prowlarr, qBittorrent, Sonarr or Radarr endpoint.
    let w = mock.lock().unwrap();
    assert!(
        w.forbidden.is_empty(),
        "unexpected upstream requests: {:?}",
        w.forbidden
    );
}

// ------------------------------------------------------------------ focused tests

/// A fresh standalone state with managed settings created through the API.
async fn api_setup(
    root: &Path,
    mock: SocketAddr,
    execution: bool,
    seeder: Option<SocketAddr>,
    dht: bool,
) -> (Paths, Running) {
    let p = fresh(root, mock, seeder, dht);
    if execution {
        enable_execution(&p, mock, seeder, dht);
    }
    let app = serve(&p.state).await;
    let doc: toml::Table = toml::from_str(SETUP_DOC).unwrap();
    let (s, v, text) = app.post("/api/v1/setup", json!({ "document": doc })).await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(v["status"], "created");
    (p, app)
}

#[tokio::test]
async fn native_search_reports_partial_failure_false_empties_and_typed_errors() {
    let (mock_addr, mock) = start_mock().await;
    let base = format!("http://{mock_addr}");
    let tmp = tempfile::tempdir().unwrap();
    let (_p, app) = api_setup(tmp.path(), mock_addr, false, None, false).await;
    let (matrix, _) = authored("The.Matrix.1999.1080p.WEB-DL.x264-UM", 2);
    {
        let mut w = mock.lock().unwrap();
        for (id, title, year) in [
            (603, "The Matrix", 1999),
            (438631, "Dune", 2021),
            (550, "Fight Club", 1999),
        ] {
            w.tmdb
                .insert(id.to_string(), tmdb_details(id, title, year, 120));
        }
        // TPB fails; the Torznab endpoint answers.
        w.tpb_status.insert("The Matrix 1999".into(), 500);
        w.tz_items
            .insert("The Matrix 1999".into(), tz_item(&matrix, &base, 2040));
        // TPB answers an empty array without its "no results" row: not a confirmed empty.
        w.tpb_results.insert("Dune 2021".into(), json!([]));
        // Both fail: TPB with HTTP 500, Torznab with its own credential error.
        w.tpb_status.insert("Fight Club 1999".into(), 500);
        w.tz_errors.insert("Fight Club 1999".into(), "100".into());
    }
    let (s, v, text) = app.get("/api/v1/media/movie:tmdb:603/releases").await;
    assert_eq!(s, 200, "{text}");
    no_leaks(&text);
    assert_eq!(
        v["counts"]["unique"], 1,
        "the Torznab result survives the TPB failure: {v}"
    );
    assert_eq!(v["completeness"]["retrieval"], "incomplete");
    assert_eq!(v["completeness"]["provider"], "incomplete");
    let issue = &v["completeness"]["issues"][0];
    assert_eq!(
        (issue["kind"].as_str(), issue["indexer"].as_str()),
        (Some("query_failed"), Some("The Pirate Bay")),
        "{v}"
    );
    assert!(
        issue["detail"]
            .as_str()
            .unwrap()
            .contains("indexer 1 (The Pirate Bay)"),
        "{v}"
    );

    let (s, v, text) = app.get("/api/v1/media/movie:tmdb:438631/releases").await;
    assert_eq!(s, 200, "{text}");
    assert_eq!(v["outcome"], "observed_empty");
    assert_eq!(v["completeness"]["retrieval"], "complete");
    assert_eq!(
        v["completeness"]["provider"], "incomplete",
        "an unconfirmed empty is not proof: {v}"
    );
    assert_eq!(v["completeness"]["issues"][0]["kind"], "empty_unconfirmed");
    mock.lock().unwrap().tpb_results.remove("Dune 2021");
    let (_, v, _) = app
        .get("/api/v1/media/movie:tmdb:438631/releases?refresh=true")
        .await;
    assert_eq!(
        v["completeness"]["provider"], "complete",
        "both said 'no results': {v}"
    );
    assert!(
        v["queries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|q| q["empty"] == "confirmed"),
        "{v}"
    );

    let (s, v, text) = app.get("/api/v1/media/movie:tmdb:550/releases").await;
    assert_eq!(s, 502, "every indexer failed: {text}");
    no_leaks(&text);
    assert_eq!(
        v["error"]["details"]["provider"],
        "indexer 1 (The Pirate Bay)"
    );
    // The Torznab endpoint's own description (which quotes the key here) is never repeated.
    assert!(!text.contains("apikey="));
    // The API's view of the Torznab failure is typed.
    mock.lock().unwrap().tpb_status.remove("Fight Club 1999");
    let (s, v, text) = app
        .get("/api/v1/media/movie:tmdb:550/releases?refresh=true")
        .await;
    assert_eq!(s, 200, "{text}");
    let tz = v["queries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|q| q["indexer"] == "Torznab Mock")
        .unwrap()
        .clone();
    assert_eq!(tz["status"], "failed");
    assert_eq!(tz["error"]["code"], "upstream_auth", "{tz}");
    app.stop().await;
    assert!(mock.lock().unwrap().forbidden.is_empty());
}

fn link(
    hash: &str,
    magnet: Option<String>,
    download: Option<String>,
    indexer: i64,
) -> arr::downloads::ReleaseLink {
    let id = arr::ids::MediaId::Movie { tmdb: 603 };
    arr::downloads::ReleaseLink {
        release_ref: format!("rel_{}", &hash[..24]),
        target: id,
        media_id: id,
        info_hash: Some(hash.to_string()),
        magnet,
        download_url: download,
        title: "The.Matrix.1999.1080p.WEB-DL.x264-UM".into(),
        size: Some(50 * 1024 * 1024),
        indexer_id: Some(indexer),
        quality: arr::quality::Quality::Webdl1080,
        revision: arr::release::Revision {
            version: 1,
            real: 0,
        },
        full_season: false,
        covers: vec![id],
        replaces: vec![],
        interactive: true,
    }
}

async fn enqueue(
    app: &Running,
    l: arr::downloads::ReleaseLink,
) -> Result<String, (String, String)> {
    use arr::downloads::{EnqueueError, EnqueueRequest};
    match arr::downloads::enqueue(
        &app.state,
        EnqueueRequest {
            link: l,
            plan_id: None,
            origin: "interactive",
        },
    )
    .await
    {
        Ok(g) => Ok(g.id),
        Err(EnqueueError::Refused(code, msg)) => Err((code.to_string(), msg)),
        Err(EnqueueError::Upstream(u)) => Err(("upstream".into(), u.describe())),
        Err(EnqueueError::Internal(e)) => panic!("{e:#}"),
    }
}

#[tokio::test]
async fn torznab_credentials_never_leave_their_origin_and_downloads_are_confined() {
    let (mock_addr, mock) = start_mock().await;
    let base = format!("http://{mock_addr}");
    let tmp = tempfile::tempdir().unwrap();
    // DHT on (public torrents): payloads that pass every check are queued.
    let (_p, app) = api_setup(tmp.path(), mock_addr, true, None, true).await;
    mock.lock()
        .unwrap()
        .tmdb
        .insert("603".into(), tmdb_details(603, "The Matrix", 1999, 136));
    let (s, _, text) = app
        .post(
            "/api/v1/library",
            json!({ "media_id": "movie:tmdb:603", "search": false }),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    let hash = "c12fe1c06bba254a9dc9f519b335aa7c1367a88a";
    let refused = |r: Result<String, (String, String)>, needle: &str| {
        let (code, msg) = r.expect_err("refused");
        assert!(msg.contains(needle), "{code}: {msg}");
        assert!(
            !msg.contains(TZ_KEY) && !msg.contains("127.0.0.2"),
            "errors never quote a link or the key: {msg}"
        );
    };
    // A download link on an origin the operator did not configure is never fetched.
    refused(
        enqueue(
            &app,
            link(hash, None, Some("http://127.0.0.2:9/x.torrent".into()), 2),
        )
        .await,
        "not on the indexer's origin",
    );
    // URL credentials are refused.
    refused(
        enqueue(
            &app,
            link(
                hash,
                None,
                Some(format!("http://user:pw@{mock_addr}/tz/dl/{hash}")),
                2,
            ),
        )
        .await,
        "credentials",
    );
    // A same-origin proxy redirecting elsewhere is refused at the redirect.
    refused(
        enqueue(
            &app,
            link(hash, None, Some(format!("{base}/tz/go/foreign")), 2),
        )
        .await,
        "not on the indexer's origin",
    );
    // A redirect to a magnet whose tracker would carry the key to a third party is refused.
    refused(
        enqueue(
            &app,
            link(hash, None, Some(format!("{base}/tz/go/magnet_leak")), 2),
        )
        .await,
        "API key to another origin",
    );
    // A .torrent announcing the key to a foreign tracker is refused, not stripped.
    let mut t = b"d8:announce".to_vec();
    let ann = format!("https://tracker.example/announce?passkey={TZ_KEY}");
    t.extend_from_slice(format!("{}:{ann}", ann.len()).as_bytes());
    t.extend_from_slice(b"4:infod6:lengthi1e4:name36:The.Matrix.1999.1080p.WEB-DL.x264-UM12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee");
    let leak_hash = arr::torrent::info_hash(&t).unwrap();
    mock.lock()
        .unwrap()
        .torrents
        .insert("announce_leak".into(), t);
    refused(
        enqueue(
            &app,
            link(
                &leak_hash,
                None,
                Some(format!("{base}/tz/go/announce_leak")),
                2,
            ),
        )
        .await,
        "API key to another origin",
    );
    // The key was sent only to the endpoint's own origin.
    {
        let w = mock.lock().unwrap();
        assert!(
            w.tz_downloads.is_empty(),
            "no /tz/dl fetch happened: {:?}",
            w.tz_downloads
        );
    }
    // A feed item linking off-origin or leaking the key in a magnet keeps no link at all.
    let leaky = format!(
        "magnet:?xt=urn:btih:{hash}&amp;tr=https%3A%2F%2Ftracker.example%2Fa%3Fk%3D{TZ_KEY}"
    );
    mock.lock().unwrap().tz_items.insert(
        "The Matrix 1999".into(),
        format!(
            r#"<item><title>The.Matrix.1999.1080p.WEB-DL.x264-UM</title><guid>a</guid><link>http://127.0.0.2:9/a.torrent</link><torznab:attr name="seeders" value="9"/></item><item><title>The.Matrix.1999.1080p.WEB-DL.x264-UM2</title><guid>b</guid><torznab:attr name="magneturl" value="{leaky}"/><torznab:attr name="seeders" value="9"/></item>"#
        ),
    );
    let (s, v, text) = app.get("/api/v1/media/movie:tmdb:603/releases").await;
    assert_eq!(s, 200, "{text}");
    no_leaks(&text);
    let tz = v["queries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|q| q["indexer"] == "Torznab Mock")
        .unwrap()
        .clone();
    assert_eq!(tz["dropped_results"], 2, "{tz}");
    let (s, g, text) = app
        .post(
            "/api/v1/grabs",
            json!({ "media_id": "movie:tmdb:603", "release_ref": v["releases"][0]["release_ref"] }),
        )
        .await;
    assert_ne!(
        s, 201,
        "an item without a usable link is never grabbed: {text}"
    );
    let _ = g;
    app.stop().await;
    assert!(mock.lock().unwrap().forbidden.is_empty());
}

#[tokio::test]
async fn trackerless_magnets_without_peer_discovery_are_refused_not_queued() {
    let (mock_addr, mock) = start_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    // DHT off, no initial peers, trackerless TPB magnets: nothing could ever find a peer.
    let (p, app) = api_setup(tmp.path(), mock_addr, true, None, false).await;
    let (matrix, _) = authored("The.Matrix.1999.1080p.WEB-DL.x264-UM", 2);
    {
        let mut w = mock.lock().unwrap();
        w.tmdb
            .insert("603".into(), tmdb_details(603, "The Matrix", 1999, 136));
        w.tpb_results.insert(
            "The Matrix 1999".into(),
            json!([tpb_row(5001, &matrix, 207, 1_700_000_000)]),
        );
    }
    let (_, v, _) = app.get("/api/v1/readiness").await;
    let peer = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "peer_discovery")
        .unwrap()
        .clone();
    assert_eq!(peer["status"], "blocked", "{peer}");
    assert!(peer["message"].as_str().unwrap().contains("dht = true"));
    let (s, v, text) = app
        .post("/api/v1/library", json!({ "media_id": "movie:tmdb:603" }))
        .await;
    assert_eq!(s, 201, "{text}");
    let ex = &v["plan"]["execution"];
    assert_eq!(ex["outcome"], "nothing_queued", "{v}");
    assert_eq!(ex["refused"][0]["code"], "no_peer_discovery", "{v}");
    let (_, q, _) = app.get("/api/v1/queue?all=true").await;
    assert_eq!(q["total"], 0, "nothing queued that could never download");
    app.stop().await;
    // With trusted public trackers in the magnets (and trackers on), the same grab is queued.
    let text = config_text(&p, mock_addr, None, true, false)
        .replace(
            "timeout_secs = 5\n[[indexer]]",
            "timeout_secs = 5\nmagnet_trackers = [\"udp://127.0.0.1:9/announce\"]\n[[indexer]]",
        )
        .replace("trackers = false", "trackers = true");
    fsutil::atomic_replace_private(&p.state.join("config.toml"), text.as_bytes()).unwrap();
    let app = serve(&p.state).await;
    let (_, v, _) = app.get("/api/v1/readiness").await;
    let peer = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "peer_discovery")
        .unwrap()
        .clone();
    assert_eq!(peer["status"], "ok", "{peer}");
    let (s, v, text) = app
        .post(
            "/api/v1/plans",
            json!({ "media_id": "movie:tmdb:603", "execute": true, "replan": true }),
        )
        .await;
    assert!(s == 200 || s == 201, "{text}");
    assert_eq!(v["plan"]["execution"]["outcome"], "queued", "{v}");
    app.stop().await;
}

#[tokio::test]
async fn provider_backoff_is_honoured_by_queued_requests_and_quotas_are_durable() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new().route(
        "/q.php",
        get(move || {
            let h = h.clone();
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(200)).await;
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", "120")],
                    "slow down",
                )
                    .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let tmp = tempfile::tempdir().unwrap();
    let store = arr::store::Store::open(&tmp.path().join("s.sqlite")).unwrap();
    let mut cfg = arr::config::IndexerConfig {
        id: 1,
        name: "The Pirate Bay".into(),
        kind: arr::config::IndexerKind::ThePirateBay,
        base_url: reqwest::Url::parse(&format!("http://{addr}/")).unwrap(),
        api_key: None,
        api_path: "api".into(),
        download_origins: vec![],
        key_origins: vec![],
        prowlarr_definition: None,
        magnet_trackers: vec![],
        uploaders: vec![],
        recent_feed: true,
        timeout: Duration::from_secs(5),
        max_concurrent: 1,
        min_interval: Duration::ZERO,
        cache_ttl: Duration::ZERO,
        max_response_bytes: 1 << 20,
        max_pages: 1,
        page_size: 100,
        query_limit: None,
        grab_limit: None,
        limits_window: Duration::from_secs(86_400),
    };
    let reg = arr::indexer::Registry::new(std::slice::from_ref(&cfg), store.clone());
    let ix = reg.get(1).unwrap();
    let req = |q: &str| arr::prowlarr::SearchRequest {
        query: q.into(),
        categories: vec![2000],
        indexer_ids: vec![1],
    };
    // Two requests at once: the second waits for the permit while the first is told to wait.
    let (one, two) = (req("one"), req("two"));
    let (a, b) = tokio::join!(ix.search(&one, true), ix.search(&two, true));
    assert_eq!(a.unwrap_err(), arr::upstream::UpstreamError::RateLimited);
    assert!(matches!(
        b.unwrap_err(),
        arr::upstream::UpstreamError::Throttled { .. }
    ));
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the queued request never went out during Retry-After"
    );
    let e = ix.search(&req("three"), true).await.unwrap_err();
    assert!(e.describe().contains("Retry-After"), "{}", e.describe());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(ix.health().status, "backoff");
    // A configured query limit is counted in the state database and survives a restart.
    cfg.id = 2;
    cfg.query_limit = Some(1);
    cfg.base_url = reqwest::Url::parse("http://127.0.0.1:9/").unwrap();
    let reg = arr::indexer::Registry::new(std::slice::from_ref(&cfg), store.clone());
    let first = reg
        .get(2)
        .unwrap()
        .search(&req("x"), true)
        .await
        .unwrap_err();
    assert!(
        !matches!(first, arr::upstream::UpstreamError::Throttled { .. }),
        "the first query is admitted (and fails to connect)"
    );
    let reg = arr::indexer::Registry::new(std::slice::from_ref(&cfg), store);
    let second = reg
        .get(2)
        .unwrap()
        .search(&req("y"), true)
        .await
        .unwrap_err();
    assert!(
        second.describe().contains("query limit (1 per day)"),
        "{}",
        second.describe()
    );
}

#[tokio::test]
async fn managed_setup_api_hot_activation_guards_and_restart() {
    let (mock_addr, mock) = start_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let p = fresh(tmp.path(), mock_addr, None, false);
    let app = serve(&p.state).await;
    let (s, v, _) = app.get("/api/v1/setup").await;
    assert_eq!(s, 200);
    assert!(
        v["settings"].is_null() && v["accepts_managed_setup"] == true,
        "{v}"
    );
    let (s, v, _) = app.get("/api/v1/library/movies").await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (503, Some("catalog_not_imported"))
    );
    let (s, v, text) = app.post("/api/v1/setup", json!({ "dry_run": true })).await;
    assert_eq!((s, v["status"].as_str()), (200, Some("dry_run")), "{text}");
    assert!(app.state.current().is_none(), "a dry run activates nothing");
    let (s, v, text) = app.post("/api/v1/setup", json!({ "document": { "profiles": [{ "id": "x", "media": "movie", "tiers": [["webdl_9000p"]] }], "download": { "initial_state": "later" } } })).await;
    assert_eq!(s, 422, "{text}");
    assert_eq!(v["error"]["code"], "invalid_setup");
    assert_eq!(
        v["error"]["details"]["errors"].as_array().unwrap().len(),
        2,
        "every problem at once: {v}"
    );
    let (s, _, _) = app
        .post("/api/v1/setup", json!({ "document": { "nonsense": 1 } }))
        .await;
    assert_eq!(s, 422, "unknown keys are refused");
    // Defaults: created and active at once, without a restart.
    let (s, v, text) = app.post("/api/v1/setup", json!({})).await;
    assert_eq!(s, 201, "{text}");
    let first = v["import_id"].as_str().unwrap().to_string();
    assert_eq!(v["profiles"].as_array().unwrap().len(), 2);
    assert_eq!(
        v["indexers"][0]["seed_ratio"], 1.0,
        "explicit default seed goal: {v}"
    );
    assert_eq!(v["indexers"][0]["seed_time_minutes"], 4320);
    let (s, v, _) = app.get("/api/v1/settings").await;
    assert_eq!(s, 200);
    assert_eq!(v["import_id"], first.as_str());
    let (s, r, _) = app.get("/api/v1/migration-report").await;
    assert_eq!((s, r["sources"][0]["app"].as_str()), (200, Some("setup")));
    let (s, v, _) = app.post("/api/v1/setup", json!({})).await;
    assert_eq!((s, v["status"].as_str()), (200, Some("unchanged")));
    mock.lock()
        .unwrap()
        .tmdb
        .insert("603".into(), tmdb_details(603, "The Matrix", 1999, 136));
    let (s, _, text) = app
        .post(
            "/api/v1/library",
            json!({ "media_id": "movie:tmdb:603", "search": false }),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(
        app.state
            .index()
            .movie(603)
            .unwrap()
            .quality_profile
            .as_deref(),
        Some("setup:movies-hd")
    );
    // A replacement may not drop what the catalog uses.
    let (s, v, text) = app
        .post("/api/v1/setup", json!({ "document": { "profiles": [{ "id": "movies-uhd", "media": "movie", "tiers": [["webdl_2160p"]] }] } }))
        .await;
    assert_eq!(s, 409, "{text}");
    assert_eq!(v["error"]["code"], "setup_conflict");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("setup:movies-hd")
    );
    // A compatible change is created, activated in place, and replaces the previous one.
    let (s, v, text) = app
        .post(
            "/api/v1/setup",
            json!({ "document": { "tv": { "rss_sync_interval_minutes": 30 } } }),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(v["replaced"], first.as_str());
    let second = v["import_id"].as_str().unwrap().to_string();
    assert_ne!(first, second);
    app.stop().await;
    // Restart: the latest setup is active, verified against its manifest.
    let app = serve(&p.state).await;
    let l = app.state.current().unwrap();
    assert_eq!(
        (l.origin, l.import_id.as_str()),
        ("managed_setup", second.as_str())
    );
    let dir = p.state.join("imports").join(&second);
    assert!(dir.join("source-setup.json").exists() && !dir.join("source-native.json").exists());
    app.stop().await;
    // A tampered import is refused at startup (digest-verified like a snapshot import).
    let settings = dir.join("settings.json");
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&settings).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o600);
        std::fs::set_permissions(&settings, perms).unwrap();
    }
    let mut text = std::fs::read_to_string(&settings).unwrap();
    text = text.replacen("\"seed_ratio\": 1.0", "\"seed_ratio\": 0.0", 1);
    std::fs::write(&settings, text).unwrap();
    let cfg = config::load(&p.state.join("config.toml")).unwrap();
    let err = api::AppState::new(cfg)
        .err()
        .expect("tampering is detected");
    assert!(format!("{err:#}").contains("manifest digest"), "{err:#}");
}

/// The test snapshot with The Pirate Bay indexer as a current Prowlarr export shapes it.
fn snapshot_with_tpb(host: &str, top100: i64, uploader: &str) -> Value {
    let mut v = common::snapshot();
    let field =
        |name: &str, value: Value| json!({ "name": name, "value": value, "privacy": "normal" });
    v["prowlarr"]["indexer"][0] = json!({
        "id": 1, "name": "The Pirate Bay", "enable": true, "protocol": "torrent", "privacy": "public", "priority": 25,
        "implementation": "Cardigann", "definitionName": "thepiratebay", "tags": [],
        "fields": [
            field("definitionFile", json!("thepiratebay")),
            field("baseUrl", json!("https://thepiratebay.org/")),
            field("apiurl", json!(host)),
            field("uploader", json!(uploader)),
            field("top100", json!(top100)),
            field("baseSettings.queryLimit", Value::Null),
            field("baseSettings.grabLimit", Value::Null),
            field("baseSettings.limitsUnit", json!(0)),
            field("torrentBaseSettings.appMinimumSeeders", Value::Null),
            field("torrentBaseSettings.seedRatio", json!(2)),
            field("torrentBaseSettings.seedTime", Value::Null),
            field("torrentBaseSettings.packSeedTime", Value::Null),
            field("torrentBaseSettings.preferMagnetUrl", json!(false)),
            json!({ "name": "cookie", "value": common::FAKE_COOKIE, "privacy": "normal" }),
        ],
    });
    v
}

#[tokio::test]
async fn an_imported_policy_is_served_by_the_native_tpb_provider_or_refused() {
    let (mock_addr, mock) = start_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let p = fresh(
        tmp.path(),
        mock_addr,
        Some("127.0.0.1:9".parse().unwrap()),
        false,
    );
    let import = |snap: Value, name: &str| {
        let f = tmp.path().join(name);
        std::fs::write(&f, serde_json::to_vec(&snap).unwrap()).unwrap();
        let (ok, v, text) = cli(&[
            "import",
            "--state-dir",
            p.state.to_str().unwrap(),
            "--snapshot",
            f.to_str().unwrap(),
        ]);
        assert!(ok, "{text}");
        v
    };
    import(snapshot_with_tpb("127.0.0.1", 6, ""), "native.json");
    enable_execution(&p, mock_addr, Some("127.0.0.1:9".parse().unwrap()), false);
    let app = serve(&p.state).await;
    // The copied source keeps the public field semantics (empty uploader, host, select value)
    // and never a credential.
    let (_, v, text) = app.get("/api/v1/settings").await;
    assert!(
        !text.contains(common::FAKE_COOKIE) && !text.contains("unparseable"),
        "{text}"
    );
    let src = &v["settings"]["indexers"][0]["source"];
    assert_eq!(src["definition"], "thepiratebay");
    assert_eq!(src["settings"]["uploader"], "");
    assert_eq!(src["settings"]["apiurl"], "127.0.0.1");
    assert_eq!(src["settings"]["top100"], 6);
    assert_eq!(src["settings"]["torrentBaseSettings.seedRatio"], 2);
    let (_, v, _) = app.get("/api/v1/readiness").await;
    let sb = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "search_backend")
        .unwrap()
        .clone();
    assert_eq!(sb["status"], "ok", "{sb}");
    assert!(
        sb["settings_mapping"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["status"] == "usable"),
        "{sb}"
    );
    // A title new to the catalog is found through the native provider with the imported id
    // and categories, and its seed goal is the Prowlarr indexer's ratio 2.
    let (fc, _) = authored("Fight.Club.1999.1080p.WEB-DL.x264-UM", 2);
    {
        let mut w = mock.lock().unwrap();
        w.tmdb
            .insert("550".into(), tmdb_details(550, "Fight Club", 1999, 139));
        w.tpb_results.insert(
            "Fight Club 1999".into(),
            json!([tpb_row(7001, &fc, 207, 1_700_000_000)]),
        );
    }
    let (s, v, text) = app
        .post(
            "/api/v1/library",
            json!({ "media_id": "movie:tmdb:550", "quality_profile": "radarr:6" }),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    no_leaks(&text);
    let grab = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("{v}"))
        .to_string();
    let g = app.grab(&grab);
    assert_eq!(
        (g.indexer_id, g.seed_ratio),
        (Some(1), Some(2.0)),
        "imported id and ratio 2 preserved"
    );
    assert!(
        mock.lock()
            .unwrap()
            .tpb_queries
            .iter()
            .any(|q| q == "q=Fight%20Club%201999&cat=201,202,207,209,210,211"),
        "imported movie categories 2000 (parent), 2040, 2045 map to the TPB movie categories: {:?}",
        mock.lock().unwrap().tpb_queries
    );
    app.stop().await;
    // Site settings the native provider does not implement fail closed.
    import(snapshot_with_tpb("127.0.0.1", 1, ""), "top100.json");
    let app = serve(&p.state).await;
    let (s, v, text) = app.get("/api/v1/media/movie:tmdb:550/releases").await;
    assert_eq!(s, 503, "{text}");
    assert_eq!(v["error"]["code"], "no_usable_indexers");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("top100 = 1"),
        "{v}"
    );
    app.stop().await;
    import(
        snapshot_with_tpb("apibay.org", 6, "someone"),
        "uploader.json",
    );
    let app = serve(&p.state).await;
    let (_, v, _) = app.get("/api/v1/readiness").await;
    let sb = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "search_backend")
        .unwrap()
        .clone();
    assert_eq!(sb["status"], "blocked", "{sb}");
    let reason = sb["settings_mapping"][0]["reason"].as_str().unwrap();
    assert!(
        reason.contains("uploader") && reason.contains("API host"),
        "{reason}"
    );
    app.stop().await;
    assert!(mock.lock().unwrap().forbidden.is_empty());
}

#[tokio::test]
async fn remote_mcp_serves_the_same_tools_over_streamable_http_with_its_own_bearer() {
    use rmcp::model::{CallToolRequestParams, ClientConfig, ProtocolVersion};
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    use rmcp::{ClientLifecycleMode, ClientServiceExt, ServiceExt};
    let (mock_addr, mock) = start_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let (_p, app) = api_setup(tmp.path(), mock_addr, false, None, false).await;
    mock.lock()
        .unwrap()
        .tmdb
        .insert("603".into(), tmdb_details(603, "The Matrix", 1999, 136));
    let url = format!("{}/mcp", app.base);
    let transport = |token: &str| {
        StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(url.clone())
                .auth_header(token.to_string()),
        )
    };
    // The initialize handshake, then the seven tools.
    let client = ClientConfig::default()
        .serve(transport(MCP_TOKEN))
        .await
        .expect("initialize with the MCP token");
    let tools: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .iter()
        .map(|t| t.name.to_string())
        .collect();
    assert_eq!(
        tools,
        [
            "search_titles",
            "add_media",
            "media_status",
            "requests",
            "library",
            "release_choices",
            "start_download"
        ]
    );
    let call = |name: &str, args: Value| {
        CallToolRequestParams::new(name.to_string())
            .with_arguments(args.as_object().unwrap().clone())
    };
    let r = client
        .call_tool(call(
            "search_titles",
            json!({ "kind": "movie", "q": "matrix" }),
        ))
        .await
        .unwrap();
    let v = r.structured_content.clone().unwrap();
    assert_eq!(r.is_error, Some(false), "{v}");
    assert_eq!(v["results"][0]["media_id"], "movie:tmdb:603", "{v}");
    // A write through the same contract (shadow mode: a shadow plan, nothing downloaded).
    let r = client
        .call_tool(call("add_media", json!({ "media_id": "movie:tmdb:603" })))
        .await
        .unwrap();
    let v = r.structured_content.clone().unwrap();
    assert_eq!(r.is_error, Some(false), "{v}");
    assert_eq!(v["automatic"]["status"], "execution_disabled", "{v}");
    assert!(
        app.state.index().movie(603).is_some(),
        "the add reached the core"
    );
    let r = client
        .call_tool(call(
            "media_status",
            json!({ "media_id": "movie:tmdb:603" }),
        ))
        .await
        .unwrap();
    assert_eq!(r.is_error, Some(false));
    // Errors keep their typed shape and outcome.
    let r = client
        .call_tool(call("add_media", json!({ "media_id": "series:tvdb:1" })))
        .await
        .unwrap();
    let v = r.structured_content.clone().unwrap();
    assert_eq!(
        (
            r.is_error,
            v["error"]["code"].as_str(),
            v["outcome"].as_str()
        ),
        (Some(true), Some("invalid_arguments"), Some("not_sent"))
    );
    let r = client
        .call_tool(call(
            "start_download",
            json!({ "media_id": "movie:tmdb:603", "release_ref": "rel_000000000000000000000000" }),
        ))
        .await
        .unwrap();
    let v = r.structured_content.clone().unwrap();
    assert_eq!(
        (v["error"]["code"].as_str(), v["outcome"].as_str()),
        (Some("release_not_found"), Some("not_applied")),
        "{v}"
    );
    assert!(
        client
            .call_tool(call("no_such_tool", json!({})))
            .await
            .is_err()
    );
    let _ = client.cancel().await;
    // The stateless discover lifecycle (newest revision) works too.
    let client = ClientConfig::default()
        .serve_with_lifecycle(
            transport(MCP_TOKEN),
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::LATEST],
            },
        )
        .await
        .expect("discover");
    assert_eq!(client.list_all_tools().await.unwrap().len(), 7);
    let _ = client.cancel().await;
    // Authentication comes before initialisation; the API token is not an MCP token.
    assert!(
        ClientConfig::default()
            .serve(transport(&app.token))
            .await
            .is_err(),
        "the API token is refused"
    );
    let raw = reqwest::Client::new();
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "raw", "version": "0" } } });
    let post = |rb: reqwest::RequestBuilder| {
        rb.header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
    };
    let r = post(raw.post(&url))
        .body(init.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    assert_eq!(r.headers().get("www-authenticate").unwrap(), "Bearer");
    let r = post(raw.post(&url).bearer_auth(MCP_TOKEN))
        .body(init.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "a raw initialize with the token");
    assert!(r.text().await.unwrap().contains("\"serverInfo\""));
    let r = post(raw.post(&url).bearer_auth(MCP_TOKEN))
        .header("host", "evil.example")
        .body(init.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 403, "Host is checked (DNS rebinding)");
    let r = post(raw.post(&url).bearer_auth(MCP_TOKEN))
        .header("origin", "https://evil.example")
        .body(init.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status().as_u16(),
        403,
        "a browser Origin is refused unless allowed"
    );
    let r = post(raw.post(&url).bearer_auth(MCP_TOKEN))
        .body("x".repeat(70 * 1024))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 413, "bodies are bounded");
    let r = raw.get(&url).bearer_auth(MCP_TOKEN).send().await.unwrap();
    assert_eq!(
        r.status().as_u16(),
        405,
        "stateless: no standalone SSE stream"
    );
    // The MCP token reaches nothing but MCP.
    let r = raw
        .get(format!("{}/api/v1/readiness", app.base))
        .bearer_auth(MCP_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    app.stop().await;
    assert!(
        mock.lock().unwrap().forbidden.is_empty(),
        "{:?}",
        mock.lock().unwrap().forbidden
    );
}

#[tokio::test]
async fn bootstrap_refuses_unavailable_roots_unmapped_paths_and_native_imports() {
    let (mock_addr, mock) = start_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let (p, app) = api_setup(tmp.path(), mock_addr, false, None, false).await;
    mock.lock().unwrap().plex_shows = vec![
        json!({ "ratingKey": "201", "type": "show", "title": "Slow Horses", "Guid": [{ "id": "tvdb://372264" }] }),
    ];
    // The TV root disappears (unmounted): the run fails and nothing is read or changed.
    std::fs::remove_dir_all(&p.tv).unwrap();
    let (s, v, text) = app
        .post("/api/v1/setup/plex/bootstrap", json!({ "media": "tv" }))
        .await;
    assert_eq!(s, 200, "{text}");
    let run = &v["runs"][0];
    assert_eq!(run["status"], "failed", "{v}");
    assert!(
        run["last_error"]
            .as_str()
            .unwrap()
            .contains("not accessible"),
        "{v}"
    );
    assert!(
        mock.lock().unwrap().plex_pages.is_empty(),
        "Plex was not even enumerated"
    );
    assert!(app.state.index().catalog.series.is_empty());
    // A file outside the configured Plex path is unmapped: reported, not owned.
    std::fs::create_dir_all(&p.movies).unwrap();
    mock.lock().unwrap().plex_movies = vec![
        json!({ "ratingKey": "101", "type": "movie", "title": "The Menu", "year": 2022,
        "Guid": [{ "id": "tmdb://593643" }], "Media": [{ "Part": [{ "file": "/other_mount/The Menu (2022)/x.mkv" }] }] }),
    ];
    mock.lock()
        .unwrap()
        .tmdb
        .insert("593643".into(), tmdb_details(593643, "The Menu", 2022, 107));
    let (_, v, _) = app
        .post("/api/v1/setup/plex/bootstrap", json!({ "media": "movies" }))
        .await;
    assert_eq!(v["runs"][0]["counts"]["unmapped"], 1, "{v}");
    assert_eq!(v["runs"][0]["issues"][0]["kind"], "unmapped");
    assert!(
        app.state.index().movie(593643).is_none(),
        "no file, no ownership"
    );
    let (_, status, _) = app.get("/api/v1/setup/plex/bootstrap").await;
    assert_eq!(
        status["runs"].as_array().unwrap().len(),
        2,
        "both runs are stored: {status}"
    );
    app.stop().await;
    // Over a native snapshot import the bootstrap is refused (that catalog carries evidence).
    let f = tmp.path().join("native.json");
    std::fs::write(
        &f,
        serde_json::to_vec(&snapshot_with_tpb("127.0.0.1", 6, "")).unwrap(),
    )
    .unwrap();
    let state = p.state.to_str().unwrap().to_string();
    let (ok, _, text) = cli(&[
        "import",
        "--state-dir",
        &state,
        "--snapshot",
        f.to_str().unwrap(),
    ]);
    assert!(ok, "{text}");
    let app = serve(&p.state).await;
    let (s, v, _) = app.post("/api/v1/setup/plex/bootstrap", json!({})).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("setup_conflict")),
        "{v}"
    );
    let (s, v, _) = app.post("/api/v1/setup", json!({})).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("setup_conflict")),
        "managed setup never replaces a snapshot import: {v}"
    );
    app.stop().await;
}

#[tokio::test]
async fn rss_gaps_are_judged_by_overlap_even_when_the_feed_is_shorter_than_its_limit() {
    let (mock_addr, mock) = start_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let (_p, app) = api_setup(tmp.path(), mock_addr, false, None, false).await;
    // A recent feed of `rows` rows a minute apart, the newest at `newest`: fewer than its
    // nominal 100 rows (real feeds may answer 50). Unrelated categories still date the window.
    let window = |rows: i64, newest: i64| -> Vec<Value> {
        (0..rows)
            .map(|i| {
                json!({ "id": (newest + i).to_string(), "name": format!("Unrelated Show S01E{:02} 1080p WEB-DL", i % 50 + 1),
                    "info_hash": format!("{:040x}", newest + i), "seeders": "3", "leechers": "0", "size": "1000000000",
                    "username": "x", "added": (newest - i * 60).to_string(), "category": if i % 5 == 0 { "104" } else { "208" } })
            })
            .collect()
    };
    let t0 = time::OffsetDateTime::now_utc().unix_timestamp() - 86_400;
    for (step, rows, newest, gap) in [
        // First sync: no baseline.
        ("baseline", 50, t0, false),
        // 30 minutes later: the 50-row window still reaches back past the last newest.
        ("overlap", 50, t0 + 30 * 60, false),
        // 81 minutes after that: oldest row 32 minutes newer than the last newest. The window
        // is half its nominal size and must still be flagged.
        ("no overlap, 50 of 100 rows", 50, t0 + 111 * 60, true),
        // Overlapping again: the flag clears.
        ("overlap again", 50, t0 + 120 * 60, false),
        // A much smaller window that has moved past the last sync is a gap too.
        ("no overlap, 3 rows", 3, t0 + 200 * 60, true),
    ] {
        mock.lock().unwrap().tpb_recent = window(rows, newest);
        let report = arr::rss::sync(&app.state, time::OffsetDateTime::now_utc(), true)
            .await
            .unwrap();
        let tpb = report.indexers.iter().find(|i| i.indexer_id == 1).unwrap();
        assert_eq!(tpb.status, "ok", "{step}: {report:?}");
        assert_eq!(
            tpb.window.unwrap_or(tpb.items),
            rows as usize,
            "{step}: {report:?}"
        );
        assert_eq!(tpb.gap_possible, gap, "{step}: {report:?}");
        let (_, v, _) = app.get("/api/v1/rss").await;
        let state = v["indexers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["indexer_id"] == 1)
            .unwrap()
            .clone();
        assert_eq!(state["gap_possible"], gap, "{step}: persisted {state}");
        let newest_s = time::OffsetDateTime::from_unix_timestamp(newest)
            .unwrap()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        assert_eq!(
            state["newest_published_at"],
            newest_s.as_str(),
            "{step}: {state}"
        );
    }
    app.stop().await;
}

#[tokio::test]
async fn bootstrap_keeps_plex_titles_and_attested_tvmaze_akas_of_the_same_show_only() {
    let (mock_addr, mock) = start_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let (p, app) = api_setup(tmp.path(), mock_addr, false, None, false).await;
    // (folder, Plex rating key, TVDB id, TVmaze id, Plex title, TVmaze title, AKAs, files)
    type Show<'a> = (
        &'a str,
        &'a str,
        u32,
        u32,
        &'a str,
        &'a str,
        Vec<Value>,
        Vec<(u32, &'a str)>,
    );
    let shows: Vec<Show> = vec![
        // English title in Plex and in the files; TVmaze's exact TVDB match names the show in
        // its original language. A file of another show in the same folder stays a conflict.
        (
            "Borderland",
            "401",
            700001,
            8001,
            "Borderland",
            "Grenzland",
            vec![json!({ "name": "Pays frontalier", "country": { "code": "FR" } })],
            vec![
                (
                    1,
                    "Borderland.S01E01.Episode.1.1080p.NF.WEB-DL.DDP5.1.x264-GROUP.mkv",
                ),
                (
                    2,
                    "Borderland.S01E02.Episode.2.1080p.NF.WEB-DL.DDP5.1.x264-GROUP.mkv",
                ),
                (3, "Other.Show.S01E03.1080p.WEB-DL.x264-GRP.mkv"),
            ],
        ),
        // Plex and TVmaze agree on three words; the files use the middle word, which TVmaze
        // lists as an official AKA of this exact show: accepted, and kept as an alternate title.
        (
            "Night Harbour Patrol",
            "402",
            700002,
            8002,
            "Night Harbour Patrol",
            "Night Harbour Patrol",
            vec![
                json!({ "name": "Nachthafen", "country": { "code": "DE" } }),
                json!({ "name": "Harbour", "country": { "code": "GB" } }),
            ],
            vec![(1, "Harbour.S01E01.1080p.WEB-DL.x264-GRP.mkv")],
        ),
        // The same shape without AKA evidence: the abbreviation is not guessed; conflict.
        (
            "Deep Field Station",
            "403",
            700003,
            8003,
            "Deep Field Station",
            "Deep Field Station",
            vec![json!({ "name": "Station Profonde", "country": { "code": "FR" } })],
            vec![(1, "Field.S01E01.1080p.WEB-DL.x264-GRP.mkv")],
        ),
        // Clean: titles agree with the files; no AKA list is requested.
        (
            "Plain Show",
            "404",
            700004,
            8004,
            "Plain Show",
            "Plain Show",
            vec![],
            vec![(1, "Plain.Show.S01E01.1080p.WEB-DL.x264-GRP.mkv")],
        ),
    ];
    {
        let mut w = mock.lock().unwrap();
        for (folder, key, tvdb, tvmaze, plex_title, tv_title, akas, files) in &shows {
            w.plex_shows.push(json!({ "ratingKey": key, "type": "show", "title": plex_title, "Guid": [{ "id": format!("tvdb://{tvdb}") }] }));
            w.plex_leaves.insert(
                key.to_string(),
                files
                    .iter()
                    .map(|(n, f)| {
                        let rel = format!("{folder}/Season 1/{f}");
                        let local = p.tv.join(&rel);
                        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
                        std::fs::write(&local, b"video").unwrap();
                        json!({ "ratingKey": format!("{key}{n}"), "type": "episode", "parentIndex": 1, "index": n,
                            "Media": [{ "Part": [{ "file": format!("/media_library/tvshows/{rel}") }] }] })
                    })
                    .collect(),
            );
            w.tvmaze_shows.insert(*tvmaze, json!({ "id": tvmaze, "name": tv_title, "premiered": "2024-01-05",
                "language": "German", "runtime": 45, "type": "Scripted", "externals": { "thetvdb": tvdb } }));
            w.tvmaze_episodes.insert(*tvmaze, (1..=3).map(|n| json!({ "season": 1, "number": n, "name": format!("Episode {n}"), "type": "regular" })).collect());
            w.tvmaze_akas.insert(*tvmaze, akas.clone());
        }
    }
    let (s, v, text) = app
        .post("/api/v1/setup/plex/bootstrap", json!({ "media": "tv" }))
        .await;
    assert_eq!(s, 200, "{text}");
    let run = &v["runs"][0];
    assert_eq!(run["status"], "done", "{v}");
    let c = &run["counts"];
    assert_eq!(
        (
            c["items_seen"].as_u64(),
            c["titles_created"].as_u64(),
            c["files_recorded"].as_u64()
        ),
        (Some(4), Some(3), Some(4)),
        "{v}"
    );
    assert_eq!(
        (c["conflicts"].as_u64(), c["metadata_failures"].as_u64()),
        (Some(2), Some(0)),
        "{v}"
    );
    let issues: Vec<String> = run["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            format!(
                "{}: {}",
                i["kind"].as_str().unwrap(),
                i["detail"].as_str().unwrap()
            )
        })
        .collect();
    let has = |needle: &str| issues.iter().any(|i| i.contains(needle));
    assert!(
        has("conflict: Borderland/Season 1/Other.Show.S01E03"),
        "{issues:#?}"
    );
    assert!(
        has("conflict: Deep Field Station/Season 1/Field.S01E01"),
        "{issues:#?}"
    );
    assert!(
        has("Plex's title \"Borderland\" (the same TVDB id) is kept as an alternate title"),
        "{issues:#?}"
    );
    assert!(
        has("name the show \"Harbour\", an official TVmaze alternative title"),
        "{issues:#?}"
    );
    // AKAs are read only for new shows whose files still named another title.
    let mut hits = mock.lock().unwrap().tvmaze_akas_hits.clone();
    hits.sort_unstable();
    assert_eq!(hits, vec![8001, 8002, 8003]);

    let index = app.state.index();
    let series = |tvdb: u32| index.series(arr::ids::SeriesKey::Tvdb(tvdb)).cloned();
    let alts = |tvdb: u32| -> Vec<String> {
        series(tvdb)
            .unwrap()
            .alternate_titles
            .iter()
            .map(|a| a.title.clone())
            .collect()
    };
    let border = series(700001).unwrap();
    assert_eq!(
        border.title, "Grenzland",
        "TVmaze's title stays the canonical one"
    );
    assert_eq!(
        alts(700001),
        vec!["Borderland"],
        "no AKA was attested by its files"
    );
    assert_eq!(alts(700002), vec!["Harbour"], "only the AKA the files use");
    assert!(
        series(700003).is_none(),
        "every file conflicted: nothing created"
    );
    assert!(alts(700004).is_empty());
    let owned = |s: &arr::catalog::Series| -> Vec<u16> {
        s.episodes
            .iter()
            .flatten()
            .filter(|e| e.has_file)
            .map(|e| e.number)
            .collect()
    };
    assert_eq!(
        owned(&border),
        vec![1, 2],
        "the foreign E03 file is not owned"
    );
    assert_eq!(owned(&series(700002).unwrap()), vec![1]);
    for tvdb in [700001, 700002, 700004] {
        let s = series(tvdb).unwrap();
        assert!(
            !s.monitored && s.episodes.iter().flatten().all(|e| !e.monitored),
            "read-only bootstrap: nothing monitored"
        );
    }
    drop(index);
    let (_, q, _) = app.get("/api/v1/queue?all=true").await;
    assert_eq!(q["total"], 0, "nothing queued");
    // A later scan judges the same names the same way: the stored alternate titles keep the
    // accepted files; the foreign one is still another title.
    let reports = arr::scan::scan(&app.state, time::OffsetDateTime::now_utc(), true)
        .await
        .unwrap();
    let tv = reports.iter().find(|r| r.name == "tv").unwrap();
    let unmatched = tv.unmatched.join("\n");
    assert!(
        unmatched.contains("Other.Show.S01E03") && unmatched.contains("names another title"),
        "{tv:?}"
    );
    assert!(
        !unmatched.contains("Borderland.S01E0") && !unmatched.contains("Harbour.S01E01"),
        "{tv:?}"
    );
    app.stop().await;
}
