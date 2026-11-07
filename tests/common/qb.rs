//! Loopback qBittorrent WebUI API mock: 4.3.9 (WebAPI 2.8.2) or 5.x (2.11) behaviour, cookie
//! sessions, multipart adds, hash-filtered info, files/properties/preferences, pause/resume vs
//! stop/start, delete (with real file deletion on the shared local directory), and injectable
//! failures (withheld add answers, expired sessions, outages). Fake credentials only.
#![allow(dead_code, clippy::result_large_err)]

use super::{FAKE_PASSWORD, FAKE_USER};
use axum::Router;
use axum::extract::{Multipart, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub const SID: &str = "FAKE-TEST-SID-0001";

/// Torrent name and its files (relative path, bytes).
pub type Fixture = (String, Vec<(String, Vec<u8>)>);

#[derive(Clone, Debug)]
pub struct MockTorrent {
    pub hash: String,
    pub name: String,
    pub category: String,
    pub tags: String,
    pub save_path: String,
    pub state: String,
    pub progress: f64,
    pub files: Vec<(String, u64)>,
    pub ratio: f64,
    pub ratio_limit: f64,
    pub seeding_time: i64,
    pub seeding_time_limit: i64,
    pub priorities: Vec<i64>,
}

#[derive(Default)]
pub struct QbState {
    pub api: String,
    pub torrents: BTreeMap<String, MockTorrent>,
    /// Release data by info-hash (what "downloading" produces), relative to the save path.
    pub fixtures: HashMap<String, Fixture>,
    pub client_root: String,
    pub local_root: PathBuf,
    pub log: Vec<String>,
    pub add_fields: Vec<BTreeMap<String, String>>,
    /// Accept the add, then answer 500 (an ambiguous outcome).
    pub withhold_add_answer: bool,
    /// After accepting an add, go down (every later call answers 503).
    pub down_after_add: bool,
    /// Answer every call with 503.
    pub down: bool,
    /// Next authenticated call answers 403 once (expired session).
    pub expire_session: bool,
    pub logins: usize,
    pub prefs: Value,
    /// Parameters a 4.3.9 server does not know but received.
    pub unknown_params: Vec<String>,
}

pub type Qb = Arc<Mutex<QbState>>;

fn ok_text(s: &str) -> Response {
    (StatusCode::OK, s.to_string()).into_response()
}

fn v5(s: &QbState) -> bool {
    s.api.starts_with("2.11") || s.api.starts_with("2.12")
}

fn authed(s: &mut QbState, headers: &HeaderMap) -> Result<(), Response> {
    if s.down {
        return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
    }
    let cookie = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if s.expire_session {
        s.expire_session = false;
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    if cookie != format!("SID={SID}") {
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    Ok(())
}

async fn login(State(q): State<Qb>, body: String) -> Response {
    let mut s = q.lock().unwrap();
    if s.down {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    s.logins += 1;
    s.log.push("login".into());
    let form: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    if form.get("username").map(String::as_str) == Some(FAKE_USER)
        && form.get("password").map(String::as_str) == Some(FAKE_PASSWORD)
    {
        (
            StatusCode::OK,
            [("set-cookie", format!("SID={SID}; HttpOnly; path=/"))],
            "Ok.",
        )
            .into_response()
    } else {
        ok_text("Fails.")
    }
}

async fn version(State(q): State<Qb>, headers: HeaderMap) -> Response {
    let mut s = q.lock().unwrap();
    if let Err(r) = authed(&mut s, &headers) {
        return r;
    }
    ok_text(&s.api.clone())
}

fn info_json(t: &MockTorrent, s: &QbState) -> Value {
    let size: u64 = t.files.iter().map(|f| f.1).sum();
    let mut v = json!({
        "hash": t.hash, "name": t.name, "category": t.category, "tags": t.tags, "state": t.state, "progress": t.progress,
        "size": size, "amount_left": ((1.0 - t.progress) * size as f64) as i64, "save_path": format!("{}/", t.save_path),
        "content_path": format!("{}/{}", t.save_path, t.name), "dlspeed": 0, "eta": 8640000,
        "ratio": t.ratio, "ratio_limit": t.ratio_limit, "seeding_time_limit": t.seeding_time_limit,
        "max_ratio": if t.ratio_limit >= 0.0 { t.ratio_limit } else if t.ratio_limit == -2.0 && s.prefs["max_ratio_enabled"] == true { s.prefs["max_ratio"].as_f64().unwrap_or(-1.0) } else { -1.0 },
        "max_seeding_time": if t.seeding_time_limit >= 0 { t.seeding_time_limit } else { -1 },
        "downloaded": size, "uploaded": (t.ratio * size as f64) as i64, "seeding_time": t.seeding_time,
    });
    if !v5(s) && s.api.as_str() < "2.8.1" {
        v.as_object_mut().unwrap().remove("seeding_time");
    }
    v
}

async fn info(
    State(q): State<Qb>,
    headers: HeaderMap,
    Query(p): Query<HashMap<String, String>>,
) -> Response {
    let mut s = q.lock().unwrap();
    if let Err(r) = authed(&mut s, &headers) {
        return r;
    }
    if p.contains_key("tag") && !v5(&s) && s.api.as_str() < "2.8.3" {
        s.unknown_params.push("info:tag".into());
    }
    s.log.push(format!(
        "info {}",
        p.get("hashes").cloned().unwrap_or_default()
    ));
    let want: Option<Vec<String>> = p
        .get("hashes")
        .map(|h| h.split('|').map(|x| x.to_ascii_lowercase()).collect());
    let list: Vec<Value> = s
        .torrents
        .values()
        .filter(|t| want.as_ref().is_none_or(|w| w.contains(&t.hash)))
        .map(|t| info_json(t, &s))
        .collect();
    axum::Json(list).into_response()
}

async fn files(
    State(q): State<Qb>,
    headers: HeaderMap,
    Query(p): Query<HashMap<String, String>>,
) -> Response {
    let mut s = q.lock().unwrap();
    if let Err(r) = authed(&mut s, &headers) {
        return r;
    }
    let Some(t) = p
        .get("hash")
        .and_then(|h| s.torrents.get(&h.to_ascii_lowercase()))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let list: Vec<Value> = t
        .files
        .iter()
        .enumerate()
        .map(|(i, (n, size))| json!({ "index": i, "name": n, "size": size, "progress": t.progress, "priority": t.priorities.get(i).copied().unwrap_or(1), "is_seed": t.progress >= 1.0 }))
        .collect();
    axum::Json(list).into_response()
}

async fn properties(
    State(q): State<Qb>,
    headers: HeaderMap,
    Query(p): Query<HashMap<String, String>>,
) -> Response {
    let mut s = q.lock().unwrap();
    if let Err(r) = authed(&mut s, &headers) {
        return r;
    }
    let Some(t) = p
        .get("hash")
        .and_then(|h| s.torrents.get(&h.to_ascii_lowercase()))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    axum::Json(json!({ "share_ratio": t.ratio, "seeding_time": t.seeding_time, "total_downloaded": 1, "total_uploaded": 1, "save_path": format!("{}/", t.save_path) })).into_response()
}

async fn preferences(State(q): State<Qb>, headers: HeaderMap) -> Response {
    let mut s = q.lock().unwrap();
    if let Err(r) = authed(&mut s, &headers) {
        return r;
    }
    axum::Json(s.prefs.clone()).into_response()
}

async fn categories(State(q): State<Qb>, headers: HeaderMap) -> Response {
    let mut s = q.lock().unwrap();
    if let Err(r) = authed(&mut s, &headers) {
        return r;
    }
    let mut cats = serde_json::Map::new();
    for t in s.torrents.values() {
        cats.insert(
            t.category.clone(),
            json!({ "name": t.category, "savePath": "" }),
        );
    }
    axum::Json(Value::Object(cats)).into_response()
}

const KNOWN_ADD_4_3_9: &[&str] = &[
    "urls",
    "torrents",
    "savepath",
    "cookie",
    "category",
    "tags",
    "skip_checking",
    "paused",
    "root_folder",
    "rename",
    "upLimit",
    "dlLimit",
    "ratioLimit",
    "seedingTimeLimit",
    "autoTMM",
    "sequentialDownload",
    "firstLastPiecePrio",
    "contentLayout",
];

async fn add(State(q): State<Qb>, headers: HeaderMap, mut mp: Multipart) -> Response {
    {
        let mut s = q.lock().unwrap();
        if let Err(r) = authed(&mut s, &headers) {
            return r;
        }
    }
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut torrent: Option<Vec<u8>> = None;
    while let Ok(Some(f)) = mp.next_field().await {
        let name = f.name().unwrap_or("").to_string();
        if name == "torrents" {
            torrent = f.bytes().await.ok().map(|b| b.to_vec());
            fields.insert(name, "<file>".into());
        } else {
            fields.insert(name, f.text().await.unwrap_or_default());
        }
    }
    let mut s = q.lock().unwrap();
    s.log.push("add".into());
    if !v5(&s) {
        for k in fields.keys() {
            if !KNOWN_ADD_4_3_9.contains(&k.as_str()) {
                s.unknown_params.push(format!("add:{k}"));
            }
        }
    }
    s.add_fields.push(fields.clone());
    let (hash, name) = if let Some(t) = &torrent {
        match arr::torrent::info_hash(t) {
            Ok(h) => {
                let name = s
                    .fixtures
                    .get(&h)
                    .map(|f| f.0.clone())
                    .unwrap_or_else(|| "unnamed".into());
                (h, name)
            }
            Err(_) => return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response(),
        }
    } else if let Some(u) = fields.get("urls") {
        match arr::torrent::magnet_hash(u) {
            Ok(h) => {
                let name = s
                    .fixtures
                    .get(&h)
                    .map(|f| f.0.clone())
                    .unwrap_or_else(|| "metadata pending".into());
                (h, name)
            }
            Err(_) => return ok_text("Fails."),
        }
    } else {
        return ok_text("Fails.");
    };
    if s.torrents.contains_key(&hash) {
        return ok_text("Fails.");
    }
    let paused_key = if v5(&s) { "stopped" } else { "paused" };
    let paused = fields.get(paused_key).map(String::as_str) == Some("true");
    let files: Vec<(String, u64)> = s
        .fixtures
        .get(&hash)
        .map(|(n, f)| {
            let single = f.len() == 1 && !f[0].0.contains('/');
            f.iter()
                .map(|(p, b)| {
                    (
                        if single {
                            p.clone()
                        } else {
                            format!("{n}/{p}")
                        },
                        b.len() as u64,
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let t = MockTorrent {
        hash: hash.clone(),
        name,
        category: fields.get("category").cloned().unwrap_or_default(),
        tags: fields.get("tags").cloned().unwrap_or_default(),
        save_path: fields
            .get("savepath")
            .cloned()
            .unwrap_or_else(|| s.client_root.clone())
            .trim_end_matches('/')
            .to_string(),
        state: if paused {
            if v5(&s) { "stoppedDL" } else { "pausedDL" }
        } else if files.is_empty() {
            "metaDL"
        } else {
            "downloading"
        }
        .into(),
        progress: 0.0,
        priorities: vec![1; files.len()],
        files,
        ratio: 0.0,
        ratio_limit: fields
            .get("ratioLimit")
            .and_then(|r| r.parse().ok())
            .unwrap_or(-2.0),
        seeding_time: 0,
        seeding_time_limit: fields
            .get("seedingTimeLimit")
            .and_then(|r| r.parse().ok())
            .unwrap_or(-2),
    };
    s.torrents.insert(hash, t);
    if s.down_after_add {
        s.down_after_add = false;
        s.down = true;
    }
    if s.withhold_add_answer {
        s.withhold_add_answer = false;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    ok_text("Ok.")
}

fn form(body: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect()
}

async fn simple(q: Qb, headers: HeaderMap, body: String, op: &'static str) -> Response {
    let mut s = q.lock().unwrap();
    if let Err(r) = authed(&mut s, &headers) {
        return r;
    }
    let five = v5(&s);
    let renamed = matches!(op, "stop" | "start");
    if renamed != five && matches!(op, "stop" | "start" | "pause" | "resume") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let f = form(&body);
    let hashes: Vec<String> = f
        .get("hashes")
        .map(|h| h.split('|').map(str::to_ascii_lowercase).collect())
        .unwrap_or_default();
    s.log.push(format!("{op} {}", hashes.join("|")));
    for h in &hashes {
        match op {
            "pause" | "stop" => {
                if let Some(t) = s.torrents.get_mut(h) {
                    t.state = match (t.progress >= 1.0, five) {
                        (true, false) => "pausedUP",
                        (true, true) => "stoppedUP",
                        (false, false) => "pausedDL",
                        (false, true) => "stoppedDL",
                    }
                    .into();
                }
            }
            "resume" | "start" => {
                if let Some(t) = s.torrents.get_mut(h) {
                    t.state = if t.progress >= 1.0 {
                        "stalledUP"
                    } else {
                        "downloading"
                    }
                    .into();
                }
            }
            "delete" => {
                let delete_files = f.get("deleteFiles").map(String::as_str) == Some("true");
                if let Some(t) = s.torrents.remove(h)
                    && delete_files
                {
                    let local = local_of(&s, &t.save_path);
                    for (name, _) in &t.files {
                        let _ = std::fs::remove_file(local.join(name));
                    }
                    if t.files.len() > 1 || t.files.iter().any(|f| f.0.contains('/')) {
                        let _ = std::fs::remove_dir_all(local.join(&t.name));
                    }
                }
            }
            "setCategory" => {
                if let Some(t) = s.torrents.get_mut(h) {
                    t.category = f.get("category").cloned().unwrap_or_default();
                }
            }
            _ => {}
        }
    }
    ok_text("")
}

fn local_of(s: &QbState, save_path: &str) -> PathBuf {
    let rest = save_path
        .strip_prefix(&s.client_root)
        .unwrap_or("")
        .trim_start_matches('/');
    if rest.is_empty() {
        s.local_root.clone()
    } else {
        s.local_root.join(rest)
    }
}

pub async fn start(api: &str, local_root: PathBuf) -> (SocketAddr, Qb) {
    let state: Qb = Arc::new(Mutex::new(QbState {
        api: api.into(),
        client_root: "/downloads".into(),
        local_root,
        prefs: json!({ "max_ratio_enabled": false, "max_ratio": -1, "max_seeding_time_enabled": false, "max_seeding_time": -1 }),
        ..Default::default()
    }));
    let s2 = state.clone();
    let op = move |name: &'static str| {
        let q = s2.clone();
        post(move |headers: HeaderMap, body: String| simple(q.clone(), headers, body, name))
    };
    let app = Router::new()
        .route("/api/v2/auth/login", post(login))
        .route("/api/v2/app/webapiVersion", get(version))
        .route("/api/v2/app/preferences", get(preferences))
        .route("/api/v2/torrents/info", get(info))
        .route("/api/v2/torrents/files", get(files))
        .route("/api/v2/torrents/properties", get(properties))
        .route("/api/v2/torrents/categories", get(categories))
        .route("/api/v2/torrents/add", post(add))
        .route("/api/v2/torrents/pause", op("pause"))
        .route("/api/v2/torrents/resume", op("resume"))
        .route("/api/v2/torrents/stop", op("stop"))
        .route("/api/v2/torrents/start", op("start"))
        .route("/api/v2/torrents/delete", op("delete"))
        .route("/api/v2/torrents/setCategory", op("setCategory"))
        .route("/api/v2/torrents/setForceStart", op("setForceStart"))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, state)
}

/// Simulates the download finishing: writes the release data under the local save path,
/// marks the torrent complete and seeding (`stalledUP`).
pub fn complete(q: &Qb, hash: &str) {
    let mut s = q.lock().unwrap();
    let (name, files) = s.fixtures.get(hash).cloned().expect("fixture registered");
    let t = s.torrents.get(hash).cloned().expect("torrent added");
    let local = local_of(&s, &t.save_path);
    let single = files.len() == 1 && !files[0].0.contains('/');
    for (p, b) in &files {
        let path = if single {
            local.join(p)
        } else {
            local.join(&name).join(p)
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b).unwrap();
    }
    let t = s.torrents.get_mut(hash).unwrap();
    t.progress = 1.0;
    t.state = "stalledUP".into();
}

/// Simulates qBittorrent stopping the torrent at its share limit.
pub fn seeded(q: &Qb, hash: &str, ratio: f64, seeding_secs: i64, stopped: bool) {
    let mut s = q.lock().unwrap();
    let five = v5(&s);
    let t = s.torrents.get_mut(hash).unwrap();
    t.ratio = ratio;
    t.seeding_time = seeding_secs;
    if stopped {
        t.state = if five { "stoppedUP" } else { "pausedUP" }.into();
    }
}

pub fn set_state(q: &Qb, hash: &str, state: &str, progress: f64) {
    let mut s = q.lock().unwrap();
    let t = s.torrents.get_mut(hash).unwrap();
    t.state = state.into();
    t.progress = progress;
}
