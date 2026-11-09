//! The web app: a phone-first page served by Arr itself, with its own sign-in.
//!
//! People sign in with Google (when `[mcp.oauth]` is configured; see `oauth.rs`) or with a
//! one-time link made by `arr app-link`. A session is an opaque cookie whose SHA-256 is kept
//! in `web.sqlite` (private, in the state directory). The page talks to `/app/api/...` only:
//! `/app/api/v1/*` is an allow-listed, in-process proxy to the API router that adds the API
//! bearer token, so the token never reaches a browser. Posters come from `/app/art/{media_id}`
//! (see `art.rs`), so the browser never contacts TMDB or TVmaze.

use crate::api::AppState;
use crate::art::Art;
use crate::config::{OAuthConfig, RuntimeConfig};
use crate::fsutil;
use crate::ids::MediaId;
use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Form, Path as UrlPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

pub const SESSION_COOKIE: &str = "arr_session";
const SESSION_TTL: i64 = 30 * 86_400;
/// How long a link from `arr app-link` works.
pub const LINK_TTL: i64 = 15 * 60;
const MAX_PROXY_BODY: usize = 64 * 1024;
const MAX_PROXY_REPLY: usize = 16 * 1024 * 1024;

const CSP: &str = "default-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn random_b64(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).expect("system randomness unavailable");
    B64.encode(b)
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Web sessions and one-time sign-in links (`<state_dir>/web.sqlite`).
pub struct Sessions {
    db: Mutex<rusqlite::Connection>,
}

impl Sessions {
    pub fn open(state_dir: &Path) -> Result<Arc<Self>> {
        let path = state_dir.join("web.sqlite");
        if path.exists() {
            fsutil::check_private_file(&path)?;
        } else {
            fsutil::write_new_private_file(&path, b"")?;
        }
        let db = rusqlite::Connection::open(&path).context("open web.sqlite")?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                hash TEXT PRIMARY KEY, kind TEXT NOT NULL, email TEXT,
                created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS links (
                hash TEXT PRIMARY KEY, expires_at INTEGER NOT NULL);",
        )?;
        Ok(Arc::new(Self { db: Mutex::new(db) }))
    }

    /// A new one-time link token, valid for `LINK_TTL`.
    pub fn create_link(&self) -> Result<String> {
        let token = random_b64(32);
        let t = now();
        let db = self.db.lock().unwrap();
        db.execute("DELETE FROM links WHERE expires_at <= ?1", [t])?;
        db.execute(
            "INSERT INTO links (hash, expires_at) VALUES (?1, ?2)",
            rusqlite::params![sha256_hex(&token), t + LINK_TTL],
        )?;
        Ok(token)
    }

    /// Uses up a link token: true exactly once, while it is live.
    fn consume_link(&self, token: &str) -> bool {
        let db = self.db.lock().unwrap();
        db.execute(
            "DELETE FROM links WHERE hash = ?1 AND expires_at > ?2",
            rusqlite::params![sha256_hex(token), now()],
        )
        .is_ok_and(|n| n == 1)
    }

    /// Starts a session (`kind` is `google` or `link`); returns the cookie value.
    pub fn create_session(&self, kind: &str, email: Option<&str>) -> Option<String> {
        let id = random_b64(32);
        let t = now();
        let db = self.db.lock().unwrap();
        db.execute("DELETE FROM sessions WHERE expires_at <= ?1", [t])
            .ok()?;
        db.execute(
            "INSERT INTO sessions (hash, kind, email, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![sha256_hex(&id), kind, email, t, t + SESSION_TTL],
        )
        .ok()?;
        Some(id)
    }

    /// `(kind, email)` of a live session.
    fn lookup(&self, id: &str) -> Option<(String, Option<String>)> {
        let db = self.db.lock().unwrap();
        db.query_row(
            "SELECT kind, email FROM sessions WHERE hash = ?1 AND expires_at > ?2",
            rusqlite::params![sha256_hex(id), now()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
    }

    fn delete(&self, id: &str) {
        let db = self.db.lock().unwrap();
        let _ = db.execute("DELETE FROM sessions WHERE hash = ?1", [sha256_hex(id)]);
    }
}

/// The `Set-Cookie` value of a session.
pub fn session_cookie(id: &str, secure: bool) -> String {
    format!(
        "{SESSION_COOKIE}={id}; Path=/; Max-Age={SESSION_TTL}; HttpOnly; SameSite=Lax{}",
        if secure { "; Secure" } else { "" }
    )
}

/// A new one-time sign-in link for `arr app-link`.
pub fn app_link(cfg: &RuntimeConfig) -> Result<String> {
    let token = Sessions::open(&cfg.state_dir)?.create_link()?;
    let base = match cfg.mcp.as_ref().and_then(|m| m.oauth.as_ref()) {
        Some(o) => o.public_url.clone(),
        None => {
            let ip = cfg.listen.ip();
            let host = match ip {
                ip if ip.is_loopback() || ip.is_unspecified() => "localhost".to_string(),
                std::net::IpAddr::V6(v6) => format!("[{v6}]"),
                ip => ip.to_string(),
            };
            format!("http://{host}:{}", cfg.listen.port())
        }
    };
    Ok(format!("{base}/app/link?t={token}"))
}

struct Web {
    state: Arc<AppState>,
    art: Art,
    sessions: Arc<Sessions>,
    api: Router,
    token: String,
    oauth: Option<OAuthConfig>,
}

type Wa = State<Arc<Web>>;

/// The web app's routes. Has no fallback of its own (the API router's serves unknown paths).
pub fn router(state: &Arc<AppState>, api: Router) -> Result<Router> {
    let web = Arc::new(Web {
        state: state.clone(),
        art: Art::new(&state.cfg.state_dir)?,
        sessions: Sessions::open(&state.cfg.state_dir)?,
        api,
        token: state.cfg.api_token.expose().to_string(),
        oauth: state.cfg.mcp.as_ref().and_then(|m| m.oauth.clone()),
    });
    Ok(Router::new()
        .route(
            "/",
            get(|| async { asset("text/html; charset=utf-8", INDEX) }),
        )
        .route(
            "/app/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }),
        )
        .route(
            "/app/app.css",
            get(|| async { asset("text/css; charset=utf-8", APP_CSS) }),
        )
        .route(
            "/app/mascot.svg",
            get(|| async { asset("image/svg+xml", MASCOT) }),
        )
        .route(
            "/app/tmdb.svg",
            get(|| async { asset("image/svg+xml", TMDB_LOGO) }),
        )
        .route(
            "/app/fonts/nunito-latin.woff2",
            get(|| async { font(FONT_LATIN) }),
        )
        .route(
            "/app/fonts/nunito-latin-ext.woff2",
            get(|| async { font(FONT_LATIN_EXT) }),
        )
        .route("/app/link", get(link_page).post(link_consume))
        .route("/app/art/{media_id}", get(art))
        .route("/app/api/me", get(me))
        .route("/app/api/signout", post(signout))
        .route("/app/api/v1/{*rest}", any(proxy))
        .with_state(web))
}

const INDEX: &str = include_str!("webapp/index.html");
const APP_JS: &str = include_str!("webapp/app.js");
const APP_CSS: &str = include_str!("webapp/app.css");
const MASCOT: &str = include_str!("webapp/mascot.svg");
const TMDB_LOGO: &str = include_str!("webapp/tmdb.svg");
const FONT_LATIN: &[u8] = include_bytes!("webapp/fonts/nunito-latin.woff2");
const FONT_LATIN_EXT: &[u8] = include_bytes!("webapp/fonts/nunito-latin-ext.woff2");

fn secure_headers(r: &mut Response) {
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // Not no-referrer: browsers then send `Origin: null` on same-page form posts, which the
    // same-origin check refuses. Other sites still get no referrer.
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    let mut r = ([(header::CONTENT_TYPE, content_type)], body).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    secure_headers(&mut r);
    r
}

fn font(body: &'static [u8]) -> Response {
    let mut r = ([(header::CONTENT_TYPE, "font/woff2")], body).into_response();
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=604800, immutable"),
    );
    secure_headers(&mut r);
    r
}

/// A small HTML page (the link pages), styled by the app's stylesheet.
fn page(status: StatusCode, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Arr</title><link rel=\"stylesheet\" href=\"/app/app.css\"></head><body><main class=\"link-page\"><img src=\"/app/mascot.svg\" alt=\"\" width=\"64\" height=\"64\">{body}</main></body></html>"
    );
    let mut r = (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    secure_headers(&mut r);
    r
}

fn json_error(status: StatusCode, code: &str, message: &str) -> Response {
    let mut r = (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn host(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::HOST).and_then(|v| v.to_str().ok())
}

/// `scheme://authority` of the public URL, and its authority.
fn public_parts(cfg: &OAuthConfig) -> (&str, &str) {
    let url = cfg.public_url.as_str();
    match url.split_once("://") {
        Some((scheme, rest)) => (scheme, rest.split('/').next().unwrap_or(rest)),
        None => ("http", url),
    }
}

impl Web {
    /// Whether the request's Host is the public URL's host.
    fn on_public_host(&self, headers: &HeaderMap) -> Option<&str> {
        let cfg = self.oauth.as_ref()?;
        let (scheme, authority) = public_parts(cfg);
        host(headers)
            .is_some_and(|h| h.eq_ignore_ascii_case(authority))
            .then_some(scheme)
    }

    fn secure(&self, headers: &HeaderMap) -> bool {
        self.on_public_host(headers) == Some("https")
    }

    /// A present `Origin` must be this request's own origin.
    fn same_origin(&self, headers: &HeaderMap) -> bool {
        let Some(origin) = headers.get(header::ORIGIN) else {
            return true;
        };
        let (Some(origin), Some(h)) = (origin.to_str().ok(), host(headers)) else {
            return false;
        };
        let scheme = self.on_public_host(headers).unwrap_or("http");
        origin.eq_ignore_ascii_case(&format!("{scheme}://{h}"))
    }

    /// The `/app/api` checks every request passes before anything else.
    fn check_app(&self, headers: &HeaderMap) -> Option<Response> {
        if headers.get("x-arr-app").and_then(|v| v.to_str().ok()) != Some("1") {
            return Some(json_error(
                StatusCode::FORBIDDEN,
                "forbidden",
                "missing X-Arr-App header",
            ));
        }
        if !self.same_origin(headers) {
            return Some(json_error(
                StatusCode::FORBIDDEN,
                "forbidden",
                "origin not allowed",
            ));
        }
        None
    }

    /// The live session of a request: `(cookie value, email)`.
    fn session(&self, headers: &HeaderMap) -> Option<(String, Option<String>)> {
        let id = headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .find_map(|c| {
                c.trim()
                    .strip_prefix(SESSION_COOKIE)?
                    .strip_prefix('=')
                    .map(str::to_string)
            })?;
        let (kind, email) = self.sessions.lookup(&id)?;
        if kind == "google" {
            // Only while Google sign-in exists and the address is still allowed.
            let allowed = self
                .oauth
                .as_ref()
                .zip(email.as_deref())
                .is_some_and(|(o, e)| o.allowed_emails.contains(&e.to_ascii_lowercase()));
            if !allowed {
                return None;
            }
        }
        Some((id, email))
    }

    fn unauthorized(&self) -> Response {
        let mut r = (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": { "code": "unauthorized", "message": "sign in to use Arr" },
                "signin": { "google": self.oauth.is_some() },
            })),
        )
            .into_response();
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        r
    }
}

/// A poster. Same-origin `<img>` requests carry the cookie but not the app header, so this
/// checks the session only. No poster is a plain 404: the app keeps its title tile.
async fn art(State(w): Wa, headers: HeaderMap, UrlPath(id): UrlPath<String>) -> Response {
    if w.session(&headers).is_none() {
        return w.unauthorized();
    }
    let Ok(id) = id.parse::<MediaId>() else {
        return json_error(StatusCode::BAD_REQUEST, "invalid_request", "not a media id");
    };
    let mut r = match w.art.poster(&w.state, &id).await {
        Some((bytes, kind)) => {
            let mut r = ([(header::CONTENT_TYPE, kind)], bytes).into_response();
            r.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=604800"),
            );
            r
        }
        None => {
            let mut r = StatusCode::NOT_FOUND.into_response();
            r.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=3600"),
            );
            r
        }
    };
    r.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    r
}

async fn me(State(w): Wa, headers: HeaderMap) -> Response {
    if let Some(r) = w.check_app(&headers) {
        return r;
    }
    let Some((_, email)) = w.session(&headers) else {
        return w.unauthorized();
    };
    let mut r = Json(json!({ "email": email })).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

async fn signout(State(w): Wa, headers: HeaderMap) -> Response {
    if let Some(r) = w.check_app(&headers) {
        return r;
    }
    let Some((id, _)) = w.session(&headers) else {
        return w.unauthorized();
    };
    w.sessions.delete(&id);
    let mut r = StatusCode::NO_CONTENT.into_response();
    let secure = if w.secure(&headers) { "; Secure" } else { "" };
    if let Ok(v) = HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{secure}"
    )) {
        r.headers_mut().insert(header::SET_COOKIE, v);
    }
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

#[derive(Deserialize)]
struct LinkQuery {
    t: Option<String>,
}

fn token_shaped(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 128
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}

/// The sign-in page of a link. It never uses the token: link previews fetch GETs.
async fn link_page(axum::extract::Query(q): axum::extract::Query<LinkQuery>) -> Response {
    match q.t.filter(|t| token_shaped(t)) {
        Some(t) => page(
            StatusCode::OK,
            &format!(
                "<h1>Sign in to Arr</h1><form method=\"post\" action=\"/app/link\"><input type=\"hidden\" name=\"t\" value=\"{t}\"><button type=\"submit\">Sign in</button></form>"
            ),
        ),
        None => page(
            StatusCode::BAD_REQUEST,
            "<h1>This link isn't valid</h1><p>Ask the person who runs Arr for a new sign-in link.</p>",
        ),
    }
}

#[derive(Deserialize)]
struct LinkForm {
    t: Option<String>,
}

async fn link_consume(State(w): Wa, headers: HeaderMap, Form(f): Form<LinkForm>) -> Response {
    if !w.same_origin(&headers) {
        return page(
            StatusCode::FORBIDDEN,
            "<h1>Not allowed</h1><p>Open the sign-in link again.</p>",
        );
    }
    let session =
        f.t.filter(|t| token_shaped(t) && w.sessions.consume_link(t))
            .and_then(|_| w.sessions.create_session("link", None));
    let Some(id) = session else {
        return page(
            StatusCode::GONE,
            "<h1>This link has expired</h1><p>Sign-in links work once and only for 15 minutes. Ask the person who runs Arr for a new one.</p>",
        );
    };
    let mut r = StatusCode::SEE_OTHER.into_response();
    r.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/"));
    if let Ok(v) = HeaderValue::from_str(&session_cookie(&id, w.secure(&headers))) {
        r.headers_mut().insert(header::SET_COOKIE, v);
    }
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// Whether the app may call this API route.
fn allowed(method: &Method, path: &str) -> bool {
    let rest = path.strip_prefix("/api/v1/").unwrap_or("");
    let parts: Vec<&str> = rest.split('/').collect();
    match *method {
        Method::GET => matches!(
            parts.as_slice(),
            ["feed"]
                | ["ideas"]
                | ["discover"]
                | ["library", "movies" | "series"]
                | ["show", _]
                | ["show", _, "season", _]
                | ["movie", _]
        ),
        Method::POST => match parts.as_slice() {
            ["quick-add" | "follow" | "stop"] => true,
            ["queue", id, "retry"] => !id.is_empty(),
            _ => false,
        },
        _ => false,
    }
}

async fn proxy(State(w): Wa, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    if let Some(r) = w.check_app(&parts.headers) {
        return r;
    }
    if w.session(&parts.headers).is_none() {
        return w.unauthorized();
    }
    let path = parts
        .uri
        .path()
        .strip_prefix("/app")
        .unwrap_or(parts.uri.path())
        .to_string();
    if !allowed(&parts.method, &path) {
        return json_error(StatusCode::NOT_FOUND, "not_found", "no such route");
    }
    let Ok(bytes) = axum::body::to_bytes(body, MAX_PROXY_BODY).await else {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            "request body too large",
        );
    };
    let uri = match parts.uri.query() {
        Some(q) => format!("{path}?{q}"),
        None => path,
    };
    let mut auth = match HeaderValue::from_str(&format!("Bearer {}", w.token)) {
        Ok(v) => v,
        Err(_) => return json_error(StatusCode::BAD_GATEWAY, "bad_gateway", "API unavailable"),
    };
    auth.set_sensitive(true);
    let mut rb = Request::builder()
        .method(parts.method.clone())
        .uri(uri)
        .header(header::AUTHORIZATION, auth);
    if let Some(ct) = parts.headers.get(header::CONTENT_TYPE) {
        rb = rb.header(header::CONTENT_TYPE, ct);
    }
    let Ok(inner) = rb.body(Body::from(bytes)) else {
        return json_error(StatusCode::BAD_REQUEST, "invalid_request", "bad request");
    };
    let Ok(resp) = w.api.clone().oneshot(inner).await;
    let (rp, rbody) = resp.into_parts();
    let Ok(raw) = axum::body::to_bytes(rbody, MAX_PROXY_REPLY).await else {
        return json_error(StatusCode::BAD_GATEWAY, "bad_gateway", "reply too large");
    };
    let mut out = Response::new(Body::from(raw));
    *out.status_mut() = rp.status;
    for name in [header::CONTENT_TYPE, header::RETRY_AFTER] {
        if let Some(v) = rp.headers.get(&name) {
            out.headers_mut().insert(name, v.clone());
        }
    }
    out.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    out
}
