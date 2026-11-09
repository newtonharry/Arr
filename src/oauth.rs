//! OAuth 2.1 sign-in for the remote MCP endpoint, with Google as the identity provider.
//!
//! Arr is its own authorization server for `/mcp`: a connector (ChatGPT) discovers it through
//! the well-known documents, identifies itself with a Client ID Metadata Document (its
//! `client_id` is an https URL under a trusted prefix), sends the user through `/authorize` and
//! a consent page to Google, and receives an authorization code that `/token` exchanges (PKCE,
//! `private_key_jwt` client authentication) for an opaque access and refresh token. Only the
//! SHA-256 of a token is stored (`oauth.sqlite` in the state directory), so tokens survive a
//! restart and are never readable from disk. Codes, consent transactions and assertion ids live
//! in memory with short lifetimes and bounds.
//!
//! Only the configured Google accounts (`allowed_emails`) can sign in, and the list is checked
//! again whenever a token is used or refreshed. Dynamic client registration is not supported.
//! Logs carry outcome codes only, never tokens, codes, assertions, secrets or addresses.

use crate::config::OAuthConfig;
use crate::fsutil;
use anyhow::{Context, Result};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use reqwest::Url;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;

const MAX_FORM_BYTES: usize = 16 * 1024;
const MAX_CLIENT_DOC_BYTES: usize = 5 * 1024;
const MAX_JWKS_BYTES: usize = 64 * 1024;
const MAX_GOOGLE_BYTES: usize = 64 * 1024;
const MAX_PENDING: usize = 1000;
const MAX_CACHE: usize = 64;
const MAX_JTIS: usize = 10_000;
const TXN_TTL: Duration = Duration::from_secs(600);
const CODE_TTL: Duration = Duration::from_secs(300);
const DOC_TTL: Duration = Duration::from_secs(3600);
const JWKS_TTL: Duration = Duration::from_secs(600);
const ACCESS_PREFIX: &str = "arr_at_";
const REFRESH_PREFIX: &str = "arr_rt_";
const ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";

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

fn escape(s: &str) -> String {
    s.chars().fold(String::new(), |mut o, c| {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
        o
    })
}

/// Bounded map of values that expire; used for transactions, codes and caches.
struct Pending<V> {
    ttl: Duration,
    items: HashMap<String, (Instant, V)>,
}

impl<V> Pending<V> {
    fn new(ttl: Duration) -> Mutex<Self> {
        Mutex::new(Self {
            ttl,
            items: HashMap::new(),
        })
    }
    fn purge(&mut self) {
        let ttl = self.ttl;
        self.items.retain(|_, (t, _)| t.elapsed() < ttl);
    }
    /// Stores `v`; false when the map is full of live entries.
    fn insert(&mut self, k: String, v: V) -> bool {
        self.purge();
        if self.items.len() >= MAX_PENDING {
            return false;
        }
        self.items.insert(k, (Instant::now(), v));
        true
    }
    /// Cache insert: evicts the oldest entry when full.
    fn cache(&mut self, k: String, v: V) {
        self.purge();
        if self.items.len() >= MAX_CACHE
            && let Some(old) = self
                .items
                .iter()
                .min_by_key(|(_, (t, _))| *t)
                .map(|(k, _)| k.clone())
        {
            self.items.remove(&old);
        }
        self.items.insert(k, (Instant::now(), v));
    }
    fn get(&self, k: &str) -> Option<&V> {
        self.items
            .get(k)
            .filter(|(t, _)| t.elapsed() < self.ttl)
            .map(|(_, v)| v)
    }
    /// Single use: removes and returns the live entry.
    fn take(&mut self, k: &str) -> Option<V> {
        let (t, v) = self.items.remove(k)?;
        (t.elapsed() < self.ttl).then_some(v)
    }
}

/// What a connector's metadata document says about it.
struct ClientDoc {
    name: String,
    redirect_uris: Vec<String>,
    auth_method: String,
    jwks_uri: Option<String>,
}

struct Jwk {
    kid: Option<String>,
    n: Vec<u8>,
    e: Vec<u8>,
}

/// A validated `/authorize` request waiting for the user's decision.
struct Txn {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    state: String,
}

/// An approved request waiting for Google's callback.
struct Upstream {
    /// The connector's request; none when the web app started this sign-in.
    txn: Option<Txn>,
    verifier: String,
    /// SHA-256 of the `arr_signin` cookie set on the browser that approved; the callback
    /// must come from that browser, so a forwarded Google link cannot complete it.
    browser: String,
}

const SIGNIN_COOKIE: &str = "arr_signin";

/// An authorization code.
struct Code {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    sub: String,
    email: String,
}

pub struct OAuth {
    cfg: OAuthConfig,
    hosts: Vec<String>,
    issuer: String,
    http: reqwest::Client,
    db: Mutex<rusqlite::Connection>,
    slots: tokio::sync::Semaphore,
    txns: Mutex<Pending<Txn>>,
    upstream: Mutex<Pending<Upstream>>,
    codes: Mutex<Pending<Code>>,
    docs: Mutex<Pending<Arc<ClientDoc>>>,
    jwks: Mutex<Pending<Arc<Vec<Jwk>>>>,
    /// Assertion ids seen, with their expiry (unix seconds).
    jtis: Mutex<HashMap<String, i64>>,
    /// Web app sessions, issued after a web sign-in.
    web: Arc<crate::webapp::Sessions>,
}

impl OAuth {
    pub fn new(
        cfg: &OAuthConfig,
        allowed_hosts: &[String],
        state_dir: &std::path::Path,
    ) -> Result<Arc<Self>> {
        let path = state_dir.join("oauth.sqlite");
        if path.exists() {
            fsutil::check_private_file(&path)?;
        } else {
            fsutil::write_new_private_file(&path, b"")?;
        }
        let db = rusqlite::Connection::open(&path).context("open oauth.sqlite")?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS tokens (
                hash TEXT PRIMARY KEY, kind TEXT NOT NULL, sub TEXT NOT NULL, email TEXT NOT NULL,
                client_id TEXT NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);",
        )?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()?;
        Ok(Arc::new(Self {
            cfg: cfg.clone(),
            hosts: allowed_hosts.to_vec(),
            issuer: format!("{}/", cfg.public_url),
            http,
            db: Mutex::new(db),
            slots: tokio::sync::Semaphore::new(16),
            txns: Pending::new(TXN_TTL),
            upstream: Pending::new(TXN_TTL),
            codes: Pending::new(CODE_TTL),
            docs: Pending::new(DOC_TTL),
            jwks: Pending::new(JWKS_TTL),
            jtis: Mutex::new(HashMap::new()),
            web: crate::webapp::Sessions::open(state_dir)?,
        }))
    }

    /// The `WWW-Authenticate` challenge of a refused `/mcp` request.
    pub fn challenge(&self) -> HeaderValue {
        HeaderValue::from_str(&format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\"",
            self.cfg.public_url
        ))
        .unwrap_or_else(|_| HeaderValue::from_static("Bearer"))
    }

    fn email_allowed(&self, email: &str) -> bool {
        let e = email.to_ascii_lowercase();
        self.cfg.allowed_emails.contains(&e)
    }

    /// Whether `token` is a live access token of an address that is still allowed.
    pub fn verify_access(&self, token: &str) -> bool {
        if !token.starts_with(ACCESS_PREFIX) {
            return false;
        }
        let db = self.db.lock().unwrap();
        db.query_row(
            "SELECT email FROM tokens WHERE hash = ?1 AND kind = 'access' AND expires_at > ?2",
            rusqlite::params![sha256_hex(token), now()],
            |r| r.get::<_, String>(0),
        )
        .is_ok_and(|email| self.email_allowed(&email))
    }

    fn resource(&self) -> String {
        format!("{}/mcp", self.cfg.public_url)
    }

    // ---------------------------------------------------------------- upstream fetches

    /// Sends `rb` and returns the JSON object of a 2xx reply of at most `max` bytes.
    async fn fetch_json(&self, rb: reqwest::RequestBuilder, max: usize) -> Option<Value> {
        let mut r = rb.send().await.ok()?;
        if !r.status().is_success() || r.content_length().is_some_and(|l| l > max as u64) {
            return None;
        }
        let mut buf = Vec::new();
        while let Some(c) = r.chunk().await.ok()? {
            if buf.len() + c.len() > max {
                return None;
            }
            buf.extend_from_slice(&c);
        }
        serde_json::from_slice::<Value>(&buf)
            .ok()
            .filter(Value::is_object)
    }

    /// Whether `id` is `<trusted prefix>[<id>/]client.json`.
    fn trusted(&self, id: &str) -> bool {
        self.cfg.trusted_client_metadata.iter().any(|p| {
            id.strip_prefix(p.as_str()).is_some_and(|rest| {
                rest == "client.json"
                    || rest.strip_suffix("/client.json").is_some_and(|n| {
                        (1..=128).contains(&n.len())
                            && n.bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                    })
            })
        })
    }

    /// The (cached) metadata document of a trusted client.
    async fn client_doc(&self, id: &str) -> Option<Arc<ClientDoc>> {
        if !self.trusted(id) {
            return None;
        }
        if let Some(d) = self.docs.lock().unwrap().get(id) {
            return Some(d.clone());
        }
        let v = self
            .fetch_json(
                self.http.get(id).header(header::ACCEPT, "application/json"),
                MAX_CLIENT_DOC_BYTES,
            )
            .await?;
        if v.get("client_id")?.as_str()? != id {
            return None;
        }
        let redirect_uris: Vec<String> = v
            .get("redirect_uris")?
            .as_array()?
            .iter()
            .map(|u| u.as_str().map(str::to_string))
            .collect::<Option<_>>()?;
        if redirect_uris.is_empty() || redirect_uris.len() > 16 {
            return None;
        }
        let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let doc = Arc::new(ClientDoc {
            name: text("client_name")
                .unwrap_or_else(|| id.to_string())
                .chars()
                .filter(|c| !c.is_control())
                .take(100)
                .collect(),
            redirect_uris,
            auth_method: text("token_endpoint_auth_method")?,
            jwks_uri: text("jwks_uri"),
        });
        self.docs.lock().unwrap().cache(id.to_string(), doc.clone());
        Some(doc)
    }

    /// The client's RSA signing keys and whether they were just fetched.
    async fn client_keys(&self, uri: &str, fresh: bool) -> Option<(Arc<Vec<Jwk>>, bool)> {
        if !fresh && let Some(k) = self.jwks.lock().unwrap().get(uri) {
            return Some((k.clone(), false));
        }
        let v = self
            .fetch_json(
                self.http
                    .get(uri)
                    .header(header::ACCEPT, "application/json"),
                MAX_JWKS_BYTES,
            )
            .await?;
        let dec = |k: &Value, f: &str| {
            let b = B64.decode(k.get(f)?.as_str()?).ok()?;
            let start = b.iter().position(|&x| x != 0)?;
            Some(b[start..].to_vec())
        };
        let keys: Vec<Jwk> = v
            .get("keys")?
            .as_array()?
            .iter()
            .filter(|k| k.get("kty").and_then(Value::as_str) == Some("RSA"))
            .filter(|k| {
                k.get("use")
                    .and_then(Value::as_str)
                    .is_none_or(|u| u == "sig")
            })
            .filter_map(|k| {
                Some(Jwk {
                    kid: k.get("kid").and_then(Value::as_str).map(str::to_string),
                    n: dec(k, "n")?,
                    e: dec(k, "e")?,
                })
            })
            .take(16)
            .collect();
        let keys = Arc::new(keys);
        self.jwks
            .lock()
            .unwrap()
            .cache(uri.to_string(), keys.clone());
        Some((keys, true))
    }

    // ---------------------------------------------------------------- tokens

    fn issue_tokens(&self, client_id: &str, sub: &str, email: &str) -> Option<Value> {
        let (at, rt) = (
            format!("{ACCESS_PREFIX}{}", random_b64(32)),
            format!("{REFRESH_PREFIX}{}", random_b64(32)),
        );
        let t = now();
        let db = self.db.lock().unwrap();
        db.execute("DELETE FROM tokens WHERE expires_at <= ?1", [t])
            .ok()?;
        for (tok, kind, ttl) in [
            (&at, "access", self.cfg.access_token_ttl),
            (&rt, "refresh", self.cfg.refresh_token_ttl),
        ] {
            db.execute(
                "INSERT INTO tokens (hash, kind, sub, email, client_id, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    sha256_hex(tok),
                    kind,
                    sub,
                    email,
                    client_id,
                    t,
                    t + ttl.as_secs() as i64
                ],
            )
            .ok()?;
        }
        Some(json!({
            "access_token": at,
            "token_type": "Bearer",
            "expires_in": self.cfg.access_token_ttl.as_secs(),
            "refresh_token": rt,
        }))
    }
}

// -------------------------------------------------------------------- responses

fn secure_headers(r: &mut Response) {
    let h = r.headers_mut();
    for (k, v) in [
        (
            "content-security-policy",
            "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
        ),
        ("x-frame-options", "DENY"),
        // Not no-referrer: under it browsers send `Origin: null` on the consent form POST,
        // which the same-origin check refuses. same-origin still sends no referrer elsewhere.
        ("referrer-policy", "same-origin"),
    ] {
        h.insert(k, HeaderValue::from_static(v));
    }
}

fn html(status: StatusCode, body: String) -> Response {
    let page = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Arr</title><style>body{{font-family:sans-serif;max-width:32em;margin:3em auto;padding:0 1em}}button{{font-size:1em;padding:.5em 1.2em;margin-right:.5em}}</style></head><body>{body}</body></html>"
    );
    let mut r = (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        page,
    )
        .into_response();
    secure_headers(&mut r);
    r
}

fn error_page(status: StatusCode, msg: &str) -> Response {
    html(status, format!("<h1>Arr</h1><p>{}</p>", escape(msg)))
}

/// 302 to `base` with `params` added to its query.
fn redirect(base: &str, params: &[(&str, &str)]) -> Response {
    let Ok(mut u) = Url::parse(base) else {
        return error_page(StatusCode::BAD_REQUEST, "Invalid redirect.");
    };
    u.query_pairs_mut().extend_pairs(params);
    let mut r = StatusCode::FOUND.into_response();
    if let Ok(v) = HeaderValue::from_str(u.as_str()) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r
}

/// An error redirect back to the (already validated) client.
fn client_error(o: &OAuth, redirect_uri: &str, code: &str, state: Option<&str>) -> Response {
    let mut p = vec![("error", code), ("iss", o.issuer.as_str())];
    if let Some(s) = state {
        p.push(("state", s));
    }
    redirect(redirect_uri, &p)
}

struct TokenError(StatusCode, &'static str, &'static str);

impl IntoResponse for TokenError {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(json!({ "error": self.1, "error_description": self.2 })),
        )
            .into_response()
    }
}

fn invalid_client(d: &'static str) -> TokenError {
    TokenError(StatusCode::UNAUTHORIZED, "invalid_client", d)
}
fn invalid_request(d: &'static str) -> TokenError {
    TokenError(StatusCode::BAD_REQUEST, "invalid_request", d)
}
fn invalid_grant(d: &'static str) -> TokenError {
    TokenError(StatusCode::BAD_REQUEST, "invalid_grant", d)
}

// -------------------------------------------------------------------- router

type Oa = State<Arc<OAuth>>;

pub fn router(o: Arc<OAuth>) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(server_metadata),
        )
        .route("/authorize", get(authorize))
        .route("/authorize/consent", post(consent))
        .route("/auth/callback", get(callback))
        .route("/app/signin", get(web_signin))
        .route("/token", post(token))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_FORM_BYTES))
        .layer(axum::middleware::from_fn_with_state(o.clone(), guard))
        .with_state(o)
}

/// Host allow-list, same-origin POSTs, bounded concurrency and `Cache-Control: no-store`.
async fn guard(State(o): Oa, req: Request, next: axum::middleware::Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut resp = 'checked: {
        if !host
            .as_deref()
            .is_some_and(|h| crate::mcp::host_allowed(h, &o.hosts))
        {
            break 'checked error_page(StatusCode::FORBIDDEN, "Host not allowed.");
        }
        if req.method() != axum::http::Method::GET
            && let Some(origin) = req.headers().get(header::ORIGIN)
            && origin.to_str().ok() != Some(o.cfg.public_url.as_str())
        {
            break 'checked error_page(StatusCode::FORBIDDEN, "Origin not allowed.");
        }
        let Ok(_permit) = o.slots.try_acquire() else {
            break 'checked error_page(StatusCode::SERVICE_UNAVAILABLE, "Too many requests.");
        };
        next.run(req).await
    };
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

async fn resource_metadata(State(o): Oa) -> Response {
    Json(json!({
        "resource": o.resource(),
        "authorization_servers": [o.issuer],
        "bearer_methods_supported": ["header"],
    }))
    .into_response()
}

async fn server_metadata(State(o): Oa) -> Response {
    let p = &o.cfg.public_url;
    Json(json!({
        "issuer": o.issuer,
        "authorization_endpoint": format!("{p}/authorize"),
        "token_endpoint": format!("{p}/token"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["private_key_jwt", "none"],
        "token_endpoint_auth_signing_alg_values_supported": ["RS256"],
        "authorization_response_iss_parameter_supported": true,
        "client_id_metadata_document_supported": true,
    }))
    .into_response()
}

// -------------------------------------------------------------------- /authorize

async fn authorize(State(o): Oa, Query(q): Query<HashMap<String, String>>) -> Response {
    let get = |k: &str| q.get(k).map(String::as_str);
    let unknown = || error_page(StatusCode::BAD_REQUEST, "Unknown or untrusted client.");
    let Some(client_id) = get("client_id") else {
        return unknown();
    };
    let Some(doc) = o.client_doc(client_id).await else {
        tracing::info!(outcome = "authorize_unknown_client", "oauth");
        return unknown();
    };
    let Some(redirect_uri) =
        get("redirect_uri").filter(|u| doc.redirect_uris.iter().any(|d| d == u))
    else {
        tracing::info!(outcome = "authorize_bad_redirect_uri", "oauth");
        return error_page(StatusCode::BAD_REQUEST, "Redirect address not registered.");
    };
    // The client and redirect address are valid from here: errors go back to the client.
    let state = get("state").filter(|s| !s.is_empty() && s.len() <= 512);
    let challenge = get("code_challenge").filter(|c| {
        c.len() == 43
            && c.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    });
    let valid = get("response_type") == Some("code")
        && get("code_challenge_method") == Some("S256")
        && get("resource").is_none_or(|r| r == o.resource());
    let (Some(state), Some(challenge), true) = (state, challenge, valid) else {
        tracing::info!(outcome = "authorize_invalid_request", "oauth");
        return client_error(&o, redirect_uri, "invalid_request", state);
    };
    let id = random_b64(32);
    let txn = Txn {
        client_id: client_id.into(),
        redirect_uri: redirect_uri.into(),
        challenge: challenge.into(),
        state: state.into(),
    };
    if !o.txns.lock().unwrap().insert(id.clone(), txn) {
        return error_page(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many sign-ins in progress.",
        );
    }
    let host = Url::parse(redirect_uri)
        .ok()
        .map(|u| match (u.host_str(), u.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            _ => String::new(),
        })
        .unwrap_or_default();
    tracing::info!(outcome = "consent_shown", "oauth");
    html(
        StatusCode::OK,
        format!(
            "<h1>Arr</h1><p><b>{}</b> wants to use your Arr media assistant tools. You will be sent to <b>{}</b> afterwards.</p><p>Continue by signing in with Google?</p>\
             <form method=\"post\" action=\"/authorize/consent\"><input type=\"hidden\" name=\"txn\" value=\"{}\">\
             <button type=\"submit\" name=\"decision\" value=\"approve\">Approve</button>\
             <button type=\"submit\" name=\"decision\" value=\"deny\">Deny</button></form>",
            escape(&doc.name),
            escape(&host),
            escape(&id)
        ),
    )
}

#[derive(Deserialize)]
struct ConsentForm {
    txn: Option<String>,
    decision: Option<String>,
}

async fn consent(State(o): Oa, Form(f): Form<ConsentForm>) -> Response {
    let Some(txn) = f.txn.and_then(|t| o.txns.lock().unwrap().take(&t)) else {
        return error_page(
            StatusCode::BAD_REQUEST,
            "This request expired. Start again.",
        );
    };
    match f.decision.as_deref() {
        Some("approve") => {}
        Some("deny") => {
            tracing::info!(outcome = "consent_denied", "oauth");
            return client_error(&o, &txn.redirect_uri, "access_denied", Some(&txn.state));
        }
        _ => return error_page(StatusCode::BAD_REQUEST, "Invalid choice."),
    }
    start_google(&o, Some(txn))
}

/// `GET /app/signin`: the web app's Google sign-in (the same flow as an approved consent).
async fn web_signin(State(o): Oa) -> Response {
    start_google(&o, None)
}

/// Sends the browser to Google; the callback finishes the connector request `txn`, or signs
/// the web app in when there is none.
fn start_google(o: &OAuth, txn: Option<Txn>) -> Response {
    let is_web = txn.is_none();
    let (state, verifier, browser) = (random_b64(32), random_b64(32), random_b64(32));
    let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = o.cfg.google_authorization_endpoint.clone();
    url.query_pairs_mut().extend_pairs([
        ("client_id", o.cfg.google_client_id.as_str()),
        (
            "redirect_uri",
            &format!("{}/auth/callback", o.cfg.public_url),
        ),
        ("response_type", "code"),
        ("scope", "openid email"),
        ("state", &state),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("prompt", "select_account"),
    ]);
    if !o.upstream.lock().unwrap().insert(
        state,
        Upstream {
            txn,
            verifier,
            browser: sha256_hex(&browser),
        },
    ) {
        return error_page(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many sign-ins in progress.",
        );
    }
    tracing::info!(
        outcome = if is_web {
            "web_signin_started"
        } else {
            "consent_approved"
        },
        "oauth"
    );
    let mut r = redirect(url.as_str(), &[]);
    if let Ok(v) = HeaderValue::from_str(&format!(
        "{SIGNIN_COOKIE}={browser}; Path=/auth/callback; Max-Age=600; HttpOnly; Secure; SameSite=Lax"
    )) {
        r.headers_mut().insert(header::SET_COOKIE, v);
    }
    r
}

/// The `arr_signin` cookie value sent with a request, if any.
fn signin_cookie(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(SIGNIN_COOKIE)?.strip_prefix('='))
}

// -------------------------------------------------------------------- /auth/callback

async fn callback(
    State(o): Oa,
    headers: axum::http::HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(up) = q
        .get("state")
        .and_then(|s| o.upstream.lock().unwrap().take(s))
    else {
        return error_page(
            StatusCode::BAD_REQUEST,
            "This sign-in expired. Start again.",
        );
    };
    let same_browser = signin_cookie(&headers)
        .is_some_and(|c| bool::from(sha256_hex(c).as_bytes().ct_eq(up.browser.as_bytes())));
    if !same_browser {
        tracing::info!(outcome = "callback_other_browser", "oauth");
        return error_page(
            StatusCode::BAD_REQUEST,
            "This sign-in was started in another browser. Start again from your assistant.",
        );
    }
    let Some(txn) = &up.txn else {
        return web_callback(&o, &q, &up.verifier).await;
    };
    let deny = |outcome: &str, code: &str| {
        tracing::info!(outcome, "oauth");
        client_error(&o, &txn.redirect_uri, code, Some(&txn.state))
    };
    let (Some(code), None) = (q.get("code"), q.get("error")) else {
        return deny("callback_upstream_error", "access_denied");
    };
    let Some(reply) = google_exchange(&o, code, &up.verifier).await else {
        return deny("callback_exchange_failed", "server_error");
    };
    let Some((sub, email)) = reply
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(|t| check_id_token(&o, t, now()))
    else {
        return deny("callback_identity_refused", "access_denied");
    };
    let code = random_b64(32);
    let entry = Code {
        client_id: txn.client_id.clone(),
        redirect_uri: txn.redirect_uri.clone(),
        challenge: txn.challenge.clone(),
        sub,
        email,
    };
    if !o.codes.lock().unwrap().insert(code.clone(), entry) {
        return deny("callback_codes_full", "server_error");
    }
    tracing::info!(outcome = "code_issued", "oauth");
    redirect(
        &txn.redirect_uri,
        &[("code", &code), ("state", &txn.state), ("iss", &o.issuer)],
    )
}

/// Exchanges Google's authorization code (with the PKCE verifier) for its token reply.
async fn google_exchange(o: &OAuth, code: &str, verifier: &str) -> Option<Value> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", o.cfg.google_client_id.as_str()),
        ("client_secret", o.cfg.google_client_secret.expose()),
        (
            "redirect_uri",
            &format!("{}/auth/callback", o.cfg.public_url),
        ),
        ("code_verifier", verifier),
    ];
    let rb = o
        .http
        .post(o.cfg.google_token_endpoint.clone())
        .timeout(Duration::from_secs(10))
        .form(&form);
    o.fetch_json(rb, MAX_GOOGLE_BYTES).await
}

/// The end of a web app sign-in: a session for an allowed Google account, then the app.
async fn web_callback(o: &OAuth, q: &HashMap<String, String>, verifier: &str) -> Response {
    let refused = |outcome: &str, msg: &str| {
        tracing::info!(outcome, "oauth");
        error_page(StatusCode::FORBIDDEN, msg)
    };
    let (Some(code), None) = (q.get("code"), q.get("error")) else {
        return refused(
            "web_upstream_error",
            "Sign-in was cancelled. Go back and try again.",
        );
    };
    let Some(reply) = google_exchange(o, code, verifier).await else {
        return refused(
            "web_exchange_failed",
            "Google sign-in failed. Go back and try again.",
        );
    };
    let Some((_, email)) = reply
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(|t| check_id_token(o, t, now()))
    else {
        return refused("web_identity_refused", "That Google account can't use Arr.");
    };
    let Some(id) = o.web.create_session("google", Some(&email)) else {
        return refused("web_session_failed", "Could not sign you in. Try again.");
    };
    tracing::info!(outcome = "web_session_issued", "oauth");
    let mut r = StatusCode::SEE_OTHER.into_response();
    r.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/"));
    let secure = o.cfg.public_url.starts_with("https://");
    if let Ok(v) = HeaderValue::from_str(&crate::webapp::session_cookie(&id, secure)) {
        r.headers_mut().insert(header::SET_COOKIE, v);
    }
    r
}

fn decode_part(part: &str) -> Option<Value> {
    serde_json::from_slice(&B64.decode(part).ok()?).ok()
}

/// Returns `(sub, email)` of an acceptable Google ID token.
///
/// The signature is not verified: OIDC Core 3.1.3.7 allows skipping it when the ID token is
/// received directly from the token endpoint over TLS, which is the only way one arrives here
/// (the client secret and PKCE verifier authenticated the exchange). The claims are still
/// checked.
fn check_id_token(o: &OAuth, token: &str, now: i64) -> Option<(String, String)> {
    let claims = decode_part(token.split('.').nth(1)?)?;
    let client_id = o.cfg.google_client_id.as_str();
    if !matches!(
        claims.get("iss")?.as_str()?,
        "https://accounts.google.com" | "accounts.google.com"
    ) {
        return None;
    }
    let azp = claims.get("azp");
    if azp.is_some_and(|a| a.as_str() != Some(client_id)) {
        return None;
    }
    match claims.get("aud")? {
        Value::String(a) if a == client_id => {}
        Value::Array(a) if a.iter().any(|v| v.as_str() == Some(client_id)) => {
            if a.len() > 1 && azp.is_none() {
                return None;
            }
        }
        _ => return None,
    }
    if claims.get("exp")?.as_f64()? <= now as f64 {
        return None;
    }
    let verified = match claims.get("email_verified")? {
        Value::Bool(b) => *b,
        Value::String(s) => s == "true",
        _ => false,
    };
    let email = claims.get("email")?.as_str()?.to_ascii_lowercase();
    let sub = claims.get("sub")?.as_str().filter(|s| !s.is_empty())?;
    (verified && o.email_allowed(&email)).then(|| (sub.to_string(), email))
}

// -------------------------------------------------------------------- /token

#[derive(Deserialize, Default)]
struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    client_assertion: Option<String>,
    client_assertion_type: Option<String>,
}

async fn token(
    State(o): Oa,
    form: Result<Form<TokenForm>, axum::extract::rejection::FormRejection>,
) -> Result<Response, TokenError> {
    let Form(f) = form.map_err(|_| invalid_request("malformed form body"))?;
    let client_id = authenticate_client(&o, &f).await.inspect_err(|_| {
        tracing::info!(outcome = "token_client_refused", "oauth");
    })?;
    let missing = || invalid_request("missing parameter");
    let granted = match f.grant_type.as_deref() {
        Some("authorization_code") => {
            let code = f.code.as_deref().ok_or_else(missing)?;
            let redirect_uri = f.redirect_uri.as_deref().ok_or_else(missing)?;
            let verifier = f.code_verifier.as_deref().ok_or_else(missing)?;
            if !(43..=128).contains(&verifier.len())
                || !verifier
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
            {
                return Err(invalid_request("invalid code_verifier"));
            }
            let bad = || invalid_grant("invalid or expired authorization code");
            let c = o.codes.lock().unwrap().take(code).ok_or_else(bad)?;
            let computed = B64.encode(Sha256::digest(verifier.as_bytes()));
            if c.client_id != client_id
                || c.redirect_uri != redirect_uri
                || !bool::from(computed.as_bytes().ct_eq(c.challenge.as_bytes()))
                || !o.email_allowed(&c.email)
            {
                return Err(bad());
            }
            (c.sub, c.email)
        }
        Some("refresh_token") => {
            let rt = f.refresh_token.as_deref().ok_or_else(missing)?;
            let bad = || invalid_grant("invalid or expired refresh token");
            if !rt.starts_with(REFRESH_PREFIX) {
                return Err(bad());
            }
            // Rotation: the presented token is deleted whatever happens next.
            let row = o.db.lock().unwrap().query_row(
                "DELETE FROM tokens WHERE hash = ?1 AND kind = 'refresh' RETURNING sub, email, client_id, expires_at",
                [sha256_hex(rt)],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)),
            );
            match row {
                Ok((sub, email, cid, exp))
                    if cid == client_id && exp > now() && o.email_allowed(&email) =>
                {
                    (sub, email)
                }
                _ => return Err(bad()),
            }
        }
        Some(_) => {
            return Err(TokenError(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                "grant_type must be authorization_code or refresh_token",
            ));
        }
        None => return Err(missing()),
    };
    let issued = o
        .issue_tokens(&client_id, &granted.0, &granted.1)
        .ok_or(TokenError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "could not store the tokens",
        ))?;
    tracing::info!(outcome = "tokens_issued", "oauth");
    Ok(Json(issued).into_response())
}

/// Authenticates the client and returns its `client_id`.
async fn authenticate_client(o: &OAuth, f: &TokenForm) -> Result<String, TokenError> {
    if f.client_secret.is_some() {
        return Err(invalid_client("client_secret is not supported"));
    }
    if let Some(kind) = f.client_assertion_type.as_deref() {
        let assertion = f.client_assertion.as_deref();
        return match (kind == ASSERTION_TYPE, assertion) {
            (true, Some(a)) => client_assertion(o, f, a).await,
            _ => Err(invalid_client("unsupported client authentication")),
        };
    }
    let id = f
        .client_id
        .as_deref()
        .ok_or_else(|| invalid_client("client authentication required"))?;
    let doc = o
        .client_doc(id)
        .await
        .ok_or_else(|| invalid_client("unknown client"))?;
    if doc.auth_method != "none" {
        return Err(invalid_client(
            "this client must authenticate with a signed assertion",
        ));
    }
    Ok(id.to_string())
}

/// Verifies a `private_key_jwt` assertion against the client's published keys.
async fn client_assertion(o: &OAuth, f: &TokenForm, jwt: &str) -> Result<String, TokenError> {
    let bad = invalid_client;
    let parts: Vec<&str> = jwt.split('.').collect();
    let [h, c, s] = parts[..] else {
        return Err(bad("malformed assertion"));
    };
    if jwt.len() > 8192 {
        return Err(bad("malformed assertion"));
    }
    let (Some(header), Some(claims), Ok(sig)) = (decode_part(h), decode_part(c), B64.decode(s))
    else {
        return Err(bad("malformed assertion"));
    };
    if header.get("alg").and_then(Value::as_str) != Some("RS256") {
        return Err(bad("assertion algorithm must be RS256"));
    }
    let cid = claims
        .get("iss")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if claims.get("sub").and_then(Value::as_str) != Some(cid)
        || f.client_id.as_deref().is_some_and(|p| p != cid)
    {
        return Err(bad("assertion issuer and subject must be the client_id"));
    }
    let doc = o
        .client_doc(cid)
        .await
        .ok_or_else(|| bad("unknown client"))?;
    let jwks_uri = doc
        .jwks_uri
        .as_deref()
        .filter(|u| doc.auth_method == "private_key_jwt" && same_origin(u, cid))
        .ok_or_else(|| bad("client has no usable jwks_uri"))?;
    let kid = header.get("kid").and_then(Value::as_str);
    let signed = &jwt[..h.len() + 1 + c.len()];
    let mut verified = false;
    for fresh in [false, true] {
        let Some((keys, fetched)) = o.client_keys(jwks_uri, fresh).await else {
            break;
        };
        let mut candidates = keys
            .iter()
            .filter(|k| kid.is_none() || k.kid.as_deref() == kid);
        verified = candidates.any(|k| {
            RsaPublicKeyComponents { n: &k.n, e: &k.e }
                .verify(&RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &sig)
                .is_ok()
        });
        // Refetch once, and only for a key id we do not know.
        let unknown_kid = kid.is_some() && !keys.iter().any(|k| k.kid.as_deref() == kid);
        if verified || fetched || !unknown_kid {
            break;
        }
    }
    if !verified {
        return Err(bad("assertion signature invalid"));
    }
    let t = now();
    let aud_ok = match claims.get("aud") {
        Some(Value::String(a)) => vec![a.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    }
    .iter()
    .any(|a| {
        *a == format!("{}/token", o.cfg.public_url) || a.trim_end_matches('/') == o.cfg.public_url
    });
    let num = |k: &str| claims.get(k).and_then(Value::as_f64);
    let exp = num("exp").unwrap_or(0.0) as i64;
    let jti = claims
        .get("jti")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !aud_ok
        || exp <= t
        || exp > t + 3600
        || num("iat").is_some_and(|v| v > (t + 60) as f64)
        || num("nbf").is_some_and(|v| v > (t + 60) as f64)
        || jti.is_empty()
        || jti.len() > 256
    {
        return Err(bad("assertion claims invalid"));
    }
    let mut seen = o.jtis.lock().unwrap();
    seen.retain(|_, e| *e > t);
    if seen.len() >= MAX_JTIS || seen.insert(format!("{cid}\n{jti}"), exp).is_some() {
        return Err(bad("assertion already used"));
    }
    Ok(cid.to_string())
}

fn same_origin(a: &str, b: &str) -> bool {
    match (Url::parse(a), Url::parse(b)) {
        (Ok(a), Ok(b)) => a.origin() == b.origin() && a.origin().is_tuple(),
        _ => false,
    }
}
