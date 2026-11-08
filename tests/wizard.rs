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
                axum::Json(json!({ "id": 42, "code": "pin-code", "authToken": null }))
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
                    if id != 42 || q.get("code").map(String::as_str) != Some("pin-code") {
                        return StatusCode::NOT_FOUND.into_response();
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

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
        .args(["--ffprobe", "/usr/bin/ffprobe"])
        .arg(format!("--plex-tv-url=http://{mock}/"))
        .arg(format!("--plex-auth-url=http://{mock}/auth"))
        .args(extra)
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
    let (mut child, link, port) = start_wizard(&state_dir, mock, &[]).await;
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
            "tmdb": "",
            "downloading": false,
            "auto_update": true,
        })
    };
    let (s, v) = page.post("/api/apply", choices("3:0")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(!state_dir.join("config.toml").exists());

    let (s, v) = page.post("/api/apply", choices("1:0")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let mut last = Value::Null;
    for _ in 0..120 {
        let (_, v) = page.get("/api/state").await;
        last = v["phase"].clone();
        if last["state"] != "working" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_eq!(last["state"], "done", "{last}");
    let (s, _) = page.post("/api/apply", choices("1:0")).await;
    assert_eq!(s, StatusCode::CONFLICT);
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
        format!("movies = {:?}", movies.to_str().unwrap()),
        format!("tv_path = {:?}", tv.to_str().unwrap()),
        format!("download_dir = {:?}", downloads.to_str().unwrap()),
        "[execution]\nenabled = false".into(),
        "[tmdb]\nenabled = false".into(),
    ] {
        assert!(config.contains(&want), "missing {want:?} in:\n{config}");
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
            "/usr/bin/ffprobe",
            "--port",
        ])
        .arg(free_port().to_string())
        .arg("--state-dir")
        .arg(tmp.path())
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already exists"));
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
    // Once that Plex is signed in to this account, looking again connects.
    world.lock().unwrap().identity = MACHINE.into();
    let (_, v) = page.post("/api/plex/check", json!({})).await;
    assert_eq!(v["missing"], Value::Null);
    assert_eq!(v["plex"]["name"], "Living Room");
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
    ];
    let (_child, link, port) = start_wizard(&tmp.path().join("state"), mock, &extra).await;
    let page = open_page(&link, port).await;
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

    // The next look finds a new, unlinked Plex, links it to the account and connects.
    let (s, v) = page.post("/api/plex/check", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(world.lock().unwrap().claims, 1);
    assert_eq!(v["plex"]["name"], "Living Room", "{v}");
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
