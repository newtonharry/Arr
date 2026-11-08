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

use crate::config::Secret;
use crate::fsutil;
use crate::plex::Section;
use crate::plexsignin::{self, PLEX_TIMEOUT, PinState, PlexTv, Server, sections};
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
/// Between looks at plex.tv after linking a new Plex.
const CLAIM_WAIT: Duration = Duration::from_secs(1);

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
    /// The installer was told yes (`--error-reports`): the page starts with reports on.
    pub error_reports: bool,
    /// Where TMDB's API is, for checking keys (overridden in tests).
    pub tmdb_url: Url,
}

struct Wizard {
    opts: Options,
    code: String,
    session: String,
    tv: PlexTv,
    http: reqwest::Client,
    inner: Mutex<Inner>,
    /// Held while looking for (and claiming) this computer's Plex, so two looks at once
    /// can't claim it twice.
    lookup: tokio::sync::Mutex<()>,
    finished: Notify,
    /// Error reports, from the moment the user ticks the box (docs/PRIVACY.md).
    reporting: Mutex<Option<crate::report::Reporting>>,
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
        /// A movie library was set up without a TMDB key, so Arr can't find new movies.
        no_tmdb: bool,
    },
    Failed {
        message: String,
    },
}

type St = State<Arc<Wizard>>;

/// Runs the wizard until setup finishes (Ok) or is abandoned (Err).
pub async fn run(mut opts: Options) -> Result<()> {
    let config = opts.state_dir.join("config.toml");
    if config.exists() {
        // Not "remove it": Arr's catalog still points at the old folders.
        bail!(
            "{} already exists; Arr is set up (to start over, run the uninstaller and choose to delete Arr's data first)",
            config.display()
        );
    }
    fsutil::ensure_private_dir(&opts.state_dir)?;
    let client_id = plexsignin::client_id(&opts.state_dir)?;
    if !opts.bind.is_loopback() && !private_lan(opts.bind) {
        // The page would be open to the internet, protected only by its link.
        println!(
            "\n{} isn't a private network address, so the setup page stays on this computer. To open it from yours, first run: ssh -L {port}:localhost:{port} <user>@<this server>",
            opts.bind,
            port = opts.port
        );
        opts.bind = IpAddr::from([127, 0, 0, 1]);
    }
    let http = upstream::http_client(PLEX_TIMEOUT, reqwest::redirect::Policy::none());
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
        tv: PlexTv {
            base: opts.plex_tv.clone(),
            auth: opts.plex_auth.clone(),
            client_id,
            http: http.clone(),
        },
        http,
        inner: Mutex::new(Inner::default()),
        lookup: tokio::sync::Mutex::new(()),
        finished: Notify::new(),
        reporting: Mutex::new(
            opts.error_reports
                .then(|| crate::report::start(true, "setup")),
        ),
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
                _ = async {
                    tokio::time::sleep(LIFETIME).await;
                    // Never in the middle of setting up: that would leave it half done.
                    while working(&done) {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                } => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        })
        .await?;
    match wiz.inner.lock().unwrap().phase.clone() {
        Phase::Done { .. } => Ok(()),
        _ => bail!("setup wasn't finished"),
    }
}

fn working(wiz: &Wizard) -> bool {
    matches!(wiz.inner.lock().unwrap().phase, Phase::Working { .. })
}

/// A private network address (home or office network, link-local, carrier-grade NAT): one
/// only nearby computers can reach, so the setup page may listen on it.
fn private_lan(ip: IpAddr) -> bool {
    let v4 = |v4: std::net::Ipv4Addr| {
        let o = v4.octets();
        v4.is_private() || v4.is_link_local() || (o[0] == 100 && o[1] & 0xc0 == 64)
    };
    match ip {
        IpAddr::V4(a) => v4(a),
        IpAddr::V6(a) => {
            let s = a.segments()[0];
            s & 0xfe00 == 0xfc00 || s & 0xffc0 == 0xfe80 || a.to_ipv4_mapped().is_some_and(v4)
        }
    }
}

fn router(wiz: Arc<Wizard>) -> Router {
    let api = Router::new()
        .route("/api/state", get(state))
        .route("/api/plex/signin", post(signin))
        .route("/api/plex/check", post(check_signin))
        .route("/api/plex/install", post(install_plex))
        .route("/api/plex/libraries", post(create_libraries))
        .route("/api/reporting", post(set_reporting))
        .route("/api/tmdb/check", post(check_tmdb))
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
        "media_movies": crate::plexinstall::default_media().join("Movies"),
        "media_tv": crate::plexinstall::default_media().join("TV Shows"),
        "phase": inner.phase,
        "reporting": {
            "available": crate::report::available(),
            "enabled": wiz.reporting.lock().unwrap().as_ref().is_some_and(|r| r.is_on()),
        },
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

/// Starts a (new) sign-in. Whatever the last one found is forgotten, so signing in with a
/// different account starts clean.
async fn signin(State(wiz): St) -> Response {
    let pin = match wiz.tv.new_pin().await {
        Ok(p) => p,
        Err(e) => return plex_down(e),
    };
    // After any look in progress, so it can't connect the old account's Plex afterwards.
    let _lookup = wiz.lookup.lock().await;
    {
        let mut inner = wiz.inner.lock().unwrap();
        inner.pin = Some((pin.id, pin.code));
        inner.account = None;
        inner.plex = None;
        inner.missing = None;
        inner.unclaimed_port = None;
    }
    Json(json!({ "url": pin.url })).into_response()
}

fn plex_down(e: upstream::UpstreamError) -> Response {
    problem(
        StatusCode::BAD_GATEWAY,
        &format!("couldn't reach plex.tv: {}", e.describe()),
    )
}

/// Polls the sign-in once; once signed in, looks for this computer's Plex, or reads its
/// libraries again (ones added in Plex meanwhile show up).
async fn check_signin(State(wiz): St) -> Response {
    // Held from reading the PIN to the last state change, so a new sign-in (which waits for
    // it) always comes after, and a late reply for the old PIN can't sign the old account in.
    let _lookup = wiz.lookup.lock().await;
    let (pin, signed_in) = {
        let inner = wiz.inner.lock().unwrap();
        (inner.pin.clone(), inner.account.is_some())
    };
    if !signed_in {
        let Some((id, code)) = pin else {
            return Json(snapshot(&wiz)).into_response();
        };
        let reply = wiz.tv.check_pin(id, &code).await;
        let mut inner = wiz.inner.lock().unwrap();
        if inner.pin.as_ref() != Some(&(id, code)) {
            drop(inner);
            return Json(snapshot(&wiz)).into_response();
        }
        match reply {
            Ok(PinState::SignedIn(t)) => inner.account = Some(t),
            Ok(PinState::Waiting) => {
                drop(inner);
                return Json(snapshot(&wiz)).into_response();
            }
            Ok(PinState::Expired) => {
                inner.pin = None;
                return problem(
                    StatusCode::GONE,
                    "the Plex sign-in expired; click Sign in again",
                );
            }
            Err(e) => return plex_down(e),
        }
    }
    let plex = wiz.inner.lock().unwrap().plex.clone();
    if let Some(plex) = plex {
        return match sections(plex.base_url.clone(), plex.token.clone()).await {
            Ok(s) => {
                if let Some(p) = wiz.inner.lock().unwrap().plex.as_mut() {
                    p.sections = s;
                }
                Json(snapshot(&wiz)).into_response()
            }
            Err(e) => problem(
                StatusCode::BAD_GATEWAY,
                &format!("couldn't read Plex's libraries: {}", e.describe()),
            ),
        };
    }
    // Until Plex is found, every check looks again, so a failed lookup or a Plex
    // installed meanwhile is picked up.
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
        .tv
        .url("api/claim/token.json")
        .map_err(|_| "bad plex.tv address".to_string())?;
    let token: ClaimToken = match upstream::send(
        wiz.tv
            .headers(wiz.http.get(url))
            .header("X-Plex-Token", account.expose()),
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
        plexsignin::urlencode(&token.token)
    );
    let r = upstream::send(wiz.tv.headers(wiz.http.post(url)))
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

/// The signed-in account's Plex servers (none before signing in).
async fn servers(wiz: &Wizard) -> Result<Vec<Server>, upstream::UpstreamError> {
    let account = wiz.inner.lock().unwrap().account.clone();
    match account {
        Some(a) => wiz.tv.servers(&a).await,
        None => Ok(vec![]),
    }
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
        let Ok(id) = plexsignin::identity(&wiz.http, &base).await else {
            continue;
        };
        answered = true;
        if id.claimed == Some(false) {
            wiz.inner.lock().unwrap().unclaimed_port = Some(port);
            return Err(Missing::Unclaimed);
        }
        let Some(server) = servers.iter().find(|s| s.id == id.machine_identifier) else {
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

// ------------------------------------------------------------------ error reports

#[derive(Deserialize)]
struct ReportingChoice {
    enabled: bool,
}

#[derive(Deserialize)]
struct TmdbKey {
    key: String,
}

/// Checks a TMDB key as it's pasted, so the page can say straight away whether it works.
async fn check_tmdb(State(wiz): St, Json(k): Json<TmdbKey>) -> Response {
    match crate::tmdbkey::verify(&wiz.opts.tmdb_url, &k.key).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => problem(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    }
}

/// The "Send error reports" box: reporting starts the moment it's ticked, so a failure
/// later in setup is reported too, and stops when it's unticked.
async fn set_reporting(State(wiz): St, Json(r): Json<ReportingChoice>) -> Response {
    let next = r.enabled.then(|| crate::report::start(true, "setup"));
    // The old client is dropped outside the lock, and off the async threads: dropping
    // waits up to two seconds for queued reports.
    let old = std::mem::replace(&mut *wiz.reporting.lock().unwrap(), next);
    tokio::task::spawn_blocking(move || drop(old));
    Json(snapshot(&wiz)).into_response()
}

// ------------------------------------------------------------------ installing Plex

/// How this page installs Plex as administrator: the system's password dialog running
/// `arr plex-install`. None where that isn't possible (no apt/dnf, no dialog, no desktop).
fn install_command(wiz: &Wizard) -> Option<Vec<String>> {
    if let Some(cmd) = &wiz.opts.plex_installer {
        return Some(vec!["sh".into(), "-c".into(), cmd.clone()]);
    }
    let pm = crate::plexinstall::package_manager()?;
    if pm == "windows" || pm == "macos" {
        // As you: Plex's Windows installer asks Windows for permission itself, and on macOS
        // the app goes into Applications.
        return Some(vec![
            wiz.opts.arr.to_string_lossy().into_owned(),
            "plex-install".into(),
        ]);
    }
    let desktop =
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    if !desktop || !Path::new("/usr/bin/pkexec").is_file() {
        return None;
    }
    let mut cmd = vec![
        "/usr/bin/pkexec".to_string(),
        wiz.opts.arr.to_string_lossy().into_owned(),
    ];
    cmd.extend([
        "plex-install".into(),
        "--media".into(),
        crate::plexinstall::default_media()
            .to_string_lossy()
            .into_owned(),
    ]);
    if let Some(owner) = owner() {
        cmd.extend(["--owner".into(), owner]);
    }
    Some(cmd)
}

/// The same, for pasting into a terminal where the page can't ask for the password itself.
fn manual_install_command(wiz: &Wizard) -> Option<String> {
    if matches!(crate::plexinstall::package_manager()?, "windows" | "macos") {
        return None;
    }
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
            Err(e) => {
                let message = format!("{e:#}");
                // Closing the password dialog is a choice, not a failure.
                if message != NO_PASSWORD {
                    crate::report::error("plex-install", &crate::report::reportable(&e));
                }
                Install::Failed { message }
            }
        };
    });
    Json(snapshot(&wiz)).into_response()
}

fn set_install_step(wiz: &Wizard, step: &str) {
    wiz.inner.lock().unwrap().install = Install::Running { step: step.into() };
}

const NO_PASSWORD: &str = "Plex wasn't installed, because the password wasn't entered. Click Install Plex for me to try again.";

async fn run_install(wiz: &Wizard, cmd: Vec<String>) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("couldn't start the Plex installer")?;
    let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        bail!("couldn't start the Plex installer");
    };
    let errors = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });
    // Steps until the installer exits. Not until its output closes: a program it starts
    // (Plex, on Windows) may keep that open.
    let mut lines = BufReader::new(stdout).lines();
    let failed = |e: std::io::Error| anyhow::Error::new(e).context("the Plex installer failed");
    let status = loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    if let Some(step) = line.strip_prefix("step: ") {
                        set_install_step(wiz, step);
                    }
                }
                _ => break child.wait().await.map_err(failed)?,
            },
            status = child.wait() => break status.map_err(failed)?,
        }
    };
    let errors = tokio::time::timeout(Duration::from_secs(2), errors)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    if !status.success() {
        // pkexec: 126 when the password dialog is closed, 127 when it isn't authorized.
        return Err(match status.code() {
            Some(126 | 127) if cmd[0].ends_with("pkexec") => anyhow::anyhow!(NO_PASSWORD),
            // Its output is shown on the page but not reported: it can name anything.
            _ => anyhow::Error::new(crate::report::Private(tail(&errors)))
                .context("Plex couldn't be installed"),
        });
    }
    set_install_step(wiz, "Waiting for Plex to start");
    let identity = Url::parse(&format!("http://127.0.0.1:{}/identity", wiz.opts.plex_port))?;
    for _ in 0..90 {
        if let Ok(r) = upstream::send(wiz.http.get(identity.clone())).await
            && r.status().is_success()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    bail!("Plex was installed but hasn't started yet. Wait a minute, then click Look again.")
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
            "Plex runs in Docker here: add the libraries in Plex itself, then click Look again",
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
            .map(|f| trim_sep(f.trim()).to_string())
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
                "Plex may not be able to see {folder}: it runs as its own user, and folders inside your home folder are usually private. {} works.",
                crate::plexinstall::default_media().display()
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
    // Only Linux runs Plex as its own user; on Windows and macOS it runs as you.
    #[cfg(all(unix, not(target_os = "macos")))]
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
    #[cfg(any(not(unix), target_os = "macos"))]
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
        return Err(if cfg!(windows) {
            "enter a full path, like D:\\Media\\Movies".into()
        } else {
            "enter a full path, starting with /".into()
        });
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

/// A folder without the separator at its end (`/srv/media/` -> `/srv/media`), but never
/// a bare root (`/`, `D:\`).
fn trim_sep(p: &str) -> &str {
    let seps: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };
    let t = p.trim_end_matches(seps);
    if t.is_empty() || t.ends_with(':') {
        p
    } else {
        t
    }
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
    auto_update: bool,
    /// Send error reports (written as `[reporting] enabled`).
    #[serde(default)]
    reporting: bool,
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
    // Checked and claimed in one go, so two tabs can't both start setup.
    let plex = {
        let mut inner = wiz.inner.lock().unwrap();
        if matches!(inner.phase, Phase::Working { .. } | Phase::Done { .. }) {
            return problem(StatusCode::CONFLICT, "setup is already running");
        }
        let Some(plex) = inner.plex.clone() else {
            return problem(StatusCode::BAD_REQUEST, "connect to Plex first");
        };
        inner.phase = Phase::Working {
            step: "Checking your choices".into(),
            detail: String::new(),
        };
        plex
    };
    // Until setup is handed to its own task, any way out of here (a refusal, or the page
    // going away mid-check, which drops this handler) goes back to editing.
    let mut editing = BackToEditing(Some(&wiz));
    let refuse = |msg: &str| problem(StatusCode::BAD_REQUEST, msg);
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
            plex_path: trim_sep(loc).to_string(),
            folder: trim_sep(&ch.folder).to_string(),
        }))
    };
    let (movies, tv) = match (pick(&c.movies, "movie"), pick(&c.tv, "show")) {
        (Ok(m), Ok(t)) => (m, t),
        (Err(e), _) | (_, Err(e)) => return refuse(&e),
    };
    if movies.is_none() && tv.is_none() {
        return refuse("choose a movie or TV library");
    }
    if let Err(m) = check_folder(&c.downloads, true) {
        return refuse(&format!("download folder: {m}"));
    }
    // The key only finds movies, so without a movie library it isn't asked for.
    let tmdb = match movies {
        Some(_) => c.tmdb.trim().to_string(),
        None => String::new(),
    };
    if !tmdb.is_empty() {
        // Checked again here, not only as it's typed: a key saved has worked once.
        set_phase(&wiz, "Checking your TMDB key", "");
        if let Err(e) = crate::tmdbkey::verify(&wiz.opts.tmdb_url, &tmdb).await {
            return refuse(&format!("{e:#}"));
        }
    }
    set_phase(&wiz, "Saving your settings", "");
    editing.0 = None;
    let no_tmdb = movies.is_some() && tmdb.is_empty();
    let w = wiz.clone();
    tokio::spawn(async move {
        let result = finish_setup(&w, &plex, movies, tv, &c, &tmdb).await;
        let mut inner = w.inner.lock().unwrap();
        inner.phase = match result {
            Ok(()) => Phase::Done { no_tmdb },
            Err(e) => {
                crate::report::error("setup", &crate::report::reportable(&e));
                Phase::Failed {
                    message: format!("{e:#}"),
                }
            }
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

/// Puts the phase back to editing when dropped, unless emptied first.
struct BackToEditing<'a>(Option<&'a Wizard>);

impl Drop for BackToEditing<'_> {
    fn drop(&mut self) {
        if let Some(w) = self.0 {
            w.inner.lock().unwrap().phase = Phase::Editing;
        }
    }
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
    // Under another name until everything has worked: the installers take config.toml to
    // mean "Arr is set up".
    let config = dir.join("config.toml.new");
    let text = render_config(
        &wiz.opts,
        plex,
        movies.as_ref(),
        tv.as_ref(),
        c,
        !tmdb.is_empty(),
    );
    fsutil::atomic_replace_private(&config, text.as_bytes())?;

    let result = async {
        set_phase(wiz, "Checking everything works", "");
        arr(wiz, &["setup", "--config"], &config).await?;
        arr(wiz, &["check", "--config"], &config).await?;
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
        )
    }
    .await;
    if let Err(e) = result {
        // Kept for reference; Finish writes a new one.
        let _ = std::fs::rename(&config, dir.join("config.toml.failed"));
        return Err(e);
    }
    std::fs::rename(&config, dir.join("config.toml")).context("can't save config.toml")?;
    fsutil::fsync_dir(dir)
}

/// Runs `arr <args> <config>` and fails with the end of its error output.
async fn arr(wiz: &Wizard, args: &[&str], config: &Path) -> Result<()> {
    let out = Command::new(&wiz.opts.arr)
        .args(args)
        .arg(config)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("can't run {}", wiz.opts.arr.display()))?;
    if out.status.success() {
        return Ok(());
    }
    // Its output is shown on the page but not reported: it can name anything.
    Err(
        anyhow::Error::new(crate::report::Private(tail(&out.stderr)))
            .context(format!("arr {} failed", args[0])),
    )
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
        .kill_on_drop(true)
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
    // The import's own words (log lines, titles) are shown on the page but not reported.
    let private = |m: String| anyhow::Error::new(crate::report::Private(m));
    let errors = || tail(last.join("\n").as_bytes());
    // An unfinished run is in the report on stdout, whether or not the exit says so.
    let out: Value = serde_json::from_slice(&stdout).unwrap_or(Value::Null);
    if let Some(run) = out["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["status"] != "done")
    {
        let why = match run["last_error"].as_str() {
            Some(e) => e.to_string(),
            None if status.success() => "it stopped before the end".into(),
            None => errors(),
        };
        return Err(private(why).context("the Plex import didn't finish"));
    }
    if !status.success() {
        return Err(private(errors()).context("the Plex import failed"));
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
# More options: https://github.com/newtonharry/Arr/blob/main/config/standalone.example.toml

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
enabled = true
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
        downloads = s(trim_sep(&c.downloads)),
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
    if c.reporting {
        out.push_str(
            "\n# Error reports with names, paths, addresses and tokens removed; see docs/PRIVACY.md.\n[reporting]\nenabled = true\n",
        );
    }
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
    fn the_page_listens_only_on_private_addresses() {
        for ip in [
            "192.168.1.20",
            "10.0.0.5",
            "172.16.4.1",
            "169.254.10.1",
            "100.64.0.1",
            "100.127.255.254",
            "fd12:3456::1",
            "fe80::1",
            "::ffff:192.168.1.20",
        ] {
            assert!(private_lan(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "0.0.0.0",
            "8.8.8.8",
            "100.128.0.1",
            "172.32.0.1",
            "::",
            "2001:db8::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!private_lan(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn trailing_separators_go_but_roots_stay() {
        assert_eq!(trim_sep("/srv/media/"), "/srv/media");
        assert_eq!(trim_sep("/"), "/");
        if cfg!(windows) {
            assert_eq!(trim_sep("D:\\Media\\"), "D:\\Media");
            assert_eq!(trim_sep("D:\\"), "D:\\");
        }
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
