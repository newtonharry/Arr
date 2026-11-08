//! First-run setup in a web browser (`arr wizard`).
//!
//! The installer starts this before Arr has a config. It serves one page (see `wizard/`)
//! that signs in to Plex, finds the Plex on this computer (Arr moves files into Plex's
//! folders, so it only works with the Plex beside it), picks libraries and folders, then
//! writes the same config `install.sh` writes, runs `arr setup`, `arr check` and the Plex
//! import, and exits. The installer then installs the service as before.
//!
//! Access needs the link printed in the terminal: its secret code is swapped for an
//! HttpOnly, SameSite=Strict cookie, and changes also need a custom header, so other sites
//! can't drive it. Plex tokens stay on this side; the browser only sees the plex.tv sign-in
//! link. On a machine reached over SSH, the installer binds the address the SSH client used,
//! so the link opens from the computer you're typing on.

use crate::config::{PlexConfig, Secret};
use crate::fsutil;
use crate::plex::{Plex, Section};
use crate::upstream;
use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::{Json, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Notify;

const INDEX_HTML: &str = include_str!("wizard/index.html");
const APP_JS: &str = include_str!("wizard/app.js");
const APP_CSS: &str = include_str!("wizard/app.css");

const COOKIE: &str = "arr_setup";
/// Every change must carry this header; a plain cross-site form can't set it.
const CHANGE_HEADER: &str = "x-arr-setup";
/// The wizard gives up if setup isn't finished in this time.
const LIFETIME: Duration = Duration::from_secs(2 * 60 * 60);
/// After setup finishes, the wizard waits this long for the page to say goodbye.
const LINGER: Duration = Duration::from_secs(60);
const PLEX_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_PLEX_JSON: usize = 2 * 1024 * 1024;
/// Between looks at plex.tv after linking a new Plex.
const CLAIM_WAIT: Duration = Duration::from_secs(1);
/// Where a Plex installed from the page keeps movies and shows (Movies, TV Shows).
const MEDIA: &str = "/srv/media";

pub struct Options {
    pub state_dir: PathBuf,
    /// The wizard listens on `bind:port`; Arr's API will use `127.0.0.1:port` afterwards.
    pub bind: IpAddr,
    pub port: u16,
    pub torrent_port: u16,
    pub upload_limit_kib: u64,
    pub ffprobe: PathBuf,
    /// The `arr` binary that runs `setup`, `check` and `bootstrap`.
    pub arr: PathBuf,
    pub plex_tv: Url,
    pub plex_auth: Url,
    /// Look for a Plex container (`docker ps`) to translate its folders to this machine's.
    pub docker: bool,
    /// Open the link in this computer's browser too.
    pub open: bool,
    /// Tests: a shell command run instead of installing Plex as administrator.
    pub plex_installer: Option<String>,
    /// Where Plex listens on this computer (32400).
    pub plex_port: u16,
}

struct Wizard {
    opts: Options,
    code: String,
    session: String,
    client_id: String,
    http: reqwest::Client,
    inner: Mutex<Inner>,
    finished: Notify,
}

#[derive(Default)]
struct Inner {
    pin: Option<(u64, String)>,
    account: Option<Secret>,
    plex: Option<Connected>,
    /// Set when the last look found no Plex on this computer that Arr can use.
    missing: Option<Missing>,
    install: Install,
    /// The port of an unclaimed Plex found on this computer.
    unclaimed_port: Option<u16>,
    phase: Phase,
}

/// Installing Plex from the page (`arr plex-install` as administrator).
#[derive(Clone, Default, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Install {
    #[default]
    Idle,
    Running {
        step: String,
    },
    Done,
    Failed {
        message: String,
    },
}

/// Why the Plex on this computer can't be used.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Missing {
    /// Nothing answers as Plex on this computer.
    NotInstalled,
    /// Plex answers, but it isn't one of the signed-in account's servers.
    NotLinked,
    /// It's the account's server, but its libraries couldn't be read (still starting?).
    Unreadable,
    /// A new Plex that isn't linked to any account yet; the page links it.
    Unclaimed,
}

/// A Plex server the signed-in account owns.
struct Server {
    id: String,
    name: String,
    token: Secret,
    ports: Vec<u16>,
}

#[derive(Clone)]
struct Connected {
    name: String,
    base_url: Url,
    token: Secret,
    sections: Vec<Section>,
    /// (container path, host path) of a Plex container's mounts, longest first.
    mounts: Vec<(String, String)>,
}

#[derive(Clone, Default, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Phase {
    #[default]
    Editing,
    Working {
        step: String,
        detail: String,
    },
    Done {
        downloading: bool,
    },
    Failed {
        message: String,
    },
}

type St = State<Arc<Wizard>>;

/// Runs the wizard until setup finishes (Ok) or is abandoned (Err).
pub async fn run(opts: Options) -> Result<()> {
    let config = opts.state_dir.join("config.toml");
    if config.exists() {
        bail!(
            "{} already exists; Arr is set up (remove it to start over)",
            config.display()
        );
    }
    fsutil::ensure_private_dir(&opts.state_dir)?;
    fsutil::ensure_private_dir(&opts.state_dir.join("secrets"))?;
    // Plex lists Arr once under Authorized Devices, however often setup runs.
    let id_file = opts.state_dir.join("secrets/plex_client_id");
    if !id_file.exists() {
        fsutil::write_new_private_file(&id_file, fsutil::random_hex(16)?.as_bytes())?;
    }
    let client_id = fsutil::read_secret_file(&id_file)?;
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(opts.bind, opts.port))
        .await
        .with_context(|| {
            format!(
                "can't listen on {}:{} (is Arr already running?)",
                opts.bind, opts.port
            )
        })?;
    let wiz = Arc::new(Wizard {
        code: fsutil::random_hex(16)?,
        session: fsutil::random_hex(32)?,
        client_id,
        http: upstream::http_client(PLEX_TIMEOUT, reqwest::redirect::Policy::none()),
        inner: Mutex::new(Inner::default()),
        finished: Notify::new(),
        opts,
    });
    let host = if wiz.opts.bind.is_loopback() {
        "localhost".to_string()
    } else {
        match wiz.opts.bind {
            IpAddr::V6(v6) => format!("[{v6}]"),
            v4 => v4.to_string(),
        }
    };
    let link = format!("http://{host}:{}/?code={}", wiz.opts.port, wiz.code);
    println!("\nOpen this link in your web browser to set up Arr:\n");
    println!("    {link}\n");
    if wiz.opts.open {
        // Windows: `start` would treat the link's `&` as a command separator.
        let (opener, args): (&str, &[&str]) = if cfg!(windows) {
            ("rundll32", &["url.dll,FileProtocolHandler"])
        } else if cfg!(target_os = "macos") {
            ("open", &[])
        } else {
            ("xdg-open", &[])
        };
        let _ = Command::new(opener)
            .args(args)
            .arg(&link)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    println!("Waiting for setup to finish (press Ctrl+C to cancel)...");
    let done = wiz.clone();
    axum::serve(listener, router(wiz.clone()))
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = done.finished.notified() => {}
                _ = tokio::time::sleep(LIFETIME) => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        })
        .await?;
    match wiz.inner.lock().unwrap().phase.clone() {
        Phase::Done { .. } => Ok(()),
        _ => bail!("setup wasn't finished"),
    }
}

fn router(wiz: Arc<Wizard>) -> Router {
    let api = Router::new()
        .route("/api/state", get(state))
        .route("/api/plex/signin", post(signin))
        .route("/api/plex/check", post(check_signin))
        .route("/api/plex/install", post(install_plex))
        .route("/api/plex/libraries", post(create_libraries))
        .route("/api/folder", post(folder))
        .route("/api/apply", post(apply))
        .route("/api/finish", post(finish))
        .route_layer(middleware::from_fn_with_state(wiz.clone(), guard));
    Router::new()
        .route("/", get(index))
        .route(
            "/app.js",
            get(|| async { asset("text/javascript", APP_JS) }),
        )
        .route("/app.css", get(|| async { asset("text/css", APP_CSS) }))
        .merge(api)
        .layer(middleware::from_fn(secure_headers))
        .with_state(wiz)
}

fn asset(kind: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, kind)], body).into_response()
}

async fn secure_headers(req: axum::extract::Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
        ),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // Nothing here needs a Referer, and it keeps addresses out of other sites' logs.
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn has_session(wiz: &Wizard, headers: &HeaderMap) -> bool {
    use subtle::ConstantTimeEq;
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().strip_prefix(COOKIE)?.strip_prefix('='))
        .any(|v| bool::from(v.as_bytes().ct_eq(wiz.session.as_bytes())))
}

async fn guard(State(wiz): St, req: axum::extract::Request, next: Next) -> Response {
    if !has_session(&wiz, req.headers()) {
        return problem(
            StatusCode::UNAUTHORIZED,
            "open the link printed in the terminal",
        );
    }
    if req.method() != axum::http::Method::GET && !req.headers().contains_key(CHANGE_HEADER) {
        return problem(StatusCode::FORBIDDEN, "missing the setup header");
    }
    next.run(req).await
}

fn problem(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[derive(Deserialize)]
struct CodeQuery {
    code: Option<String>,
}

async fn index(State(wiz): St, Query(q): Query<CodeQuery>, headers: HeaderMap) -> Response {
    use subtle::ConstantTimeEq;
    if let Some(code) = q.code {
        if bool::from(code.as_bytes().ct_eq(wiz.code.as_bytes())) {
            let cookie = format!(
                "{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
                wiz.session,
                LIFETIME.as_secs()
            );
            let mut resp = Redirect::to("/").into_response();
            if let Ok(v) = HeaderValue::from_str(&cookie) {
                resp.headers_mut().insert(header::SET_COOKIE, v);
            }
            return resp;
        }
        return (
            StatusCode::UNAUTHORIZED,
            page_text(
                "That setup link has expired or is wrong. Use the link printed in the terminal.",
            ),
        )
            .into_response();
    }
    if !has_session(&wiz, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            page_text("Open the link printed in the terminal (it includes a secret code)."),
        )
            .into_response();
    }
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        INDEX_HTML,
    )
        .into_response()
}

fn page_text(msg: &'static str) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("Arr setup\n\n{msg}\n"),
    )
        .into_response()
}

// ------------------------------------------------------------------ state

async fn state(State(wiz): St) -> Json<Value> {
    Json(snapshot(&wiz))
}

fn snapshot(wiz: &Wizard) -> Value {
    let inner = wiz.inner.lock().unwrap();
    let plex = inner.plex.as_ref().map(|p| {
        let libs = |kind: &str| -> Vec<Value> {
            p.sections
                .iter()
                .filter(|s| s.kind == kind)
                .flat_map(|s| {
                    s.locations.iter().enumerate().map(move |(i, loc)| {
                        let local = local_path(p, loc);
                        json!({
                            "id": format!("{}:{}", s.key, i),
                            "title": s.title,
                            "plex_path": loc,
                            "local_path": local,
                        })
                    })
                })
                .collect()
        };
        json!({
            "name": p.name,
            "movies": libs("movie"),
            "tv": libs("show"),
        })
    });
    json!({
        "signed_in": inner.account.is_some(),
        "waiting_for_signin": inner.pin.is_some() && inner.account.is_none(),
        "plex": plex,
        "missing": inner.missing,
        "install": inner.install,
        "can_install": install_command(wiz).is_some(),
        "install_command": manual_install_command(wiz),
        "media_folder": MEDIA,
        "phase": inner.phase,
    })
}

/// Where this machine sees a folder Plex reports, if it can tell.
fn local_path(p: &Connected, plex_path: &str) -> Option<String> {
    for (dst, src) in &p.mounts {
        if let Some(rest) = plex_path.strip_prefix(dst.as_str())
            && (rest.is_empty() || rest.starts_with('/'))
        {
            return Some(format!("{src}{rest}"));
        }
    }
    Path::new(plex_path).is_dir().then(|| plex_path.to_string())
}

// ------------------------------------------------------------------ Plex sign-in

fn plex_tv(wiz: &Wizard, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    rb.header("Accept", "application/json")
        .header("X-Plex-Product", "Arr")
        .header("X-Plex-Version", env!("CARGO_PKG_VERSION"))
        .header("X-Plex-Client-Identifier", &wiz.client_id)
}

#[derive(Deserialize)]
struct Pin {
    id: u64,
    code: String,
    #[serde(rename = "authToken", default)]
    auth_token: Option<String>,
}

async fn signin(State(wiz): St) -> Response {
    let url = match wiz.opts.plex_tv.join("api/v2/pins?strong=true") {
        Ok(u) => u,
        Err(_) => return problem(StatusCode::INTERNAL_SERVER_ERROR, "bad plex.tv address"),
    };
    let pin: Pin = match upstream::send(plex_tv(&wiz, wiz.http.post(url))).await {
        Ok(r) => match upstream::read_json(r, MAX_PLEX_JSON).await {
            Ok(p) => p,
            Err(e) => return plex_down(e),
        },
        Err(e) => return plex_down(e),
    };
    let mut auth = wiz.opts.plex_auth.clone();
    auth.set_fragment(Some(&format!(
        "?clientID={}&code={}&context%5Bdevice%5D%5Bproduct%5D=Arr",
        urlencode(&wiz.client_id),
        urlencode(&pin.code)
    )));
    wiz.inner.lock().unwrap().pin = Some((pin.id, pin.code));
    Json(json!({ "url": auth.as_str() })).into_response()
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn plex_down(e: upstream::UpstreamError) -> Response {
    problem(
        StatusCode::BAD_GATEWAY,
        &format!("couldn't reach plex.tv: {}", e.describe()),
    )
}

/// Polls the sign-in once; once signed in, looks for this computer's Plex.
async fn check_signin(State(wiz): St) -> Response {
    let (pin, signed_in) = {
        let inner = wiz.inner.lock().unwrap();
        (inner.pin.clone(), inner.account.is_some())
    };
    if !signed_in {
        let Some((id, code)) = pin else {
            return Json(snapshot(&wiz)).into_response();
        };
        let url = match wiz
            .opts
            .plex_tv
            .join(&format!("api/v2/pins/{id}?code={}", urlencode(&code)))
        {
            Ok(u) => u,
            Err(_) => return problem(StatusCode::INTERNAL_SERVER_ERROR, "bad plex.tv address"),
        };
        let resp = match upstream::send(plex_tv(&wiz, wiz.http.get(url))).await {
            Ok(r) => r,
            Err(e) => return plex_down(e),
        };
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            wiz.inner.lock().unwrap().pin = None;
            return problem(
                StatusCode::GONE,
                "the Plex sign-in expired; click Sign in again",
            );
        }
        let pin: Pin = match upstream::read_json(resp, MAX_PLEX_JSON).await {
            Ok(p) => p,
            Err(e) => return plex_down(e),
        };
        match pin.auth_token.filter(|t| !t.is_empty()) {
            Some(t) => wiz.inner.lock().unwrap().account = Some(Secret::new(t)),
            None => return Json(snapshot(&wiz)).into_response(),
        }
    }
    // Until Plex is found, every check looks again, so a failed lookup or a Plex
    // installed meanwhile is picked up.
    if wiz.inner.lock().unwrap().plex.is_none() {
        let servers = match servers(&wiz).await {
            Ok(s) => s,
            Err(e) => return plex_down(e),
        };
        let mut missing = find_local(&wiz, &servers).await.err();
        if let Some(Missing::Unclaimed) = missing {
            // A new Plex: link it to this account, then wait for plex.tv to list it.
            if let Err(msg) = claim(&wiz).await {
                return problem(StatusCode::BAD_GATEWAY, &msg);
            }
            for _ in 0..15 {
                tokio::time::sleep(CLAIM_WAIT).await;
                let Ok(now) = self::servers(&wiz).await else {
                    continue;
                };
                missing = find_local(&wiz, &now).await.err();
                if !matches!(missing, Some(Missing::Unclaimed | Missing::NotLinked)) {
                    break;
                }
            }
        }
        wiz.inner.lock().unwrap().missing = missing;
    }
    Json(snapshot(&wiz)).into_response()
}

#[derive(Deserialize)]
struct ClaimToken {
    token: String,
}

/// Links the unclaimed Plex on this computer to the signed-in account.
async fn claim(wiz: &Wizard) -> Result<(), String> {
    let account = wiz.inner.lock().unwrap().account.clone();
    let Some(account) = account else {
        return Err("sign in to Plex first".into());
    };
    let url = wiz
        .opts
        .plex_tv
        .join("api/claim/token.json")
        .map_err(|_| "bad plex.tv address".to_string())?;
    let token: ClaimToken = match upstream::send(
        plex_tv(wiz, wiz.http.get(url)).header("X-Plex-Token", account.expose()),
    )
    .await
    {
        Ok(r) => upstream::read_json(r, 64 * 1024)
            .await
            .map_err(|e| format!("couldn't link Plex to your account: {}", e.describe()))?,
        Err(e) => return Err(format!("couldn't reach plex.tv: {}", e.describe())),
    };
    let port = wiz
        .inner
        .lock()
        .unwrap()
        .unclaimed_port
        .unwrap_or(wiz.opts.plex_port);
    let url = format!(
        "http://127.0.0.1:{port}/myplex/claim?token={}",
        urlencode(&token.token)
    );
    let r = upstream::send(plex_tv(wiz, wiz.http.post(url)))
        .await
        .map_err(|e| format!("couldn't link Plex to your account: {}", e.describe()))?;
    if !r.status().is_success() {
        return Err(format!(
            "Plex didn't accept the link to your account ({})",
            r.status()
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct Resource {
    #[serde(default)]
    name: String,
    #[serde(default)]
    provides: String,
    #[serde(default)]
    owned: bool,
    #[serde(rename = "clientIdentifier", default)]
    client_identifier: String,
    #[serde(rename = "accessToken", default)]
    access_token: Option<String>,
    #[serde(default)]
    connections: Vec<Connection>,
}

#[derive(Deserialize)]
struct Connection {
    #[serde(default)]
    port: u16,
}

/// The Plex servers this account owns, with the ports they listen on.
async fn servers(wiz: &Wizard) -> Result<Vec<Server>, upstream::UpstreamError> {
    let account = wiz.inner.lock().unwrap().account.clone();
    let Some(account) = account else {
        return Ok(vec![]);
    };
    let url = wiz
        .opts
        .plex_tv
        .join("api/v2/resources?includeHttps=1&includeRelay=0")
        .map_err(|_| upstream::UpstreamError::BadResponse {
            reason: "bad plex.tv address".into(),
        })?;
    let resp =
        upstream::send(plex_tv(wiz, wiz.http.get(url)).header("X-Plex-Token", account.expose()))
            .await?;
    let list: Vec<Resource> = upstream::read_json(resp, MAX_PLEX_JSON).await?;
    Ok(list
        .into_iter()
        .filter(|r| r.owned && r.provides.split(',').any(|p| p.trim() == "server"))
        .filter_map(|r| {
            let token = r.access_token.filter(|t| !t.is_empty())?;
            Some(Server {
                id: r.client_identifier,
                name: if r.name.is_empty() {
                    "Plex".into()
                } else {
                    r.name
                },
                token: Secret::new(token),
                ports: r
                    .connections
                    .iter()
                    .map(|c| c.port)
                    .filter(|p| *p > 0)
                    .collect(),
            })
        })
        .take(20)
        .collect())
}

#[derive(Deserialize)]
struct Identity {
    #[serde(rename = "MediaContainer")]
    container: IdentityContainer,
}

#[derive(Deserialize)]
struct IdentityContainer {
    #[serde(rename = "machineIdentifier", default)]
    machine_identifier: String,
    /// False on a new Plex that no account owns yet.
    #[serde(default)]
    claimed: Option<bool>,
}

/// Connects to the Plex running on this computer, if it's one of the account's servers.
/// Arr moves files into Plex's folders, so it only works with the Plex beside it.
async fn find_local(wiz: &Wizard, servers: &[Server]) -> Result<(), Missing> {
    let mut ports: Vec<u16> = servers
        .iter()
        .flat_map(|s| s.ports.iter().copied())
        .collect();
    ports.push(wiz.opts.plex_port);
    ports.sort_unstable();
    ports.dedup();
    let mut answered = false;
    let mut unreadable = false;
    for port in ports {
        let Ok(base) = Url::parse(&format!("http://127.0.0.1:{port}/")) else {
            continue;
        };
        let Ok(url) = base.join("identity") else {
            continue;
        };
        let Ok(r) = upstream::send(wiz.http.get(url).header("Accept", "application/json")).await
        else {
            continue;
        };
        let Ok(id) = upstream::read_json::<Identity>(r, 64 * 1024).await else {
            continue;
        };
        answered = true;
        if id.container.claimed == Some(false) {
            wiz.inner.lock().unwrap().unclaimed_port = Some(port);
            return Err(Missing::Unclaimed);
        }
        let Some(server) = servers
            .iter()
            .find(|s| s.id == id.container.machine_identifier)
        else {
            continue;
        };
        let Ok(sections) = sections(base.clone(), server.token.clone()).await else {
            unreadable = true;
            continue;
        };
        let mounts = if wiz.opts.docker {
            docker_mounts().await
        } else {
            vec![]
        };
        wiz.inner.lock().unwrap().plex = Some(Connected {
            name: server.name.clone(),
            base_url: base,
            token: server.token.clone(),
            sections,
            mounts,
        });
        return Ok(());
    }
    Err(if unreadable {
        Missing::Unreadable
    } else if answered {
        Missing::NotLinked
    } else {
        Missing::NotInstalled
    })
}

async fn sections(base_url: Url, token: Secret) -> Result<Vec<Section>, upstream::UpstreamError> {
    Plex::new(PlexConfig {
        base_url,
        token,
        movie_section: None,
        tv_section: None,
        movies_path: None,
        tv_path: None,
        timeout: PLEX_TIMEOUT,
    })
    .sections()
    .await
}

// ------------------------------------------------------------------ installing Plex

/// How this page installs Plex as administrator: the system's password dialog running
/// `arr plex-install`. None where that isn't possible (no apt/dnf, no dialog, no desktop).
fn install_command(wiz: &Wizard) -> Option<Vec<String>> {
    if let Some(cmd) = &wiz.opts.plex_installer {
        return Some(vec!["sh".into(), "-c".into(), cmd.clone()]);
    }
    crate::plexinstall::package_manager()?;
    let desktop =
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    if !desktop || !Path::new("/usr/bin/pkexec").is_file() {
        return None;
    }
    let mut cmd = vec![
        "/usr/bin/pkexec".to_string(),
        wiz.opts.arr.to_string_lossy().into_owned(),
    ];
    cmd.extend(["plex-install".into(), "--media".into(), MEDIA.into()]);
    if let Some(owner) = owner() {
        cmd.extend(["--owner".into(), owner]);
    }
    Some(cmd)
}

/// The same, for pasting into a terminal where the page can't ask for the password itself.
fn manual_install_command(wiz: &Wizard) -> Option<String> {
    crate::plexinstall::package_manager()?;
    let arr = wiz.opts.arr.to_string_lossy();
    let arr = if arr.contains([' ', '\'', '"']) {
        format!("'{}'", arr.replace('\'', "'\\''"))
    } else {
        arr.into_owned()
    };
    Some(match owner() {
        Some(o) => format!("sudo {arr} plex-install --owner {o}"),
        None => format!("sudo {arr} plex-install"),
    })
}

/// UID:GID of the user running the page, who gets the new media folders.
fn owner() -> Option<String> {
    #[cfg(unix)]
    {
        // SAFETY: getuid and getgid can't fail.
        Some(format!("{}:{}", unsafe { libc::getuid() }, unsafe {
            libc::getgid()
        }))
    }
    #[cfg(not(unix))]
    None
}

async fn install_plex(State(wiz): St) -> Response {
    let Some(cmd) = install_command(&wiz) else {
        return problem(
            StatusCode::BAD_REQUEST,
            "Arr can't ask for your password here; run the command shown on the page in a terminal",
        );
    };
    {
        let mut inner = wiz.inner.lock().unwrap();
        if matches!(inner.install, Install::Running { .. }) {
            return problem(StatusCode::CONFLICT, "Plex is already being installed");
        }
        inner.install = Install::Running {
            step: "Waiting for your password".into(),
        };
    }
    let w = wiz.clone();
    tokio::spawn(async move {
        let result = run_install(&w, cmd).await;
        w.inner.lock().unwrap().install = match result {
            Ok(()) => Install::Done,
            Err(message) => Install::Failed { message },
        };
    });
    Json(snapshot(&wiz)).into_response()
}

fn set_install_step(wiz: &Wizard, step: &str) {
    wiz.inner.lock().unwrap().install = Install::Running { step: step.into() };
}

async fn run_install(wiz: &Wizard, cmd: Vec<String>) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("couldn't start the Plex installer: {e}"))?;
    let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err("couldn't start the Plex installer".into());
    };
    let errors = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(step) = line.strip_prefix("step: ") {
            set_install_step(wiz, step);
        }
    }
    let status = child
        .wait()
        .await
        .map_err(|e| format!("the Plex installer failed: {e}"))?;
    let errors = errors.await.unwrap_or_default();
    if !status.success() {
        // pkexec: 126 when the password dialog is closed, 127 when it isn't authorized.
        return Err(match status.code() {
            Some(126 | 127) if cmd[0].ends_with("pkexec") => {
                "Plex wasn't installed, because the password wasn't entered. Click Install Plex again to retry.".into()
            }
            _ => format!("Plex couldn't be installed: {}", tail(&errors)),
        });
    }
    set_install_step(wiz, "Waiting for Plex to start");
    let identity = Url::parse(&format!("http://127.0.0.1:{}/identity", wiz.opts.plex_port))
        .map_err(|e| e.to_string())?;
    for _ in 0..90 {
        if let Ok(r) = upstream::send(wiz.http.get(identity.clone())).await
            && r.status().is_success()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(
        "Plex was installed but hasn't started yet. Restart the computer, then click Look again."
            .into(),
    )
}

#[derive(Deserialize)]
struct NewLibraries {
    movies: Option<String>,
    tv: Option<String>,
    #[serde(default)]
    language: String,
}

/// Creates Movies and TV Shows libraries in a Plex that has none, in the folders given.
async fn create_libraries(State(wiz): St, Json(n): Json<NewLibraries>) -> Response {
    let plex = wiz.inner.lock().unwrap().plex.clone();
    let Some(mut plex) = plex else {
        return problem(StatusCode::BAD_REQUEST, "connect to Plex first");
    };
    if !plex.mounts.is_empty() {
        return problem(
            StatusCode::BAD_REQUEST,
            "Plex runs in Docker here: add the libraries in Plex itself, then reload this page",
        );
    }
    let ok_lang = n.language.len() <= 8
        && n.language
            .split('-')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric()));
    let language = if ok_lang && !n.language.is_empty() {
        n.language.as_str()
    } else {
        "en-US"
    };
    let mut warnings = vec![];
    let wanted = [
        (
            n.movies,
            "movie",
            "Movies",
            "tv.plex.agents.movie",
            "Plex Movie",
        ),
        (
            n.tv,
            "show",
            "TV Shows",
            "tv.plex.agents.series",
            "Plex TV Series",
        ),
    ];
    if wanted
        .iter()
        .all(|w| w.0.as_deref().is_none_or(|f| f.trim().is_empty()))
    {
        return problem(
            StatusCode::BAD_REQUEST,
            "choose a folder for movies or TV shows",
        );
    }
    // What Plex has now, so a retry never makes a second library of a kind.
    match sections(plex.base_url.clone(), plex.token.clone()).await {
        Ok(s) => plex.sections = s,
        Err(e) => return plex_down(e),
    }
    let mut todo = vec![];
    for (folder, kind, name, agent, scanner) in wanted {
        let Some(folder) = folder
            .map(|f| f.trim().trim_end_matches('/').to_string())
            .filter(|f| !f.is_empty())
        else {
            continue;
        };
        if plex.sections.iter().any(|s| s.kind == kind) {
            continue;
        }
        // Every folder is checked before Plex is asked for anything.
        if let Err(m) = check_folder(&folder, true) {
            return problem(StatusCode::BAD_REQUEST, &format!("{folder}: {m}"));
        }
        todo.push((folder, kind, name, agent, scanner));
    }
    for (folder, kind, name, agent, scanner) in todo {
        if let Err(e) = std::fs::create_dir_all(&folder) {
            return problem(
                StatusCode::BAD_REQUEST,
                &format!("{folder}: can't create it: {}", e.kind()),
            );
        }
        if !plex_can_read(Path::new(&folder)) {
            warnings.push(format!(
                "Plex may not be able to see {folder}: it runs as its own user, and folders inside your home folder are usually private. {MEDIA} works."
            ));
        }
        let Ok(mut url) = plex.base_url.join("library/sections") else {
            return problem(StatusCode::INTERNAL_SERVER_ERROR, "bad Plex address");
        };
        url.query_pairs_mut()
            .append_pair("name", name)
            .append_pair("type", kind)
            .append_pair("agent", agent)
            .append_pair("scanner", scanner)
            .append_pair("language", language)
            .append_pair("location", &folder);
        let sent = upstream::send(
            wiz.http
                .post(url)
                .header("Accept", "application/json")
                .header("X-Plex-Token", plex.token.expose()),
        )
        .await;
        match sent {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => {
                return problem(
                    StatusCode::BAD_GATEWAY,
                    &format!("Plex didn't create the {name} library ({})", r.status()),
                );
            }
            Err(e) => return plex_down(e),
        }
    }
    match sections(plex.base_url.clone(), plex.token.clone()).await {
        Ok(s) => plex.sections = s,
        Err(e) => return plex_down(e),
    }
    wiz.inner.lock().unwrap().plex = Some(plex);
    let mut v = snapshot(&wiz);
    v["warnings"] = json!(warnings);
    Json(v).into_response()
}

/// Whether a user other than the owner can open `dir` (Plex runs as its own user on Linux).
fn plex_can_read(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| {
            std::fs::metadata(p)
                .map(|m| m.permissions().mode())
                .unwrap_or(0)
        };
        if mode(dir) & 0o005 != 0o005 {
            return false;
        }
        dir.ancestors().skip(1).all(|a| mode(a) & 0o001 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        true
    }
}

/// The mounts of a running Plex container, as (container path, host path), longest
/// container path first so `/media/movies` wins over `/media`.
async fn docker_mounts() -> Vec<(String, String)> {
    async fn out(args: &[&str]) -> Option<String> {
        let o = tokio::time::timeout(
            Duration::from_secs(5),
            Command::new("docker")
                .args(args)
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    }
    let Some(ps) = out(&["ps", "--format", "{{.Names}}\t{{.Image}}"]).await else {
        return vec![];
    };
    let Some(name) = ps.lines().find_map(|l| {
        let low = l.to_ascii_lowercase();
        let skip = ["tautulli", "overseerr", "seerr"]
            .iter()
            .any(|s| low.contains(s));
        (low.contains("plex") && !skip).then(|| l.split('\t').next().unwrap_or("").to_string())
    }) else {
        return vec![];
    };
    if name.is_empty() || name.starts_with('-') {
        return vec![];
    }
    #[derive(Deserialize)]
    struct Mount {
        #[serde(rename = "Source")]
        source: String,
        #[serde(rename = "Destination")]
        destination: String,
    }
    let Some(json) = out(&["inspect", "-f", "{{json .Mounts}}", &name]).await else {
        return vec![];
    };
    let mut mounts: Vec<(String, String)> = serde_json::from_str::<Vec<Mount>>(json.trim())
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.destination.starts_with('/') && m.source.starts_with('/'))
        .map(|m| {
            (
                m.destination.trim_end_matches('/').to_string(),
                m.source.trim_end_matches('/').to_string(),
            )
        })
        .collect();
    mounts.sort_by_key(|(d, _)| std::cmp::Reverse(d.len()));
    mounts
}

// ------------------------------------------------------------------ folders

#[derive(Deserialize)]
struct FolderQuery {
    path: String,
    #[serde(default)]
    create: bool,
}

async fn folder(Json(q): Json<FolderQuery>) -> Json<Value> {
    Json(match check_folder(&q.path, q.create) {
        Ok(()) => json!({ "ok": true }),
        Err(msg) => json!({ "ok": false, "message": msg }),
    })
}

/// A folder Arr can use: absolute, a real directory, and writable by this user (the one
/// Arr will run as). With `create`, a missing folder is fine: setup creates it.
fn check_folder(raw: &str, create: bool) -> Result<(), String> {
    let p = Path::new(raw);
    if raw.is_empty() || !p.is_absolute() || raw.chars().any(|c| c.is_control()) {
        return Err("enter a full path, starting with /".into());
    }
    if !p.exists() {
        if create {
            let parent = p.ancestors().skip(1).find(|a| a.exists());
            return match parent {
                Some(a) if a.is_dir() => writable(a).map_err(|_| {
                    format!(
                        "this folder doesn't exist, and Arr can't create it inside {}",
                        a.display()
                    )
                }),
                _ => Err("this folder doesn't exist".into()),
            };
        }
        return Err("this folder doesn't exist on this computer".into());
    }
    if !p.is_dir() {
        return Err("that's a file, not a folder".into());
    }
    writable(p).map_err(|_| "Arr can't save files here; check the folder's permissions".into())
}

fn writable(dir: &Path) -> std::io::Result<()> {
    let probe = dir.join(format!(
        ".arr-write-test-{}",
        fsutil::random_hex(6).map_err(std::io::Error::other)?
    ));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    std::fs::remove_file(&probe)
}

// ------------------------------------------------------------------ apply

#[derive(Deserialize, Clone)]
struct Choices {
    /// `<section>:<location index>` from the state's library lists, or null.
    movies: Option<LibraryChoice>,
    tv: Option<LibraryChoice>,
    downloads: String,
    #[serde(default)]
    tmdb: String,
    downloading: bool,
    auto_update: bool,
}

#[derive(Deserialize, Clone)]
struct LibraryChoice {
    id: String,
    folder: String,
}

/// A library as the config records it.
struct Library {
    section: u32,
    plex_path: String,
    folder: String,
}

async fn apply(State(wiz): St, Json(c): Json<Choices>) -> Response {
    let plex = {
        let inner = wiz.inner.lock().unwrap();
        if matches!(inner.phase, Phase::Working { .. } | Phase::Done { .. }) {
            return problem(StatusCode::CONFLICT, "setup is already running");
        }
        inner.plex.clone()
    };
    let Some(plex) = plex else {
        return problem(StatusCode::BAD_REQUEST, "connect to Plex first");
    };
    let pick = |choice: &Option<LibraryChoice>, kind: &str| -> Result<Option<Library>, String> {
        let Some(ch) = choice else { return Ok(None) };
        let (key, idx) = ch
            .id
            .split_once(':')
            .and_then(|(k, i)| Some((k.parse::<u32>().ok()?, i.parse::<usize>().ok()?)))
            .ok_or("pick a library from the list")?;
        let loc = plex
            .sections
            .iter()
            .find(|s| s.key == key && s.kind == kind)
            .and_then(|s| s.locations.get(idx))
            .ok_or("pick a library from the list")?;
        check_folder(&ch.folder, false).map_err(|m| format!("{}: {m}", ch.folder))?;
        Ok(Some(Library {
            section: key,
            plex_path: loc.trim_end_matches('/').to_string(),
            folder: ch.folder.trim_end_matches('/').to_string(),
        }))
    };
    let (movies, tv) = match (pick(&c.movies, "movie"), pick(&c.tv, "show")) {
        (Ok(m), Ok(t)) => (m, t),
        (Err(e), _) | (_, Err(e)) => return problem(StatusCode::BAD_REQUEST, &e),
    };
    if movies.is_none() && tv.is_none() {
        return problem(StatusCode::BAD_REQUEST, "choose a movie or TV library");
    }
    if let Err(m) = check_folder(&c.downloads, true) {
        return problem(StatusCode::BAD_REQUEST, &format!("download folder: {m}"));
    }
    let tmdb = c.tmdb.trim().to_string();
    if tmdb.len() > 1024 || tmdb.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return problem(StatusCode::BAD_REQUEST, "that TMDB key doesn't look right");
    }
    set_phase(&wiz, "Saving your settings", "");
    let w = wiz.clone();
    tokio::spawn(async move {
        let result = finish_setup(&w, &plex, movies, tv, &c, &tmdb).await;
        let mut inner = w.inner.lock().unwrap();
        inner.phase = match result {
            Ok(()) => Phase::Done {
                downloading: c.downloading,
            },
            Err(e) => Phase::Failed {
                message: format!("{e:#}"),
            },
        };
        if matches!(inner.phase, Phase::Done { .. }) {
            let w = w.clone();
            tokio::spawn(async move {
                tokio::time::sleep(LINGER).await;
                w.finished.notify_one();
            });
        }
    });
    Json(snapshot(&wiz)).into_response()
}

fn set_phase(wiz: &Wizard, step: &str, detail: &str) {
    wiz.inner.lock().unwrap().phase = Phase::Working {
        step: step.into(),
        detail: detail.into(),
    };
}

async fn finish_setup(
    wiz: &Wizard,
    plex: &Connected,
    movies: Option<Library>,
    tv: Option<Library>,
    c: &Choices,
    tmdb: &str,
) -> Result<()> {
    let dir = &wiz.opts.state_dir;
    let secrets = dir.join("secrets");
    let api_token = secrets.join("api_token");
    if !api_token.exists() {
        fsutil::write_new_private_file(&api_token, fsutil::random_hex(32)?.as_bytes())?;
    }
    let mcp_token = secrets.join("mcp_token");
    if !mcp_token.exists() {
        fsutil::write_new_private_file(&mcp_token, fsutil::random_hex(32)?.as_bytes())?;
    }
    fsutil::atomic_replace_private(&secrets.join("plex_token"), plex.token.expose().as_bytes())?;
    if !tmdb.is_empty() {
        fsutil::atomic_replace_private(&secrets.join("tmdb_credential"), tmdb.as_bytes())?;
    }
    std::fs::create_dir_all(&c.downloads)
        .with_context(|| format!("can't create {}", c.downloads))?;
    let config = dir.join("config.toml");
    let text = render_config(
        &wiz.opts,
        plex,
        movies.as_ref(),
        tv.as_ref(),
        c,
        !tmdb.is_empty(),
    );
    fsutil::atomic_replace_private(&config, text.as_bytes())?;

    set_phase(wiz, "Checking everything works", "");
    let checked = async {
        arr(wiz, &["setup", "--config"], &config).await?;
        arr(wiz, &["check", "--config"], &config).await
    }
    .await;
    if let Err(e) = checked {
        // The installer starts over when there's no config; keep this one for reference.
        let _ = std::fs::rename(&config, dir.join("config.toml.failed"));
        return Err(e);
    }

    set_phase(
        wiz,
        "Learning what you already own",
        "Reading your Plex libraries",
    );
    let media = match (&movies, &tv) {
        (Some(_), Some(_)) => "all",
        (Some(_), None) => "movies",
        _ => "tv",
    };
    bootstrap(wiz, &config, media).await?;
    fsutil::atomic_replace_private(
        &dir.join("setup-choices"),
        format!("auto_update={}\n", u8::from(c.auto_update)).as_bytes(),
    )?;
    Ok(())
}

/// Runs `arr <args> <config>` and fails with the end of its error output.
async fn arr(wiz: &Wizard, args: &[&str], config: &Path) -> Result<()> {
    let out = Command::new(&wiz.opts.arr)
        .args(args)
        .arg(config)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("can't run {}", wiz.opts.arr.display()))?;
    if out.status.success() {
        return Ok(());
    }
    bail!("arr {} failed: {}", args[0], tail(&out.stderr))
}

fn tail(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    let t = lines[lines.len().saturating_sub(4)..].join("\n");
    t.chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .take(1500)
        .collect()
}

/// The Plex import, with its progress lines (`Movie: running offset 50/79 ...`) shown on
/// the page.
async fn bootstrap(wiz: &Wizard, config: &Path, media: &str) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut child = Command::new(&wiz.opts.arr)
        .args(["bootstrap", "--media", media, "--config"])
        .arg(config)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("can't run {}", wiz.opts.arr.display()))?;
    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;
    let collect = tokio::spawn(async move {
        let mut buf = vec![];
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut BufReader::new(stdout), &mut buf).await;
        buf
    });
    let mut lines = BufReader::new(stderr).lines();
    let mut last = vec![];
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(p) = progress(&line) {
            set_phase(wiz, "Learning what you already own", &p);
        }
        last.push(line);
        if last.len() > 20 {
            last.remove(0);
        }
    }
    let status = child.wait().await?;
    let stdout = collect.await.unwrap_or_default();
    if !status.success() {
        bail!(
            "the Plex import failed: {}",
            tail(last.join("\n").as_bytes())
        );
    }
    let out: Value = serde_json::from_slice(&stdout).unwrap_or(Value::Null);
    if let Some(run) = out["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["status"] != "done")
    {
        bail!(
            "the Plex import didn't finish: {}",
            run["last_error"]
                .as_str()
                .unwrap_or("run the installer again to continue it")
        );
    }
    Ok(())
}

/// `Movie: running offset 50/79 titles +3 files +3` -> `Movies: 50 of 79`.
fn progress(line: &str) -> Option<String> {
    let (kind, rest) = line.split_once(": ")?;
    let what = match kind {
        "Movie" => "Movies",
        "Series" => "TV shows",
        _ => return None,
    };
    let counts = rest
        .split_whitespace()
        .skip_while(|w| *w != "offset")
        .nth(1)?;
    let (done, total) = counts.split_once('/')?;
    if total == "?" {
        return Some(format!("{what}: {done} so far"));
    }
    Some(format!("{what}: {done} of {total}"))
}

async fn finish(State(wiz): St) -> Response {
    if !matches!(wiz.inner.lock().unwrap().phase, Phase::Done { .. }) {
        return problem(StatusCode::CONFLICT, "setup isn't finished");
    }
    wiz.finished.notify_one();
    Json(json!({ "ok": true })).into_response()
}

/// The config `install.sh` writes, for the choices made on the page.
fn render_config(
    opts: &Options,
    plex: &Connected,
    movies: Option<&Library>,
    tv: Option<&Library>,
    c: &Choices,
    tmdb: bool,
) -> String {
    let s = |v: &str| toml::Value::String(v.to_string()).to_string();
    let mut out = format!(
        r#"# Arr configuration, written by the setup page. Secrets live in secrets/ (mode 600).
# All options: https://github.com/newtonharry/Arr/blob/main/config/standalone.example.toml

[server]
listen = {listen}
api_token_file = "secrets/api_token"

[state]
dir = "."

[[indexer]]
id = 1
name = "The Pirate Bay"
kind = "thepiratebay"
base_url = "https://apibay.org/"
recent_feed = true

[tvmaze]
enabled = true

[tmdb]
enabled = {tmdb}
"#,
        listen = s(&format!("127.0.0.1:{}", opts.port)),
    );
    if tmdb {
        out.push_str("credential_file = \"secrets/tmdb_credential\"\n");
    }
    out.push_str(&format!(
        r#"
[execution]
enabled = {downloading}
download_cleanup = true

# Sonarr/Radarr can keep running: Arr only acquires what you add to Arr.
[migration]
mode = "coexistence"

[embedded]
download_dir = {downloads}
listen = {torrent}
trackers = true
dht = true
max_active_downloads = 3
"#,
        downloading = c.downloading,
        downloads = s(c.downloads.trim_end_matches('/')),
        torrent = s(&format!("0.0.0.0:{}", opts.torrent_port)),
    ));
    if opts.upload_limit_kib > 0 {
        out.push_str(&format!(
            "upload_limit_bytes_per_sec = {}\n",
            opts.upload_limit_kib.saturating_mul(1024)
        ));
    }
    out.push_str(&format!(
        "\n[library]\nffprobe = {}\n",
        s(&opts.ffprobe.to_string_lossy())
    ));
    if let Some(m) = movies {
        out.push_str(&format!("movies = {}\n", s(&m.folder)));
    }
    if let Some(t) = tv {
        out.push_str(&format!("tv = {}\n", s(&t.folder)));
    }
    out.push_str(&format!(
        "\n[plex]\nbase_url = {}\ntoken_file = \"secrets/plex_token\"\n",
        s(plex.base_url.as_str())
    ));
    if let Some(m) = movies {
        out.push_str(&format!(
            "movie_section = {}\nmovies_path = {}\n",
            m.section,
            s(&m.plex_path)
        ));
    }
    if let Some(t) = tv {
        out.push_str(&format!(
            "tv_section = {}\ntv_path = {}\n",
            t.section,
            s(&t.plex_path)
        ));
    }
    out.push_str(
        r#"
# MCP for AI assistants on this machine (Claude Desktop, Claude Code, ...); see docs/MCP.md.
[mcp]
enabled = true
token_file = "secrets/mcp_token"
"#,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_lines() {
        assert_eq!(
            progress("Movie: running offset 50/79 titles +3 files +3").as_deref(),
            Some("Movies: 50 of 79")
        );
        assert_eq!(
            progress("Series: done offset 33/33 titles +0 files +0").as_deref(),
            Some("TV shows: 33 of 33")
        );
        assert_eq!(
            progress("Movie: running offset 0/? titles +0 files +0").as_deref(),
            Some("Movies: 0 so far")
        );
        assert_eq!(progress("2026-11-08 INFO something"), None);
    }

    #[test]
    fn docker_paths_map_to_the_most_specific_mount() {
        let p = Connected {
            name: "Plex".into(),
            base_url: Url::parse("http://127.0.0.1:32400/").unwrap(),
            token: Secret::new("t".into()),
            sections: vec![],
            mounts: vec![
                ("/data/movies".into(), "/srv/films".into()),
                ("/data".into(), "/srv".into()),
            ],
        };
        // Neither host folder exists here, so only the mapping can produce a path.
        assert_eq!(
            local_path(&p, "/data/movies/HD").as_deref(),
            Some("/srv/films/HD")
        );
        assert_eq!(local_path(&p, "/data/tv").as_deref(), Some("/srv/tv"));
        assert_eq!(local_path(&p, "/datasets").as_deref(), None);
    }

    #[test]
    fn folders_must_be_absolute_and_writable() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        assert!(check_folder(d, false).is_ok());
        assert!(check_folder("relative/path", false).is_err());
        assert!(check_folder(&format!("{d}/missing"), false).is_err());
        assert!(check_folder(&format!("{d}/missing/deeper"), true).is_ok());
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        assert!(check_folder(file.to_str().unwrap(), false).is_err());
    }
}
