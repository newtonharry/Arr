//! The browser setup (`arr wizard`) end to end: the real binary against a mock that plays
//! plex.tv (sign-in PIN, server list) and the Plex server itself.

use axum::Router;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

const BIN: &str = env!("CARGO_BIN_EXE_arr");
const ACCOUNT_TOKEN: &str = "account-token-0123456789";
const SERVER_TOKEN: &str = "server-token-0123456789";
const MACHINE: &str = "machine-0123456789";

#[derive(Default)]
struct World {
    pin_polls: u32,
    /// Sign-ins started: the first gets PIN 42, the next 43, and so on.
    pins_made: u64,
    /// plex.tv answers for PIN 42 late, signing in another account.
    slow_first_pin: bool,
    resources_calls: u32,
    movies_path: String,
    tv_path: String,
    /// The machine the Plex beside the wizard says it is.
    identity: String,
    /// No Plex answers on this computer until it's "installed".
    not_installed: bool,
    /// A new Plex isn't linked to any account (and plex.tv doesn't list it).
    unclaimed: bool,
    claims: u32,
    /// A movie library added in Plex after the page connected.
    added_library: Option<String>,
    /// Plex fails to list a library's items, so the import fails.
    import_fails: bool,
    /// Libraries added through the API: (type, location, language).
    created: Vec<(String, String, String)>,
    /// The token each request carried, by path.
    tokens: Vec<(String, String)>,
    client_ids: Vec<String>,
}

type Mock = Arc<Mutex<World>>;

fn record(m: &Mock, path: &str, h: &HeaderMap) {
    let mut w = m.lock().unwrap();
    if let Some(t) = h.get("x-plex-token").and_then(|v| v.to_str().ok()) {
        w.tokens.push((path.into(), t.into()));
    }
    if let Some(c) = h
        .get("x-plex-client-identifier")
        .and_then(|v| v.to_str().ok())
    {
        w.client_ids.push(c.into());
    }
}

async fn start_mock(movies: &Path, tv: &Path) -> (SocketAddr, Mock) {
    let m: Mock = Arc::new(Mutex::new(World {
        movies_path: movies.to_str().unwrap().into(),
        tv_path: tv.to_str().unwrap().into(),
        identity: MACHINE.into(),
        ..Default::default()
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route(
            "/api/v2/pins",
            post(|State(m): State<Mock>, h: HeaderMap| async move {
                record(&m, "pins", &h);
                let id = {
                    let mut w = m.lock().unwrap();
                    w.pins_made += 1;
                    41 + w.pins_made
                };
                axum::Json(json!({ "id": id, "code": "pin-code", "authToken": null }))
            }),
        )
        .route(
            "/api/v2/pins/{id}",
            get(
                |State(m): State<Mock>,
                 h: HeaderMap,
                 AxPath(id): AxPath<u64>,
                 Query(q): Query<HashMap<String, String>>| async move {
                    record(&m, "pin", &h);
                    let made = m.lock().unwrap().pins_made;
                    if !(42..42 + made).contains(&id)
                        || q.get("code").map(String::as_str) != Some("pin-code")
                    {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    if id == 42 && m.lock().unwrap().slow_first_pin {
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                        return axum::Json(json!({ "id": 42, "code": "pin-code", "authToken": "old-account" }))
                            .into_response();
                    }
                    let mut w = m.lock().unwrap();
                    w.pin_polls += 1;
                    // The first check finds the sign-in still open.
                    let token = (w.pin_polls > 1).then_some(ACCOUNT_TOKEN);
                    axum::Json(json!({ "id": 42, "code": "pin-code", "authToken": token }))
                        .into_response()
                },
            ),
        )
        .route(
            "/api/v2/resources",
            get(move |State(m): State<Mock>, h: HeaderMap| async move {
                record(&m, "resources", &h);
                if h.get("x-plex-token").and_then(|v| v.to_str().ok()) != Some(ACCOUNT_TOKEN) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                // plex.tv has a bad moment on the first lookup.
                m.lock().unwrap().resources_calls += 1;
                if m.lock().unwrap().resources_calls == 1 {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                let port = addr.port();
                let listed = !m.lock().unwrap().unclaimed;
                axum::Json(json!([
                    { "name": "Phone", "provides": "client,player", "owned": true,
                      "clientIdentifier": "phone", "accessToken": "x", "connections": [] },
                    { "name": "Friend's Plex", "provides": "server", "owned": false,
                      "clientIdentifier": "friend", "accessToken": "friend-token", "connections": [] },
                    { "name": "Living Room", "provides": if listed { "server" } else { "none" }, "owned": true,
                      "clientIdentifier": MACHINE, "accessToken": SERVER_TOKEN,
                      "connections": [
                        { "protocol": "http", "address": "127.0.0.1", "port": port,
                          "uri": format!("http://127.0.0.1:{port}"), "local": true, "relay": false },
                      ] },
                ]))
                .into_response()
            }),
        )
        .route(
            "/identity",
            get(|State(m): State<Mock>| async move {
                let w = m.lock().unwrap();
                if w.not_installed {
                    return StatusCode::NOT_FOUND.into_response();
                }
                axum::Json(json!({ "MediaContainer": { "machineIdentifier": w.identity, "claimed": !w.unclaimed } }))
                    .into_response()
            }),
        )
        .route(
            "/api/claim/token.json",
            get(|State(m): State<Mock>, h: HeaderMap| async move {
                record(&m, "resources", &h);
                axum::Json(json!({ "token": "claim-0123" }))
            }),
        )
        .route(
            "/myplex/claim",
            post(|State(m): State<Mock>, Query(q): Query<HashMap<String, String>>| async move {
                let mut w = m.lock().unwrap();
                if q.get("token").map(String::as_str) != Some("claim-0123") {
                    return StatusCode::BAD_REQUEST;
                }
                w.unclaimed = false;
                w.claims += 1;
                StatusCode::OK
            }),
        )
        .route(
            "/test/install",
            post(|State(m): State<Mock>| async move {
                let mut w = m.lock().unwrap();
                w.not_installed = false;
                w.unclaimed = true;
            }),
        )
        .route(
            "/library/sections",
            get(|State(m): State<Mock>, h: HeaderMap| async move {
                record(&m, "sections", &h);
                if h.get("x-plex-token").and_then(|v| v.to_str().ok()) != Some(SERVER_TOKEN) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                let w = m.lock().unwrap();
                // A Plex installed from the page starts with no libraries.
                let dirs: Vec<Value> = if w.claims > 0 {
                    w.created.iter().enumerate().map(|(i, (kind, path, _))| json!({
                        "key": (10 + i).to_string(), "title": if kind == "movie" { "Movies" } else { "TV Shows" },
                        "type": kind, "Location": [{ "path": path }] })).collect()
                } else {
                    vec![
                        json!({ "key": "1", "title": "Movies", "type": "movie", "Location": [{ "path": w.movies_path }] }),
                        json!({ "key": "2", "title": "TV Shows", "type": "show", "Location": [{ "path": w.tv_path }] }),
                        json!({ "key": "3", "title": "Music", "type": "artist", "Location": [{ "path": "/music" }] }),
                    ]
                };
                let mut dirs = dirs;
                if let Some(path) = &w.added_library {
                    dirs.push(json!({ "key": "4", "title": "Films", "type": "movie", "Location": [{ "path": path }] }));
                }
                axum::Json(json!({ "MediaContainer": { "Directory": dirs } })).into_response()
            })
            .post(|State(m): State<Mock>, h: HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                record(&m, "sections", &h);
                if h.get("x-plex-token").and_then(|v| v.to_str().ok()) != Some(SERVER_TOKEN) {
                    return StatusCode::UNAUTHORIZED;
                }
                let want = |k: &str| q.get(k).cloned().unwrap_or_default();
                let agent_ok = matches!(
                    (want("type").as_str(), want("agent").as_str(), want("scanner").as_str()),
                    ("movie", "tv.plex.agents.movie", "Plex Movie") | ("show", "tv.plex.agents.series", "Plex TV Series")
                );
                if !agent_ok {
                    return StatusCode::BAD_REQUEST;
                }
                m.lock().unwrap().created.push((want("type"), want("location"), want("language")));
                StatusCode::CREATED
            }),
        )
        .route(
            "/library/sections/{id}/all",
            get(|State(m): State<Mock>, h: HeaderMap| async move {
                if m.lock().unwrap().import_fails {
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
                record(&m, "all", &h);
                if h.get("x-plex-token").and_then(|v| v.to_str().ok()) != Some(SERVER_TOKEN) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(json!({ "MediaContainer": { "size": 0, "totalSize": 0, "offset": 0, "Metadata": [] } }))
                    .into_response()
            }),
        )
        .with_state(m.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, m)
}

/// The code with its last digit changed.
fn wrong_code(code: &str) -> String {
    let last = if code.ends_with('0') { '1' } else { '0' };
    format!("{}{last}", &code[..code.len() - 1])
}

/// Debian's ffprobe; on Windows, the first on the PATH.
fn ffprobe() -> std::path::PathBuf {
    if cfg!(unix) {
        return "/usr/bin/ffprobe".into();
    }
    std::env::var_os("PATH")
        .and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("ffprobe.exe"))
                .find(|f| f.is_file())
        })
        .unwrap_or_else(|| "ffprobe.exe".into())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[derive(Clone)]
struct Page {
    base: String,
    cookie: String,
    http: reqwest::Client,
}

impl Page {
    async fn get(&self, path: &str) -> (StatusCode, Value) {
        let r = self
            .http
            .get(format!("{}{path}", self.base))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap();
        let s = r.status();
        (s, r.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let r = self
            .http
            .post(format!("{}{path}", self.base))
            .header("cookie", &self.cookie)
            .header("x-arr-setup", "1")
            .json(&body)
            .send()
            .await
            .unwrap();
        let s = r.status();
        (s, r.json().await.unwrap_or(Value::Null))
    }
}

/// Starts `arr wizard` against the mock; returns it, the link it printed, and its port.
async fn start_wizard(
    state_dir: &Path,
    mock: SocketAddr,
    extra: &[String],
) -> (tokio::process::Child, String, u16) {
    let port = free_port();
    let mut child = tokio::process::Command::new(BIN)
        .args(["wizard", "--no-docker", "--port", &port.to_string()])
        .arg("--state-dir")
        .arg(state_dir)
        .arg("--ffprobe")
        .arg(ffprobe())
        .arg(format!("--plex-tv-url=http://{mock}/"))
        .arg(format!("--plex-auth-url=http://{mock}/auth"))
        .args(extra)
        // Error reports never leave a test: nothing listens on port 9.
        .env("ARR_SENTRY_DSN", "http://key@127.0.0.1:9/1")
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let link = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(l) = lines.next_line().await.unwrap() {
            if let Some(i) = l.find("http://") {
                return l[i..].trim().to_string();
            }
        }
        panic!("the wizard printed no link");
    })
    .await
    .unwrap();
    // Keep reading: if the pipe closed, the wizard's next line would fail to print.
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    (child, link, port)
}

#[tokio::test]
async fn browser_setup_signs_in_to_plex_writes_the_config_and_imports() {
    let tmp = tempfile::tempdir().unwrap();
    let movies = tmp.path().join("media/movies");
    let tv = tmp.path().join("media/tv");
    std::fs::create_dir_all(&movies).unwrap();
    std::fs::create_dir_all(&tv).unwrap();
    let state_dir = tmp.path().join("state");
    let (mock, world) = start_mock(&movies, &tv).await;
    let extra = [
        "--public-url".to_string(),
        "https://arr-test.ngrok-free.app/".to_string(),
    ];
    let (mut child, link, port) = start_wizard(&state_dir, mock, &extra).await;
    let base = format!("http://127.0.0.1:{port}");
    let code = link.split("code=").nth(1).unwrap();

    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    // Without the link's code there's no way in.
    let r = http.get(format!("{base}/api/state")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = http.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = http
        .get(format!("{base}/?code={}", wrong_code(code)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    // The code becomes a strict, script-proof cookie, and the code leaves the address bar.
    let r = http
        .get(format!("{base}/?code={code}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
    assert_eq!(r.headers()["location"], "/");
    let set = r.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(
        set.contains("HttpOnly") && set.contains("SameSite=Strict"),
        "{set}"
    );
    let cookie = set.split(';').next().unwrap().to_string();
    let page = Page {
        base: base.clone(),
        cookie,
        http: http.clone(),
    };
    let r = http
        .get(format!("{base}/"))
        .header("cookie", &page.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(
        r.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("default-src 'self'")
    );
    assert!(r.text().await.unwrap().contains("Sign in with Plex"));
    // Changes need the setup header, which a cross-site form can't send.
    let r = http
        .post(format!("{base}/api/plex/signin"))
        .header("cookie", &page.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);

    // Sign in: the page gets plex.tv's link, never a token.
    let (s, v) = page.post("/api/plex/signin", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let url = v["url"].as_str().unwrap();
    assert!(
        url.starts_with(&format!("http://{mock}/auth#?clientID=")),
        "{url}"
    );
    assert!(url.contains("&code=pin-code&"), "{url}");
    let (_, v) = page.post("/api/plex/check", json!({})).await;
    assert_eq!(v["signed_in"], false);
    assert_eq!(v["waiting_for_signin"], true);
    // Signed in, but the server lookup failed: checking again retries it.
    let (s, v) = page.post("/api/plex/check", json!({})).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{v}");
    let (_, v) = page.get("/api/state").await;
    assert_eq!(v["signed_in"], true);
    assert_eq!(v["plex"], Value::Null);
    let (s, v) = page.post("/api/plex/check", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["signed_in"], true);
    assert!(!v.to_string().contains(ACCOUNT_TOKEN) && !v.to_string().contains(SERVER_TOKEN));
    // The Plex on this computer is the account's own server, so Arr connects straight away.
    assert_eq!(v["plex"]["name"], "Living Room");
    assert_eq!(v["missing"], Value::Null);
    assert_eq!(
        v["plex"]["movies"],
        json!([{ "id": "1:0", "title": "Movies", "plex_path": movies, "local_path": movies }])
    );
    assert_eq!(v["plex"]["tv"][0]["id"], "2:0");

    // Error reports: offered, off until ticked, and switchable before Finish.
    let (_, v) = page.get("/api/state").await;
    assert_eq!(
        v["reporting"],
        json!({ "available": true, "enabled": false })
    );
    let (s, v) = page
        .post("/api/reporting", json!({ "enabled": true }))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["reporting"]["enabled"], true);
    let (_, v) = page
        .post("/api/reporting", json!({ "enabled": false }))
        .await;
    assert_eq!(v["reporting"]["enabled"], false);

    // Folder checks.
    let (_, v) = page
        .post("/api/folder", json!({ "path": "media/movies" }))
        .await;
    assert_eq!(v["ok"], false);
    let (_, v) = page.post("/api/folder", json!({ "path": movies })).await;
    assert_eq!(v["ok"], true);

    // Bad choices are refused before anything is written.
    let downloads = tmp.path().join("media/arr-downloads");
    let choices = |movie_id: &str| {
        json!({
            "movies": { "id": movie_id, "folder": movies },
            "tv": { "id": "2:0", "folder": tv },
            "downloads": downloads,
            "auto_update": true,
            "reporting": true,
        })
    };
    let (s, v) = page.post("/api/apply", choices("3:0")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(!state_dir.join("config.toml").exists());

    // Movies come from Plex: the page has no TMDB key section and no key check.
    let (s, _) = page
        .post("/api/tmdb/check", json!({ "key": "anything" }))
        .await;
    assert_ne!(s, StatusCode::OK);
    for file in ["index.html", "app.js"] {
        let text =
            std::fs::read_to_string(format!("{}/src/wizard/{file}", env!("CARGO_MANIFEST_DIR")))
                .unwrap();
        assert!(
            !text.to_lowercase().contains("tmdb"),
            "{file} mentions TMDB"
        );
    }
    let (_, v) = page.get("/api/state").await;
    assert_eq!(v["phase"]["state"], "editing", "{v}");
    // The Done screen's addresses: this computer's, and the public one the installer is
    // about to set up (without a trailing slash).
    assert_eq!(v["app"]["url"], format!("http://localhost:{port}"), "{v}");
    assert_eq!(
        v["app"]["public_url"], "https://arr-test.ngrok-free.app",
        "{v}"
    );
    // Until setup has finished there is no app to sign in to.
    let (s, _) = page.post("/api/app-link", json!({})).await;
    assert_eq!(s, StatusCode::CONFLICT);

    let finished = || async {
        let mut last = Value::Null;
        for _ in 0..120 {
            let (_, v) = page.get("/api/state").await;
            last = v["phase"].clone();
            if last["state"] != "working" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        last
    };

    // Finish clicked in two tabs at once: only one starts setup. Here the import fails, so
    // no config.toml appears (the installers would take it as set up).
    world.lock().unwrap().import_fails = true;
    let (a, b) = tokio::join!(
        page.post("/api/apply", choices("1:0")),
        page.post("/api/apply", choices("1:0"))
    );
    let mut codes = [a.0, b.0];
    codes.sort();
    assert_eq!(codes, [StatusCode::OK, StatusCode::CONFLICT], "{a:?} {b:?}");
    let last = finished().await;
    assert_eq!(last["state"], "failed", "{last}");
    assert!(
        last["message"]
            .as_str()
            .unwrap()
            .contains("the Plex import failed"),
        "{last}"
    );
    assert!(!state_dir.join("config.toml").exists());
    assert!(state_dir.join("config.toml.failed").exists());

    // Finish again tries again.
    world.lock().unwrap().import_fails = false;
    let (s, v) = page.post("/api/apply", choices("1:0")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let last = finished().await;
    assert_eq!(last["state"], "done", "{last}");
    assert!(!state_dir.join("config.toml.new").exists());
    assert!(last.get("no_tmdb").is_none(), "{last}");
    let (s, _) = page.post("/api/apply", choices("1:0")).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // "Open Arr": a one-time link for the config just written, made on request. Its token
    // is kept (hashed) for the service to consume; each request makes a new one.
    let (s, v) = page.post("/api/app-link", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let link = v["url"].as_str().unwrap();
    let prefix = format!("http://localhost:{port}/app/link?t=");
    let token = link
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("{link}"));
    let (_, v2) = page.post("/api/app-link", json!({})).await;
    assert_ne!(v2["url"].as_str().unwrap(), link);
    let db = rusqlite::Connection::open(state_dir.join("web.sqlite")).unwrap();
    let live = |t: &str| -> i64 {
        db.query_row(
            "SELECT COUNT(*) FROM links WHERE hash = ?1",
            [hex::encode(<sha2::Sha256 as sha2::Digest>::digest(
                t.as_bytes(),
            ))],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(live(token), 1, "the link wasn't saved for the service");
    assert!(
        !std::fs::read_to_string(state_dir.join("config.toml"))
            .unwrap()
            .contains(token)
    );
    let (s, _) = page.post("/api/finish", json!({})).await;
    assert_eq!(s, StatusCode::OK);
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());

    // What the installer picks up.
    let config = std::fs::read_to_string(state_dir.join("config.toml")).unwrap();
    for want in [
        format!("listen = \"127.0.0.1:{port}\""),
        format!("base_url = \"http://127.0.0.1:{}/\"", mock.port()),
        "movie_section = 1".into(),
        "tv_section = 2".into(),
        "[execution]\nenabled = true".into(),
        "[tmdb]\nenabled = false".into(),
        "[reporting]\nenabled = true".into(),
    ] {
        assert!(config.contains(&want), "missing {want:?} in:\n{config}");
    }
    // Nothing a previous Arr version would refuse: no Plex metadata addresses, no TMDB key.
    for never in [
        "discover_url",
        "metadata_url",
        "image_base_url",
        "credential_file",
    ] {
        assert!(!config.contains(never), "{never} in:\n{config}");
    }
    // Paths are compared as values: TOML spells Windows paths differently.
    let parsed: toml::Table = toml::from_str(&config).unwrap();
    for (table, key, want) in [
        ("library", "movies", &movies),
        ("plex", "tv_path", &tv),
        ("embedded", "download_dir", &downloads),
    ] {
        assert_eq!(parsed[table][key].as_str(), want.to_str(), "{table}.{key}");
    }
    assert!(downloads.is_dir());
    assert_eq!(
        std::fs::read_to_string(state_dir.join("secrets/plex_token")).unwrap(),
        SERVER_TOKEN
    );
    assert_eq!(
        std::fs::read_to_string(state_dir.join("setup-choices")).unwrap(),
        "auto_update=1\n"
    );
    #[cfg(unix)]
    for f in [
        "config.toml",
        "secrets/plex_token",
        "secrets/api_token",
        "secrets/mcp_token",
    ] {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(state_dir.join(f))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{f} is {mode:o}");
    }
    let check = std::process::Command::new(BIN)
        .arg("check")
        .arg("--config")
        .arg(state_dir.join("config.toml"))
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    // The account token was only used with plex.tv; the server got its own token.
    let w = world.lock().unwrap();
    for (path, token) in &w.tokens {
        let want = if path == "resources" {
            ACCOUNT_TOKEN
        } else {
            SERVER_TOKEN
        };
        assert_eq!(token, want, "{path}");
    }
    // The import read both libraries.
    assert_eq!(w.tokens.iter().filter(|(p, _)| p == "all").count(), 2);
    let id = std::fs::read_to_string(state_dir.join("secrets/plex_client_id")).unwrap();
    assert!(!w.client_ids.is_empty() && w.client_ids.iter().all(|c| *c == id));
}

#[tokio::test]
async fn an_existing_config_is_never_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("config.toml"), "# mine\n").unwrap();
    let out = tokio::process::Command::new(BIN)
        .args([
            "wizard",
            "--no-docker",
            "--ffprobe",
            ffprobe().to_str().unwrap(),
            "--port",
        ])
        .arg(free_port().to_string())
        .arg("--state-dir")
        .arg(tmp.path())
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("already exists") && err.contains("choose to delete Arr's data"),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("config.toml")).unwrap(),
        "# mine\n"
    );
}

#[tokio::test]
async fn only_the_accounts_plex_on_this_computer_is_used() {
    let tmp = tempfile::tempdir().unwrap();
    let (mock, world) = start_mock(tmp.path(), tmp.path()).await;
    // The Plex here belongs to someone else (or isn't signed in yet).
    world.lock().unwrap().identity = "someone-else".into();
    world.lock().unwrap().resources_calls = 1; // no plex.tv hiccup in this test
    let (_child, link, port) = start_wizard(&tmp.path().join("state"), mock, &[]).await;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let r = http
        .get(link.replace("localhost", "127.0.0.1"))
        .send()
        .await
        .unwrap();
    let cookie = r.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let page = Page {
        base: format!("http://127.0.0.1:{port}"),
        cookie,
        http,
    };
    page.post("/api/plex/signin", json!({})).await;
    page.post("/api/plex/check", json!({})).await;
    let (s, v) = page.post("/api/plex/check", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["signed_in"], true);
    assert_eq!(v["plex"], Value::Null);
    assert_eq!(v["missing"], "not_linked");
    // Signing in again (with another account, say) starts over.
    let (s, v) = page.post("/api/plex/signin", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, v) = page.get("/api/state").await;
    assert_eq!(v["signed_in"], false, "{v}");
    assert_eq!(v["missing"], Value::Null, "{v}");
    let (_, v) = page.post("/api/plex/check", json!({})).await;
    assert_eq!(v["missing"], "not_linked", "{v}");
    // Once that Plex is signed in to this account, looking again connects.
    world.lock().unwrap().identity = MACHINE.into();
    let (_, v) = page.post("/api/plex/check", json!({})).await;
    assert_eq!(v["missing"], Value::Null);
    assert_eq!(v["plex"]["name"], "Living Room");
    assert_eq!(v["plex"]["movies"].as_array().unwrap().len(), 1);
    // A library added in Plex shows up on the next look.
    world.lock().unwrap().added_library = Some("/films".into());
    let (s, v) = page.post("/api/plex/check", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["plex"]["movies"][1]["title"], "Films", "{v}");
}

/// Opens the page with the link's code; returns a client holding the session cookie.
async fn open_page(link: &str, port: u16) -> Page {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let r = http
        .get(link.replace("localhost", "127.0.0.1"))
        .send()
        .await
        .unwrap();
    let cookie = r.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    Page {
        base: format!("http://127.0.0.1:{port}"),
        cookie,
        http,
    }
}

#[cfg(unix)] // the stand-in installer is a shell command
#[tokio::test]
async fn a_computer_without_plex_gets_it_installed_linked_and_libraries_made() {
    let tmp = tempfile::tempdir().unwrap();
    let (mock, world) = start_mock(tmp.path(), tmp.path()).await;
    {
        let mut w = world.lock().unwrap();
        w.not_installed = true;
        w.resources_calls = 1; // no plex.tv hiccup in this test
    }
    // Stands in for `pkexec arr plex-install`: reports a step, then Plex is "installed".
    let installer = format!(
        "echo 'step: Installing Plex Media Server'; curl -fsS -X POST http://{mock}/test/install"
    );
    let extra = [
        format!("--plex-installer={installer}"),
        format!("--plex-port={}", mock.port()),
        // The installer's --error-reports: the box starts ticked.
        "--error-reports".into(),
    ];
    let (_child, link, port) = start_wizard(&tmp.path().join("state"), mock, &extra).await;
    let page = open_page(&link, port).await;
    let (_, v) = page.get("/api/state").await;
    assert_eq!(v["reporting"]["enabled"], true, "{v}");
    page.post("/api/plex/signin", json!({})).await;
    page.post("/api/plex/check", json!({})).await;
    let (_, v) = page.post("/api/plex/check", json!({})).await;
    assert_eq!(v["missing"], "not_installed", "{v}");
    assert_eq!(v["can_install"], true);
    assert_eq!(v["install"]["state"], "idle");

    let (s, v) = page.post("/api/plex/install", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let mut install = Value::Null;
    for _ in 0..60 {
        let (_, v) = page.get("/api/state").await;
        install = v["install"].clone();
        if install["state"] != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(install["state"], "done", "{install}");

    // The next look finds a new, unlinked Plex, links it to the account and connects. Two
    // looks at once (two tabs) link it only once.
    let ((s, v), (s2, v2)) = tokio::join!(
        page.post("/api/plex/check", json!({})),
        page.post("/api/plex/check", json!({}))
    );
    assert_eq!((s, s2), (StatusCode::OK, StatusCode::OK), "{v} {v2}");
    assert_eq!(world.lock().unwrap().claims, 1);
    assert_eq!(v["plex"]["name"], "Living Room", "{v}");
    assert_eq!(v2["plex"]["name"], "Living Room", "{v2}");
    assert_eq!(v["plex"]["movies"], json!([]));

    // It has no libraries, so the page makes them. Here they're in a private folder (like a
    // home folder), which Plex's own user can't open.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let movies = tmp.path().join("media/Movies");
    let tv = tmp.path().join("media/TV Shows");
    // A bad folder is refused before Plex is asked for anything, so a retry can't duplicate.
    let (s, v) = page
        .post(
            "/api/plex/libraries",
            json!({ "movies": movies, "tv": "media/TV Shows" }),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(world.lock().unwrap().created.is_empty());
    let (s, v) = page
        .post(
            "/api/plex/libraries",
            json!({ "movies": movies, "tv": tv, "language": "en-AU" }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(movies.is_dir() && tv.is_dir());
    assert_eq!(v["plex"]["movies"][0]["local_path"], json!(movies));
    assert_eq!(v["plex"]["tv"][0]["title"], "TV Shows");
    let created = world.lock().unwrap().created.clone();
    assert_eq!(
        created,
        vec![
            (
                "movie".into(),
                movies.to_str().unwrap().into(),
                "en-AU".into()
            ),
            ("show".into(), tv.to_str().unwrap().into(), "en-AU".into()),
        ]
    );
    // The temp folder is private, so the page warns that Plex may not see it.
    assert!(
        v["warnings"][0]
            .as_str()
            .unwrap()
            .contains("Plex may not be able to see"),
        "{v}"
    );
}

#[tokio::test]
async fn a_late_answer_for_an_earlier_signin_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let (mock, world) = start_mock(tmp.path(), tmp.path()).await;
    world.lock().unwrap().slow_first_pin = true;
    let (_child, link, port) = start_wizard(&tmp.path().join("state"), mock, &[]).await;
    let page = open_page(&link, port).await;
    page.post("/api/plex/signin", json!({})).await;
    // plex.tv is slow to say PIN 42 signed in; meanwhile the user signs in again.
    let slow = {
        let page = page.clone();
        tokio::spawn(async move { page.post("/api/plex/check", json!({})).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (s, v) = page.post("/api/plex/signin", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    slow.await.unwrap();
    // The old account isn't signed in: the page waits for the new sign-in.
    let (_, v) = page.get("/api/state").await;
    assert_eq!(v["signed_in"], false, "{v}");
    assert_eq!(v["waiting_for_signin"], true, "{v}");
}

#[test]
fn an_upload_limit_the_config_cant_hold_is_refused() {
    // 4194303 KiB/s is the most bytes per second a u32 holds, as with `arr set`.
    let out = std::process::Command::new(BIN)
        .args(["wizard", "--state-dir", "s", "--ffprobe", "f"])
        .args(["--upload-limit-kib", "4194304"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("4194304"));
}
