//! MCP for arr, built on rmcp (the official Rust SDK), with one tool adapter and
//! two transports:
//!
//! - `arr mcp`: stdio, a client of a running server's authenticated REST API;
//! - `[mcp] enabled` in `serve`: remote Streamable HTTP at `/mcp` in the same process
//!   (stateless JSON responses), which calls the API router in-process (the same auth,
//!   limits, deadline and contracts, without depending on the configured listen address).
//!   Each request passes `Host` and `Origin` validation and its own bearer token (not the API
//!   token; or an OAuth access token, see `oauth.rs`) before rmcp parses it; bodies, concurrency and time are bounded. The six tools and
//!   their contracts are identical in both transports.
//!
//! It keeps no state and knows no other service. Every tool is one or two REST calls with a
//! fixed shape, never retried. The API's typed errors come back verbatim (`not_adopted`,
//! `native_handoff_required` and the rest), and every failure says what it means for the
//! request (`outcome`): a write whose reply was lost may have been applied, so it is never
//! reported as unchanged. An unreachable API is an error: there is no fallback. Tool
//! arguments carry only titles, canonical media ids and release refs. Handoff confirmations,
//! monitoring and policy overrides, paths, URLs, deletion and torrent control cannot be
//! expressed.

use crate::ids::MediaId;
use crate::{config, fsutil};
use anyhow::{Context, Result};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use reqwest::{Method, Url};
use rmcp::handler::server::common::schema_for_input;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::RequestContext;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

/// Largest API response body accepted.
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
/// Largest incoming MCP message (one line); longer lines are dropped by the transport.
const MAX_MESSAGE_BYTES: usize = 256 * 1024;
/// The API's own limit for `q`.
const MAX_QUERY_CHARS: usize = 100;
const DEFAULT_LIST: u32 = 20;
const MAX_LIST: u32 = 50;

/// What the coexistence refusals mean for a conversational request.
const NATIVE_ROUTE: &str = "This title is still acquired by Sonarr/Radarr. It moves here only when an operator adopts it outside this assistant (native ledger and pause first). This assistant cannot confirm that handoff and does not fall back to Sonarr/Radarr.";
const OUTAGE: &str =
    "Nothing else was tried: this adapter has no fallback. Retry later or report the outage.";
const UNCERTAIN: &str = "The request may have been applied before its reply was lost. Check media_status or requests before repeating it. This adapter never retries a write and has no fallback; Sonarr/Radarr are not contacted.";

/// Refusals the API returns before a handler runs.
const BEFORE_HANDLER: &[&str] = &[
    "unauthorized",
    "overloaded",
    "payload_too_large",
    "unsupported_media_type",
    "invalid_json",
    "invalid_body",
];

pub struct Options {
    pub api_url: String,
    pub token_file: PathBuf,
    pub timeout: Duration,
}

/// Serves MCP on stdin/stdout until the client closes the session. Only MCP messages are
/// written to stdout.
pub async fn run(o: Options) -> Result<()> {
    let adapter = Adapter::over_http(&o)?;
    let (stdin, stdout) = rmcp::transport::stdio();
    let transport =
        AsyncRwTransport::new_server(stdin, stdout).with_max_line_length(MAX_MESSAGE_BYTES);
    let service = adapter
        .serve(transport)
        .await
        .context("MCP session start")?;
    service.waiting().await.context("MCP session")?;
    Ok(())
}

/// Checks every `/mcp` request before rmcp sees it: `Host`, `Origin`, the MCP bearer token
/// (constant time), the body size and the number of requests in flight.
struct RemoteGuard {
    token: crate::config::Secret,
    hosts: Vec<String>,
    origins: Vec<String>,
    max_body: usize,
    slots: std::sync::Arc<tokio::sync::Semaphore>,
    timeout: Duration,
    /// OAuth sign-in (`[mcp.oauth]`): its access tokens are accepted besides `token`.
    oauth: Option<std::sync::Arc<crate::oauth::OAuth>>,
}

pub(crate) fn host_allowed(host: &str, allowed: &[String]) -> bool {
    let Ok(a) = axum::http::uri::Authority::try_from(host) else {
        return false;
    };
    let h = a.host().trim_matches(['[', ']']).to_ascii_lowercase();
    allowed.iter().any(
        |entry| match axum::http::uri::Authority::try_from(entry.as_str()) {
            Ok(e) if e.port_u16().is_some() => {
                e.host().trim_matches(['[', ']']).eq_ignore_ascii_case(&h)
                    && e.port_u16() == a.port_u16()
            }
            _ => entry.trim_matches(['[', ']']).eq_ignore_ascii_case(&h),
        },
    )
}

fn refuse(status: axum::http::StatusCode, code: &str, message: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut r = (
        status,
        axum::Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response();
    if status == axum::http::StatusCode::UNAUTHORIZED {
        r.headers_mut().insert(
            axum::http::header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer"),
        );
    }
    r
}

async fn remote_guard(
    axum::extract::State(g): axum::extract::State<std::sync::Arc<RemoteGuard>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use subtle::ConstantTimeEq;
    let started = std::time::Instant::now();
    let method = req.method().clone();
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| req.uri().authority().map(|a| a.to_string()));
    let resp = 'checked: {
        if !host.as_deref().is_some_and(|h| host_allowed(h, &g.hosts)) {
            break 'checked refuse(
                StatusCode::FORBIDDEN,
                "host_not_allowed",
                "the Host header is not an allowed MCP host (mcp.allowed_hosts)",
            );
        }
        if let Some(o) = req.headers().get(header::ORIGIN) {
            let ok = o
                .to_str()
                .ok()
                .and_then(|o| Url::parse(o).ok())
                .is_some_and(|u| g.origins.contains(&u.origin().ascii_serialization()));
            if !ok {
                break 'checked refuse(
                    StatusCode::FORBIDDEN,
                    "origin_not_allowed",
                    "the Origin is not an allowed MCP origin (mcp.allowed_origins)",
                );
            }
        }
        let presented = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim);
        let static_token =
            presented.is_some_and(|t| bool::from(t.as_bytes().ct_eq(g.token.expose().as_bytes())));
        let oauth_token =
            presented.is_some_and(|t| g.oauth.as_ref().is_some_and(|o| o.verify_access(t)));
        if !static_token && !oauth_token {
            let mut r = refuse(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or invalid MCP bearer token",
            );
            if let Some(o) = &g.oauth {
                r.headers_mut()
                    .insert(header::WWW_AUTHENTICATE, o.challenge());
            }
            break 'checked r;
        }
        if req
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|l| l > g.max_body as u64)
        {
            break 'checked refuse(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "the MCP request body exceeds mcp.max_body_bytes",
            );
        }
        let Ok(_permit) = g.slots.clone().try_acquire_owned() else {
            break 'checked refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded",
                "too many MCP requests in flight; retry shortly",
            );
        };
        match tokio::time::timeout(g.timeout, next.run(req)).await {
            Ok(r) => r,
            Err(_) => refuse(
                StatusCode::GATEWAY_TIMEOUT,
                "request_timeout",
                "the MCP request exceeded mcp.timeout_secs",
            ),
        }
    };
    tracing::info!(%method, path = "/mcp", status = resp.status().as_u16(), elapsed_ms = started.elapsed().as_millis() as u64, "mcp request");
    resp
}

/// The remote MCP endpoint (`/mcp`) over `api` (this process's API router), with a closure
/// that ends it at shutdown.
pub fn remote(
    cfg: &crate::config::McpConfig,
    api: axum::Router,
    api_token: &str,
    state_dir: &std::path::Path,
) -> Result<(axum::Router, impl FnOnce() + Send + 'static)> {
    use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    let adapter = Adapter::local(api, api_token, cfg.timeout)?;
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_allowed_hosts(cfg.allowed_hosts.clone())
        .with_allowed_origins(cfg.allowed_origins.clone())
        .enforce_origin_validation()
        .with_max_request_body_bytes(cfg.max_body_bytes);
    let cancel = config.cancellation_token.clone();
    let service = StreamableHttpService::new(
        move || Ok(adapter.clone()),
        std::sync::Arc::new(NeverSessionManager::default()),
        config,
    );
    let oauth = cfg
        .oauth
        .as_ref()
        .map(|o| crate::oauth::OAuth::new(o, &cfg.allowed_hosts, state_dir))
        .transpose()?;
    let guard = std::sync::Arc::new(RemoteGuard {
        token: cfg.token.clone(),
        hosts: cfg.allowed_hosts.clone(),
        origins: cfg.allowed_origins.clone(),
        max_body: cfg.max_body_bytes,
        slots: std::sync::Arc::new(tokio::sync::Semaphore::new(cfg.max_concurrent)),
        timeout: cfg.timeout + Duration::from_secs(5),
        oauth: oauth.clone(),
    });
    let mut router = axum::Router::new()
        .route_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(guard, remote_guard));
    if let Some(o) = oauth {
        router = router.merge(crate::oauth::router(o));
    }
    Ok((router, move || cancel.cancel()))
}

/// What a call can change, which decides what its failure means.
#[derive(Clone, Copy, PartialEq)]
enum Effect {
    /// GET: changes nothing.
    Read,
    /// POST /library.
    Add,
    /// POST /grabs.
    Grab,
    /// POST /stop.
    Stop,
}

impl Effect {
    /// Whether this typed API error is returned before anything changes. Any other error on
    /// a write has an unknown outcome.
    fn refused_unchanged(self, code: &str) -> bool {
        if BEFORE_HANDLER.contains(&code) {
            return true;
        }
        match self {
            Effect::Read => true,
            // Other errors can follow a committed add (for example a failed automatic search).
            Effect::Add => matches!(code, "native_handoff_required" | "unsupported_scope"),
            // A stop refused for its id or because Arr never had it changed nothing.
            Effect::Stop => matches!(code, "invalid_media_id" | "not_managed"),
            // Every refusal of a grab comes before the download is recorded (a contradicting
            // payload is blocklisted).
            Effect::Grab => matches!(
                code,
                "invalid_media_id"
                    | "invalid_request"
                    | "catalog_not_imported"
                    | "release_not_found"
                    | "not_adopted"
                    | "execution_disabled"
                    | "client_not_configured"
                    | "category_shared"
                    | "already_queued"
                    | "blocklisted"
                    | "duplicate_torrent"
                    | "invalid_payload"
                    | "no_download_link"
                    | "wrong_payload"
                    | "no_peer_discovery"
                    | "indexer_limit_reached"
                    | "upstream_timeout"
                    | "upstream_auth"
                    | "upstream_rate_limited"
                    | "upstream_unavailable"
                    | "upstream_bad_response"
            ),
        }
    }
}

/// How the adapter reaches the REST API.
#[derive(Clone)]
enum Transport {
    /// A running server's URL (stdio mode).
    Http { http: reqwest::Client, base: Url },
    /// The API router of this process (remote mode): no socket, no URL.
    Local {
        router: axum::Router,
        timeout: Duration,
    },
}

#[derive(Clone)]
pub struct Adapter {
    transport: Transport,
    auth: HeaderValue,
    token: String,
}

impl Adapter {
    /// The remote transport's adapter: calls `router` (the API) in-process with `api_token`.
    pub fn local(router: axum::Router, api_token: &str, timeout: Duration) -> Result<Self> {
        let mut auth = HeaderValue::from_str(&format!("Bearer {api_token}"))
            .ok()
            .context("the API token is not a usable bearer token")?;
        auth.set_sensitive(true);
        Ok(Adapter {
            transport: Transport::Local { router, timeout },
            auth,
            token: api_token.to_string(),
        })
    }

    fn over_http(o: &Options) -> Result<Self> {
        let base = config::parse_base_url(&o.api_url, "--api-url")?;
        let token = fsutil::read_secret_file(&o.token_file).context("--token-file")?;
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))
            .ok()
            .context("--token-file does not hold a usable bearer token")?;
        auth.set_sensitive(true);
        // No proxies and no redirects: the token goes to the configured URL only.
        let http = reqwest::Client::builder()
            .timeout(o.timeout)
            .connect_timeout(o.timeout.min(Duration::from_secs(10)))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .user_agent(concat!("arr-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("HTTP client")?;
        Ok(Adapter {
            transport: Transport::Http { http, base },
            auth,
            token,
        })
    }

    fn url(base: &Url, segments: &[&str], query: &[(&str, String)]) -> Url {
        let mut url = base.clone();
        url.path_segments_mut()
            .expect("http(s) base URLs have path segments")
            .pop_if_empty()
            .extend(segments);
        if !query.is_empty() {
            let mut q = url.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
        }
        url
    }

    /// One API call, never retried. `Err` is the tool result to return: the API's own typed
    /// error, or an adapter error when the API did not answer usably.
    async fn call(
        &self,
        effect: Effect,
        segments: &[&str],
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value, CallToolResult> {
        let method = if effect == Effect::Read {
            Method::GET
        } else {
            Method::POST
        };
        let (status, raw) = match &self.transport {
            Transport::Http { http, base } => {
                self.send_http(http, base, method, effect, segments, query, body)
                    .await?
            }
            Transport::Local { router, timeout } => {
                self.send_local(router, *timeout, method, effect, segments, query, body)
                    .await?
            }
        };
        // Defence in depth: the API never echoes the token, but nothing that does leaves here.
        let text = String::from_utf8_lossy(&raw).replace(&self.token, "[redacted]");
        let value: Option<Value> = serde_json::from_str(&text).ok();
        if (200..300).contains(&status) {
            return value.ok_or_else(|| {
                unusable(
                    effect,
                    true,
                    "api_bad_response",
                    &format!("the API answered HTTP {status} without JSON"),
                )
            });
        }
        let core = value
            .as_ref()
            .and_then(|v| v.get("error"))
            .filter(|e| e["code"].is_string() && e["message"].is_string());
        Err(match core {
            Some(e) => api_error(effect, e.clone(), status),
            None => unusable(
                effect,
                true,
                "api_bad_response",
                &format!("the API answered HTTP {status} without a typed error"),
            ),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_http(
        &self,
        http: &reqwest::Client,
        base: &Url,
        method: Method,
        effect: Effect,
        segments: &[&str],
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<(u16, Vec<u8>), CallToolResult> {
        let mut req = http
            .request(method, Self::url(base, segments, query))
            .header(AUTHORIZATION, self.auth.clone());
        if let Some(b) = body {
            req = req
                .header(CONTENT_TYPE, "application/json")
                .body(b.to_string());
        }
        // Only a connection that was never made shows that the request was not delivered.
        let mut resp = req.send().await.map_err(|e| {
            let sent = !e.is_connect();
            let (code, message) = if e.is_timeout() {
                ("api_timeout", "the arr API did not answer in time")
            } else if sent {
                ("api_unreachable", "the connection to the arr API failed")
            } else {
                ("api_unreachable", "the arr API could not be reached")
            };
            unusable(effect, sent, code, message)
        })?;
        let status = resp.status().as_u16();
        if resp
            .content_length()
            .is_some_and(|l| l > MAX_RESPONSE_BYTES as u64)
        {
            return Err(too_large(effect));
        }
        let mut raw = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if raw.len() + chunk.len() > MAX_RESPONSE_BYTES {
                        return Err(too_large(effect));
                    }
                    raw.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    let (code, message) = if e.is_timeout() {
                        (
                            "api_timeout",
                            "the arr API did not finish its answer in time",
                        )
                    } else {
                        (
                            "api_unreachable",
                            "the arr API closed the connection during its answer",
                        )
                    };
                    return Err(unusable(effect, true, code, message));
                }
            }
        }
        Ok((status, raw))
    }

    /// One call to the API router of this process (no socket): the same middleware (auth,
    /// load shedding, deadline, body limit) and handlers as an HTTP request.
    #[allow(clippy::too_many_arguments)]
    async fn send_local(
        &self,
        router: &axum::Router,
        timeout: Duration,
        method: Method,
        effect: Effect,
        segments: &[&str],
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<(u16, Vec<u8>), CallToolResult> {
        use tower::ServiceExt;
        let base = Url::parse("http://arr.internal/").expect("static URL");
        let url = Self::url(&base, segments, query);
        let uri = match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_string(),
        };
        let mut req = axum::http::Request::builder()
            .method(method.as_str())
            .uri(uri)
            .header(AUTHORIZATION.as_str(), self.auth.as_bytes());
        if body.is_some() {
            req = req.header(CONTENT_TYPE.as_str(), "application/json");
        }
        let req = req
            .body(axum::body::Body::from(
                body.map(|b| b.to_string()).unwrap_or_default(),
            ))
            .map_err(|_| {
                unusable(
                    effect,
                    false,
                    "api_bad_request",
                    "the request could not be built",
                )
            })?;
        let call = async {
            let resp =
                router.clone().oneshot(req).await.map_err(|_| {
                    unusable(effect, true, "api_unreachable", "the API router failed")
                })?;
            let status = resp.status().as_u16();
            let raw = axum::body::to_bytes(resp.into_body(), MAX_RESPONSE_BYTES)
                .await
                .map_err(|_| too_large(effect))?;
            Ok::<_, CallToolResult>((status, raw.to_vec()))
        };
        match tokio::time::timeout(timeout, call).await {
            Ok(r) => r,
            Err(_) => Err(unusable(
                effect,
                true,
                "api_timeout",
                "the arr API did not answer in time",
            )),
        }
    }

    async fn search_titles(&self, a: SearchTitles) -> CallToolResult {
        let q = a.q.trim();
        if q.is_empty() || q.chars().count() > MAX_QUERY_CHARS || q.chars().any(char::is_control) {
            return invalid(&format!(
                "q must be 1 to {MAX_QUERY_CHARS} printable characters"
            ));
        }
        if a.year.is_some_and(|y| !(1870..=2100).contains(&y)) {
            return invalid("year must be between 1870 and 2100");
        }
        let mut query = vec![("kind", a.kind.as_str().to_string()), ("q", q.to_string())];
        if let Some(y) = a.year {
            query.push(("year", y.to_string()));
        }
        respond(
            self.call(Effect::Read, &["api", "v1", "discover"], &query, None)
                .await,
        )
    }

    async fn add_media(&self, a: MediaArg) -> CallToolResult {
        let id = match canonical(&a.media_id, Scope::Acquirable) {
            Ok(id) => id,
            Err(r) => return r,
        };
        // The REST contract's defaults apply: monitored, and searched automatically.
        respond(
            self.call(
                Effect::Add,
                &["api", "v1", "library"],
                &[],
                Some(json!({ "media_id": id })),
            )
            .await
            .map(automatic_add),
        )
    }

    async fn stop(&self, a: MediaArg) -> CallToolResult {
        let id = match canonical(&a.media_id, Scope::Acquirable) {
            Ok(id) => id,
            Err(r) => return r,
        };
        respond(
            self.call(
                Effect::Stop,
                &["api", "v1", "stop"],
                &[],
                Some(json!({ "media_id": id })),
            )
            .await
            .map(stop_summary),
        )
    }

    async fn media_status(&self, a: StatusArg) -> CallToolResult {
        let id = match canonical(&a.media_id, Scope::Any) {
            Ok(id) => id,
            Err(r) => return r,
        };
        respond(
            self.call(Effect::Read, &["api", "v1", "status", &id], &[], None)
                .await,
        )
    }

    async fn requests(&self, a: RequestsArg) -> CallToolResult {
        let limit = a.limit.unwrap_or(DEFAULT_LIST);
        if !(1..=MAX_LIST).contains(&limit) {
            return invalid(&format!("limit must be between 1 and {MAX_LIST}"));
        }
        let queue = match self
            .call(
                Effect::Read,
                &["api", "v1", "queue"],
                &[("limit", limit.to_string())],
                None,
            )
            .await
        {
            Ok(v) => v,
            Err(r) => return r,
        };
        let requests = match self
            .call(Effect::Read, &["api", "v1", "requests"], &[], None)
            .await
        {
            Ok(v) => v,
            Err(r) => return r,
        };
        let items = requests["items"].as_array().cloned().unwrap_or_default();
        let shown: Vec<Value> = items.iter().take(limit as usize).cloned().collect();
        CallToolResult::structured(json!({
            "queue": queue,
            "requests": {
                "total": items.len(),
                "items": shown,
                "omitted": items.len().saturating_sub(limit as usize),
                "note": requests["note"],
            },
        }))
    }

    async fn library(&self, a: LibraryArg) -> CallToolResult {
        let limit = a.limit.unwrap_or(DEFAULT_LIBRARY_PAGE);
        if !(1..=MAX_LIBRARY_PAGE).contains(&limit) {
            return invalid(&format!("limit must be between 1 and {MAX_LIBRARY_PAGE}"));
        }
        let offset = a.offset.unwrap_or(0);
        if offset > MAX_LIBRARY_OFFSET {
            return invalid(&format!("offset must be at most {MAX_LIBRARY_OFFSET}"));
        }
        let mut query = vec![("limit", limit.to_string()), ("offset", offset.to_string())];
        if let Some(q) = a.q.as_deref().map(str::trim) {
            if q.is_empty()
                || q.chars().count() > MAX_QUERY_CHARS
                || q.chars().any(char::is_control)
            {
                return invalid(&format!(
                    "q must be 1 to {MAX_QUERY_CHARS} printable characters"
                ));
            }
            query.push(("q", q.to_string()));
        }
        let path = match a.kind {
            Kind::Movie => "movies",
            Kind::Series => "series",
        };
        match self
            .call(Effect::Read, &["api", "v1", "library", path], &query, None)
            .await
        {
            Ok(v) => CallToolResult::structured(library_page(&a.kind, &v, offset)),
            Err(r) => r,
        }
    }

    async fn release_choices(&self, a: MediaArg) -> CallToolResult {
        let id = match canonical(&a.media_id, Scope::Acquirable) {
            Ok(id) => id,
            Err(r) => return r,
        };
        respond(
            self.call(
                Effect::Read,
                &["api", "v1", "media", &id, "releases"],
                &[],
                None,
            )
            .await,
        )
    }

    async fn start_download(&self, a: StartDownload) -> CallToolResult {
        let id = match canonical(&a.media_id, Scope::Acquirable) {
            Ok(id) => id,
            Err(r) => return r,
        };
        let r = &a.release_ref;
        let well_formed = r.len() == 28
            && r.starts_with("rel_")
            && r[4..]
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if !well_formed {
            return invalid("release_ref must be a release_ref returned by release_choices");
        }
        respond(
            self.call(
                Effect::Grab,
                &["api", "v1", "grabs"],
                &[],
                Some(json!({ "media_id": id, "release_ref": r })),
            )
            .await,
        )
    }
}

impl ServerHandler for Adapter {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "arr",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Movie and TV requests for the arr server. To answer what the user already has, use library (their whole catalog, including everything imported from Plex) or media_status for one title. To add something, find it with search_titles, then call add_media with its canonical media id: that is the default, automatic path (monitored, then searched and downloaded by this server's own policy). Add exactly the scope the user asked for; do not replace a requested season with individual episodes unless the user agrees. release_choices and start_download are a manual mode, only for when the user explicitly asks to choose a release: start_download downloads only the exact release the user chose. Titles still managed by Sonarr/Radarr are refused (native_handoff_required, not_adopted) and are not adopted; adoption is an operator step outside this assistant. An add_media result's automatic field says what happened; a blocked plan with no release yet is normal with execution enabled, and plans are never executed by hand. Repeating an add or a download creates no duplicate. Errors are returned as is with their outcome; there is no fallback, and a write with an unknown outcome is checked with media_status or requests before it is repeated. If movie search reports that it isn't set up, tell the user what that message says; TV works without it.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let result = match request.name.as_ref() {
            "search_titles" => match parse(args) {
                Ok(a) => self.search_titles(a).await,
                Err(r) => r,
            },
            "add_media" => match parse(args) {
                Ok(a) => self.add_media(a).await,
                Err(r) => r,
            },
            "stop" => match parse(args) {
                Ok(a) => self.stop(a).await,
                Err(r) => r,
            },
            "media_status" => match parse(args) {
                Ok(a) => self.media_status(a).await,
                Err(r) => r,
            },
            "requests" => match parse(args) {
                Ok(a) => self.requests(a).await,
                Err(r) => r,
            },
            "library" => match parse(args) {
                Ok(a) => self.library(a).await,
                Err(r) => r,
            },
            "release_choices" => match parse(args) {
                Ok(a) => self.release_choices(a).await,
                Err(r) => r,
            },
            "start_download" => match parse(args) {
                Ok(a) => self.start_download(a).await,
                Err(r) => r,
            },
            _ => return Err(ErrorData::invalid_params("tool not found", None)),
        };
        Ok(result.into())
    }
}

// ------------------------------------------------------------------ tool arguments

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
enum Kind {
    Movie,
    Series,
}

impl Kind {
    fn as_str(&self) -> &'static str {
        match self {
            Kind::Movie => "movie",
            Kind::Series => "series",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchTitles {
    /// What to look for.
    kind: Kind,
    /// Title words, e.g. "the matrix".
    #[schemars(length(min = 1, max = 100))]
    q: String,
    /// Release year (movies) or first-air year (series).
    #[serde(default)]
    #[schemars(range(min = 1870, max = 2100))]
    year: Option<u16>,
}

const ACQUIRABLE_ID: &str = r"^(movie:tmdb:[1-9][0-9]{0,9}|season:(tvdb|tvmaze):[1-9][0-9]{0,9}:[1-9][0-9]{0,2}|episode:(tvdb|tvmaze):[1-9][0-9]{0,9}:[1-9][0-9]{0,2}:[1-9][0-9]{0,3})$";
const ANY_ID: &str = r"^(movie:tmdb:[1-9][0-9]{0,9}|series:(tvdb|tvmaze):[1-9][0-9]{0,9}|season:(tvdb|tvmaze):[1-9][0-9]{0,9}:[1-9][0-9]{0,2}|episode:(tvdb|tvmaze):[1-9][0-9]{0,9}:[1-9][0-9]{0,2}:[1-9][0-9]{0,3})$";

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MediaArg {
    /// Canonical id: movie:tmdb:<id>, season:<tvdb|tvmaze>:<id>:<season> or episode:<tvdb|tvmaze>:<id>:<season>:<episode>, for exactly the movie, season or episode the user asked for.
    #[schemars(regex(pattern = ACQUIRABLE_ID))]
    media_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusArg {
    /// Canonical id: movie:tmdb:<id>, series:<tvdb|tvmaze>:<id>, season:<tvdb|tvmaze>:<id>:<season> or episode:<tvdb|tvmaze>:<id>:<season>:<episode>.
    #[schemars(regex(pattern = ANY_ID))]
    media_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RequestsArg {
    /// How many downloads and requests to show (default 20).
    #[serde(default)]
    #[schemars(range(min = 1, max = 50))]
    limit: Option<u32>,
}

const DEFAULT_LIBRARY_PAGE: u32 = 50;
const MAX_LIBRARY_PAGE: u32 = 100;
const MAX_LIBRARY_OFFSET: u32 = 1_000_000;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LibraryArg {
    /// Movies or series.
    kind: Kind,
    /// Optional title words to narrow the list, e.g. "horses".
    #[serde(default)]
    #[schemars(length(min = 1, max = 100))]
    q: Option<String>,
    /// Titles per page (default 50).
    #[serde(default)]
    #[schemars(range(min = 1, max = 100))]
    limit: Option<u32>,
    /// Where the page starts: 0, then the previous page's next_offset.
    #[serde(default)]
    #[schemars(range(min = 0, max = 1_000_000))]
    offset: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StartDownload {
    /// The canonical id the chosen release was listed for by release_choices.
    #[schemars(regex(pattern = ACQUIRABLE_ID))]
    media_id: String,
    /// The release_ref of the approved release the user explicitly chose from release_choices for this media_id (valid for 7 days).
    #[schemars(regex(pattern = r"^rel_[0-9a-f]{24}$"))]
    release_ref: String,
}

fn tool<T: JsonSchema + 'static>(
    name: &'static str,
    description: &'static str,
    annotations: ToolAnnotations,
) -> Tool {
    let schema = schema_for_input::<T>().expect("tool arguments are JSON objects");
    Tool::new(name, description, schema).with_annotations(annotations)
}

fn tools() -> Vec<Tool> {
    let read = ToolAnnotations::new()
        .read_only(true)
        .destructive(false)
        .open_world(true);
    let write = ToolAnnotations::new()
        .read_only(false)
        .destructive(false)
        .idempotent(true)
        .open_world(true);
    vec![
        tool::<SearchTitles>(
            "search_titles",
            "Find movies or series by title. Results carry canonical media ids and whether each title is already known here (in_catalog).",
            read.clone(),
        ),
        tool::<MediaArg>(
            "add_media",
            "The default way to add what the user asked for: a movie, one season or one episode by canonical id, monitored, then searched and downloaded automatically by this server's policy. Add exactly the requested scope; do not substitute individual episodes for a requested season without the user's agreement. The result's automatic field says whether a download was queued or nothing could be downloaded yet. Repeating an add creates no duplicate. Titles still managed by Sonarr/Radarr are refused with native_handoff_required and are not adopted.",
            write.clone(),
        ),
        tool::<MediaArg>(
            "stop",
            "Stop getting a movie, season or episode: cancels a download that hasn't finished and stops Arr looking for it. Never deletes anything already in Plex. Use it when the user changes their mind about something they asked for. A download that is nearly done is left to finish. Repeating it is harmless.",
            write.clone(),
        ),
        tool::<StatusArg>(
            "media_status",
            "Ownership, acquisition state, per-season and per-episode progress, downloads and items needing attention for a canonical id.",
            read.clone(),
        ),
        tool::<RequestsArg>(
            "requests",
            "Current downloads (the active queue) and pending acquisition requests.",
            read.clone(),
        ),
        tool::<LibraryArg>(
            "library",
            "Lists the user's library: every movie or series this server knows, including everything imported from Plex, alphabetically with paging. Each movie says whether it is owned (a file is present); each series lists owned and total episodes per season. Use it for \"what's in my library\" or \"which seasons do I have\"; use search_titles to find something new.",
            read.clone(),
        ),
        tool::<MediaArg>(
            "release_choices",
            "Manual mode only, when the user explicitly asks to see or choose releases (adding something does not need it: use add_media). Searches the indexers for a movie, season or episode and lists releases with this server's decision; approved releases carry a release_ref.",
            read,
        ),
        tool::<StartDownload>(
            "start_download",
            "Manual mode only: downloads the one approved release the user explicitly chose from release_choices, for the same canonical id. Never choose a release on the user's behalf; add_media is the automatic default. A unit that is already being downloaded is refused (already_queued), so repeating this creates no duplicate. Titles still managed by Sonarr/Radarr are refused with not_adopted.",
            write,
        ),
    ]
}

// ------------------------------------------------------------------ results

/// A compact page of the catalog: what an assistant needs to answer "what do I have".
fn library_page(kind: &Kind, v: &Value, offset: u32) -> Value {
    let items: Vec<Value> = v["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|i| match kind {
                    Kind::Movie => json!({
                        "media_id": i["id"],
                        "title": i["title"],
                        "year": i["year"],
                        "owned": i["has_file"],
                        "quality": i["file_quality"],
                        "monitored": i["monitored"],
                        "downloading": i["queued"],
                    }),
                    Kind::Series => json!({
                        "media_id": i["id"],
                        "title": i["title"],
                        "year": i["year"],
                        "monitored": i["monitored"],
                        "episodes": i["episode_counts"],
                        "seasons": i["seasons"].as_array().map(|s| s.iter().map(|s| json!({
                            "season": s["number"],
                            "owned": s["episode_file_count"],
                            "episodes": s["total_episode_count"],
                        })).collect::<Vec<_>>()),
                    }),
                })
                .collect()
        })
        .unwrap_or_default();
    let total = v["total"].as_u64().unwrap_or(0);
    let end = u64::from(offset) + items.len() as u64;
    json!({
        "kind": kind.as_str(),
        "total": total,
        "offset": offset,
        "next_offset": if end < total && !items.is_empty() { json!(end) } else { Value::Null },
        "items": items,
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Scope {
    /// Movies, seasons and episodes: what this server acquires.
    Acquirable,
    /// Also whole series.
    Any,
}

/// The core's parser accepts canonical ids only (no aliases, padding or case changes).
fn canonical(raw: &str, scope: Scope) -> Result<String, CallToolResult> {
    match raw.parse::<MediaId>() {
        Ok(MediaId::Series(_)) if scope == Scope::Acquirable => Err(invalid(
            "whole series are not added through this tool: ask the user which season or episode they want",
        )),
        Ok(id) => Ok(id.to_string()),
        Err(_) => Err(invalid(
            "media_id must be a canonical id such as movie:tmdb:603, season:tvdb:372264:1 or episode:tvdb:372264:1:2",
        )),
    }
}

fn parse<T: DeserializeOwned>(args: serde_json::Map<String, Value>) -> Result<T, CallToolResult> {
    serde_json::from_value(Value::Object(args))
        .map_err(|e| invalid(&format!("invalid arguments: {e}")))
}

/// A stop's reply with a plain sentence saying what happened.
fn stop_summary(mut v: Value) -> Value {
    let cancelled = v["cancelled"].as_u64().unwrap_or(0);
    let finishing = v["finishing"].as_u64().unwrap_or(0);
    let mut text = format!(
        "Arr will not look for it any more. Cancelled {cancelled} download{}.",
        if cancelled == 1 { "" } else { "s" }
    );
    if finishing > 0 {
        text.push_str(&format!(
            " {finishing} {} still finishing and will be added to Plex.",
            if finishing == 1 { "is" } else { "are" }
        ));
    }
    text.push_str(" Nothing already in Plex was touched.");
    v["message"] = json!(text);
    v
}

/// A successful add as the assistant should read it; the core's facts are kept.
/// - `adoption.native_handoff_confirmed` only says whether this request carried the
///   operator's acknowledgement, which the adapter never sends. It is dropped. For an adopted
///   native title, `handoff: "on_record"` says what the success itself establishes: a
///   confirmed handoff covers the scope. The core's note becomes `operator_note`.
/// - A plan that was not executed because nothing was executable (blocked, already owned or
///   in progress) is not shown as a shadow plan to execute by hand.
/// - `automatic` summarises the outcome from the plan alone.
fn automatic_add(mut v: Value) -> Value {
    if let Some(a) = v.get_mut("adoption").and_then(Value::as_object_mut) {
        a.remove("native_handoff_confirmed");
        if let Some(note) = a.remove("note") {
            a.insert("operator_note".into(), note);
        }
        if a.get("native_app").is_some_and(|x| !x.is_null())
            && a.get("mode") == Some(&json!("coexistence"))
            && a.get("acquisition") == Some(&json!("adopted"))
        {
            a.insert("handoff".into(), json!("on_record"));
        }
    }
    let Some(state) = v["plan"]["state"].as_str().map(str::to_string) else {
        return v;
    };
    if !v["plan"]["execution"].is_object() {
        return v;
    }
    let ex = &mut v["plan"]["execution"];
    let count = |k: &str| ex[k].as_array().map_or(0, Vec::len);
    let (grabs, refused) = (count("grabs"), count("refused"));
    let (status, message) = if ex["download_submission"] != "enabled" {
        (
            "execution_disabled",
            "Execution is disabled on this server: this is a shadow plan and nothing is downloaded.".to_string(),
        )
    } else if ex["mode"] == "executed" && grabs > 0 {
        let partly = if refused > 0 {
            ", and some candidates were refused (plan.execution.refused)"
        } else {
            ""
        };
        (
            if refused > 0 {
                "partially_queued"
            } else {
                "download_queued"
            },
            format!(
                "{grabs} download(s) queued{partly}. Follow them with requests or media_status."
            ),
        )
    } else if ex["mode"] == "executed" {
        (
            "nothing_queued",
            "Every candidate was refused (plan.execution.refused); nothing was downloaded."
                .to_string(),
        )
    } else {
        let (status, message) = match state.as_str() {
            "blocked" => (
                "not_downloaded_yet",
                "Nothing could be downloaded now: no release was chosen (see plan.blockers and plan.units). This is not a trial run; execution is enabled. If a request is waiting (check requests or media_status), the server retries it and RSS can pick up a new release automatically. No manual step is needed.",
            ),
            "already_satisfied" => ("already_owned", "Already owned: nothing to download."),
            "in_progress" => (
                "in_progress",
                "Already owned or being downloaded: nothing new to download.",
            ),
            _ => return v,
        };
        ex["mode"] = json!("not_executed");
        ex["note"] = json!(format!(
            "automatic add: nothing was executable (plan {state}); nothing was submitted"
        ));
        (status, message.to_string())
    };
    v["automatic"] = json!({ "status": status, "message": message });
    v
}

fn respond(r: Result<Value, CallToolResult>) -> CallToolResult {
    match r {
        Ok(v) => CallToolResult::structured(v),
        Err(r) => r,
    }
}

/// Outcome of a failed call: `nothing_changed` (a read), `not_sent`, `not_applied` (refused
/// by the API before any change) or `unknown`.
fn outcome(effect: Effect, sent: bool, refused_unchanged: bool) -> &'static str {
    match effect {
        Effect::Read => "nothing_changed",
        _ if !sent => "not_sent",
        _ if refused_unchanged => "not_applied",
        _ => "unknown",
    }
}

/// Refused here, before any request.
fn invalid(message: &str) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "error": { "code": "invalid_arguments", "message": message },
        "outcome": "not_sent",
    }))
}

/// The API did not answer usably.
fn unusable(effect: Effect, sent: bool, code: &str, message: &str) -> CallToolResult {
    let outcome = outcome(effect, sent, false);
    CallToolResult::structured_error(json!({
        "error": { "code": code, "message": message },
        "outcome": outcome,
        "guidance": if outcome == "unknown" { UNCERTAIN } else { OUTAGE },
    }))
}

fn too_large(effect: Effect) -> CallToolResult {
    unusable(
        effect,
        true,
        "api_response_too_large",
        &format!("the API response is larger than {MAX_RESPONSE_BYTES} bytes"),
    )
}

/// The API's typed error, unchanged, with its outcome, and routing guidance for the
/// coexistence refusals.
fn api_error(effect: Effect, error: Value, status: u16) -> CallToolResult {
    let code = error["code"].as_str().unwrap_or_default().to_string();
    let outcome = outcome(effect, true, effect.refused_unchanged(&code));
    let mut v = json!({ "error": error, "http_status": status, "outcome": outcome });
    if outcome == "unknown" {
        v["guidance"] = json!(UNCERTAIN);
    }
    if matches!(
        code.as_str(),
        "not_adopted" | "native_handoff_required" | "unsupported_scope"
    ) {
        v["routing"] = json!(NATIVE_ROUTE);
    }
    CallToolResult::structured_error(v)
}
