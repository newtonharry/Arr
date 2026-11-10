//! Runtime configuration (`config.toml`). Secrets are referenced by file path only.

use crate::fsutil;
use anyhow::{Context, Result, bail};
use reqwest::Url;
use serde::Deserialize;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A secret value that never prints itself.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Secret(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    server: ServerSection,
    #[serde(default)]
    state: StateSection,
    prowlarr: Option<ProwlarrSection>,
    #[serde(default)]
    tvmaze: TvmazeSection,
    #[serde(default)]
    tmdb: TmdbSection,
    #[serde(default)]
    execution: ExecutionSection,
    qbittorrent: Option<QbitSection>,
    embedded: Option<EmbeddedSection>,
    library: Option<LibrarySection>,
    #[serde(default)]
    automation: AutomationSection,
    plex: Option<PlexSection>,
    #[serde(default)]
    migration: MigrationSection,
    /// Native indexers (`[[indexer]]`), searched directly instead of through Prowlarr.
    #[serde(default)]
    indexer: Vec<IndexerSection>,
    mcp: Option<McpSection>,
    #[serde(default)]
    reporting: ReportingSection,
    /// Tests only (`test-client` feature): the in-memory download client replaces the
    /// embedded engine. The section does not exist in a release build, which rejects it.
    #[cfg(feature = "test-client")]
    test_client: Option<TestClientSection>,
}

#[cfg(feature = "test-client")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TestClientSection {
    /// The registry key of the shared in-memory state (`client::fake::handle`).
    id: String,
}

/// Opt-in error reports (docs/PRIVACY.md): off unless the user said yes during setup.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ReportingSection {
    #[serde(default)]
    enabled: bool,
}

/// One native indexer: The Pirate Bay's JSON API or a generic Torznab endpoint.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexerSection {
    /// Stable id. Imported Radarr/Sonarr indexer settings reference Prowlarr indexer ids;
    /// the native indexer with the same id serves them, so imported ids never change.
    id: u32,
    name: String,
    /// `thepiratebay`, `knaben` or `torznab`.
    kind: String,
    /// The exact trusted API base URL (TPB: the apibay-style JSON API; Torznab: the
    /// endpoint's base, `api_path` is appended). No credentials, query or fragment.
    base_url: String,
    /// Torznab: owner-only file holding the API key (sent only to `base_url`'s origin).
    api_key_file: Option<String>,
    /// Torznab: the API path below `base_url` (default `api`).
    api_path: Option<String>,
    /// Torznab: other origins whose `.torrent` links may be fetched (no key is sent there).
    download_origins: Option<Vec<String>>,
    /// Torznab: the Prowlarr definition this endpoint stands in for, when the imported
    /// settings name one (an explicit operator statement; never inferred).
    prowlarr_definition: Option<String>,
    /// Torznab: origins besides `base_url`'s that may receive the API key inside magnet
    /// tracker or `.torrent` announce URLs (a private tracker announcing on another host).
    /// Default none: such payloads are refused.
    key_origins: Option<Vec<String>>,
    /// TPB: public trackers added to magnets built from info-hashes (default none).
    magnet_trackers: Option<Vec<String>>,
    /// TPB: only results from these uploader names (default: all uploaders).
    uploaders: Option<Vec<String>>,
    /// TPB: read `precompiled/data_top100_recent.json` for RSS (default true). Knaben: read
    /// its latest releases for RSS (default false).
    recent_feed: Option<bool>,
    #[serde(default = "d_indexer_timeout")]
    timeout_secs: u64,
    #[serde(default = "d_indexer_concurrency")]
    max_concurrent: usize,
    /// Minimum spacing between two requests to this indexer.
    #[serde(default = "d_min_interval")]
    min_interval_ms: u64,
    #[serde(default = "d_cache_ttl")]
    cache_ttl_secs: u64,
    #[serde(default = "d_indexer_response")]
    max_response_bytes: usize,
    /// Torznab: pages per search (TPB answers one page of at most 100 results).
    max_pages: Option<u32>,
    /// Torznab: results requested per page (clamped to the endpoint's caps limit).
    page_size: Option<u32>,
    /// Queries allowed per `limits_unit` (Prowlarr `baseSettings.queryLimit` semantics).
    query_limit: Option<u32>,
    /// Grabs allowed per `limits_unit` (Prowlarr `baseSettings.grabLimit` semantics).
    grab_limit: Option<u32>,
    /// `day` (default) or `hour`.
    limits_unit: Option<String>,
}

fn d_indexer_timeout() -> u64 {
    30
}
fn d_indexer_concurrency() -> usize {
    1
}
fn d_min_interval() -> u64 {
    2000
}
fn d_indexer_response() -> usize {
    4 * 1024 * 1024
}

/// Remote MCP (Streamable HTTP) in the `serve` process, at `/mcp`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpSection {
    #[serde(default)]
    enabled: bool,
    /// Owner-only file holding the MCP bearer token (different from the API token).
    token_file: Option<String>,
    /// Accepted `Host` header values (`host` or `host:port`). Default for a loopback
    /// listener: localhost, 127.0.0.1 and ::1 on any port. Required otherwise.
    allowed_hosts: Option<Vec<String>>,
    /// Browser origins allowed to call (`scheme://host[:port]`); a request with any other
    /// `Origin` header is refused. Default: none.
    #[serde(default)]
    allowed_origins: Vec<String>,
    #[serde(default = "d_mcp_body")]
    max_body_bytes: usize,
    #[serde(default = "d_mcp_concurrency")]
    max_concurrent: usize,
    #[serde(default = "d_mcp_timeout")]
    timeout_secs: u64,
    /// OAuth 2.1 sign-in (Google) for connectors such as ChatGPT. Absent: static token only.
    oauth: Option<OAuthSection>,
}

/// `[mcp.oauth]`: Google as the identity provider for the remote MCP endpoint.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OAuthSection {
    public_url: String,
    google_client_id: String,
    google_client_secret_file: String,
    allowed_emails: Vec<String>,
    trusted_client_metadata: Option<Vec<String>>,
    google_authorization_endpoint: Option<String>,
    google_token_endpoint: Option<String>,
    access_token_ttl_secs: Option<u64>,
    refresh_token_ttl_days: Option<u64>,
}

fn d_mcp_body() -> usize {
    64 * 1024
}
fn d_mcp_concurrency() -> usize {
    4
}
fn d_mcp_timeout() -> u64 {
    120
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct MigrationSection {
    /// `coexistence` (the only mode). Still required explicitly when execution is enabled, and
    /// still written by the installers and the wizard: the previous release refuses a config
    /// with downloading on and no `[migration] mode`, so a rolled-back binary needs the line.
    /// `legacy` (Sonarr/Radarr import) is refused.
    mode: Option<String>,
}

/// The migration mode. Only `coexistence` remains: it is the monitored-only acquisition
/// gate, which is now unconditional. The type is kept so `[migration]` stays parseable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationMode {
    Coexistence,
}

impl MigrationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            MigrationMode::Coexistence => "coexistence",
        }
    }
}

fn default_listen() -> String {
    "127.0.0.1:8787".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerSection {
    #[serde(default = "default_listen")]
    listen: String,
    #[serde(default)]
    allow_non_loopback: bool,
    api_token_file: String,
    #[serde(default = "d_body")]
    max_body_bytes: usize,
    #[serde(default = "d_req_timeout")]
    request_timeout_secs: u64,
    #[serde(default = "d_concurrency")]
    max_concurrent_requests: usize,
}

fn d_body() -> usize {
    64 * 1024
}
fn d_req_timeout() -> u64 {
    180
}
fn d_concurrency() -> usize {
    16
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct StateSection {
    dir: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProwlarrSection {
    base_url: String,
    api_key_file: String,
    #[serde(default = "d_prowlarr_timeout")]
    timeout_secs: u64,
    #[serde(default = "d_page_size")]
    page_size: u32,
    #[serde(default = "d_max_pages")]
    max_pages: u32,
    #[serde(default = "d_prowlarr_concurrency")]
    max_concurrent: usize,
    #[serde(default = "d_cache_ttl")]
    cache_ttl_secs: u64,
    #[serde(default = "d_max_response")]
    max_response_bytes: usize,
}

fn d_prowlarr_timeout() -> u64 {
    60
}
fn d_page_size() -> u32 {
    100
}
fn d_max_pages() -> u32 {
    2
}
fn d_prowlarr_concurrency() -> usize {
    2
}
fn d_cache_ttl() -> u64 {
    900
}
fn d_max_response() -> usize {
    16 * 1024 * 1024
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TvmazeSection {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "d_tvmaze_url")]
    base_url: String,
    /// Where TVmaze serves its pictures (posters are fetched only from here).
    #[serde(default = "d_tvmaze_image_url")]
    image_base_url: String,
    #[serde(default = "d_meta_timeout")]
    timeout_secs: u64,
}

impl Default for TvmazeSection {
    fn default() -> Self {
        TvmazeSection {
            enabled: false,
            base_url: d_tvmaze_url(),
            image_base_url: d_tvmaze_image_url(),
            timeout_secs: d_meta_timeout(),
        }
    }
}

fn d_tvmaze_url() -> String {
    "https://api.tvmaze.com".into()
}
fn d_tvmaze_image_url() -> String {
    "https://static.tvmaze.com".into()
}
fn d_meta_timeout() -> u64 {
    15
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TmdbSection {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "d_tmdb_url")]
    base_url: String,
    /// Where TMDB serves its pictures (posters are fetched only from here).
    #[serde(default = "d_tmdb_image_url")]
    image_base_url: String,
    credential_file: Option<String>,
    #[serde(default = "d_meta_timeout")]
    timeout_secs: u64,
}

impl Default for TmdbSection {
    fn default() -> Self {
        TmdbSection {
            enabled: false,
            base_url: d_tmdb_url(),
            image_base_url: d_tmdb_image_url(),
            credential_file: None,
            timeout_secs: d_meta_timeout(),
        }
    }
}

fn d_tmdb_url() -> String {
    "https://api.themoviedb.org".into()
}
fn d_tmdb_image_url() -> String {
    "https://image.tmdb.org".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionSection {
    /// `download_submission` is its earlier name.
    #[serde(default, alias = "download_submission")]
    enabled: bool,
    /// Follow the imported `removeCompletedDownloads`/`removeFailedDownloads` policy (true) or
    /// always keep downloads in the client (false).
    #[serde(default = "d_true")]
    download_cleanup: bool,
}

impl Default for ExecutionSection {
    fn default() -> Self {
        ExecutionSection {
            enabled: false,
            download_cleanup: true,
        }
    }
}

fn d_true() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QbitSection {
    base_url: String,
    username_file: Option<String>,
    password_file: Option<String>,
    save_path: String,
    local_path: String,
    movie_category: Option<String>,
    tv_category: Option<String>,
    #[serde(default = "d_qbit_timeout")]
    timeout_secs: u64,
}

fn d_qbit_timeout() -> u64 {
    20
}

/// The embedded BitTorrent engine (librqbit), an alternative to `[qbittorrent]`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbeddedSection {
    download_dir: String,
    listen: String,
    #[serde(default = "yes")]
    trackers: bool,
    #[serde(default)]
    dht: bool,
    #[serde(default)]
    local_discovery: bool,
    #[serde(default)]
    initial_peers: Vec<String>,
    peer_limit: Option<u32>,
    upload_limit_bytes_per_sec: Option<u32>,
    download_limit_bytes_per_sec: Option<u32>,
    #[serde(default = "d_max_active")]
    max_active_downloads: u32,
    ratio_limit: Option<f64>,
    seeding_time_limit_minutes: Option<i64>,
    #[serde(default = "d_metadata_timeout")]
    metadata_timeout_secs: u64,
    movie_category: Option<String>,
    tv_category: Option<String>,
}

fn yes() -> bool {
    true
}

fn d_max_active() -> u32 {
    3
}

fn d_metadata_timeout() -> u64 {
    60
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LibrarySection {
    movies: Option<String>,
    tv: Option<String>,
    /// Native root folder path the `movies`/`tv` directory corresponds to (default: the only
    /// imported root folder of that kind).
    movies_native_root: Option<String>,
    tv_native_root: Option<String>,
    #[serde(default = "d_ffprobe")]
    ffprobe: String,
    #[serde(default = "d_probe_timeout")]
    probe_timeout_secs: u64,
    /// Native sample detection by duration against the item runtime.
    #[serde(default = "d_true")]
    sample_detection: bool,
    #[serde(default = "d_probes_per_scan")]
    probes_per_scan: u32,
}

fn d_probes_per_scan() -> u32 {
    20
}

/// Debian's ffprobe; on Windows, the first `ffprobe.exe` on the PATH.
fn d_ffprobe() -> String {
    if cfg!(windows) {
        return std::env::var_os("PATH")
            .and_then(|p| {
                std::env::split_paths(&p)
                    .map(|d| d.join("ffprobe.exe"))
                    .find(|f| f.is_absolute() && f.is_file())
            })
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "ffprobe.exe".into());
    }
    "/usr/bin/ffprobe".into()
}
fn d_probe_timeout() -> u64 {
    600
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AutomationSection {
    /// RSS sync interval override (default: the imported `rssSyncInterval`; 0 disables).
    rss_interval_minutes: Option<u32>,
    /// Explicit backlog searches and request retries run per worker pass at most.
    #[serde(default = "d_searches_per_run")]
    searches_per_run: u32,
    /// First retry delay of an explicit request that found nothing (doubles, up to a day).
    #[serde(default = "d_request_retry")]
    request_retry_minutes: u64,
    /// Explicit requests stop being retried after this many days.
    #[serde(default = "d_request_expiry")]
    request_expiry_days: u64,
    #[serde(default = "d_metadata_hours")]
    metadata_refresh_hours: u64,
    #[serde(default = "d_rescan_hours")]
    rescan_hours: u64,
    #[serde(default = "d_poll")]
    poll_seconds: u64,
    #[serde(default = "d_stall")]
    stall_timeout_minutes: u64,
}

impl Default for AutomationSection {
    fn default() -> Self {
        AutomationSection {
            rss_interval_minutes: None,
            searches_per_run: d_searches_per_run(),
            request_retry_minutes: d_request_retry(),
            request_expiry_days: d_request_expiry(),
            metadata_refresh_hours: d_metadata_hours(),
            rescan_hours: d_rescan_hours(),
            poll_seconds: d_poll(),
            stall_timeout_minutes: d_stall(),
        }
    }
}

fn d_request_retry() -> u64 {
    60
}
fn d_request_expiry() -> u64 {
    14
}
fn d_searches_per_run() -> u32 {
    5
}
fn d_metadata_hours() -> u64 {
    12
}
fn d_rescan_hours() -> u64 {
    6
}
fn d_poll() -> u64 {
    30
}
fn d_stall() -> u64 {
    720
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlexSection {
    base_url: String,
    token_file: String,
    movie_section: Option<u32>,
    tv_section: Option<u32>,
    /// The `[library] movies` root as Plex sees it (path mapping; enables folder refreshes).
    movies_path: Option<String>,
    /// The `[library] tv` root as Plex sees it.
    tv_path: Option<String>,
    #[serde(default = "d_meta_timeout")]
    timeout_secs: u64,
    /// Where Plex's movie search lives (tests only; installers never write this).
    #[serde(default = "d_plex_discover")]
    discover_url: String,
    /// Where Plex's movie details live (tests only).
    #[serde(default = "d_plex_metadata")]
    metadata_url: String,
    /// Where Plex's posters live (tests only).
    #[serde(default = "d_plex_images")]
    image_base_url: String,
}

fn d_plex_discover() -> String {
    "https://discover.provider.plex.tv".into()
}
fn d_plex_metadata() -> String {
    "https://metadata.provider.plex.tv".into()
}
fn d_plex_images() -> String {
    "https://metadata-static.plex.tv".into()
}

/// qBittorrent connection and the download directory as both sides see it.
#[derive(Debug, Clone)]
pub struct QbitConfig {
    pub base_url: Url,
    pub username: Option<Secret>,
    pub password: Option<Secret>,
    /// Directory torrents are saved to, as qBittorrent sees it (absolute, no trailing `/`).
    pub save_path: String,
    /// The same directory as this server sees it.
    pub local_path: PathBuf,
    /// Override of the imported client categories.
    pub movie_category: Option<String>,
    pub tv_category: Option<String>,
    pub timeout: Duration,
}

/// The embedded engine: one librqbit session in this process, with its own download
/// directory and private session state. No qBittorrent API, URL or credential is involved.
#[derive(Debug, Clone)]
pub struct EmbeddedConfig {
    /// Where torrents are written (absolute); also how the "client" reports its save path.
    pub download_dir: PathBuf,
    pub save_path: String,
    /// Engine persistence (session JSON, fast-resume bitfields, DHT state), under the state dir.
    pub session_dir: PathBuf,
    pub listen: SocketAddr,
    pub trackers: bool,
    pub dht: bool,
    pub local_discovery: bool,
    /// Peers every torrent starts with (controlled tests and private swarms).
    pub initial_peers: Vec<SocketAddr>,
    pub peer_limit: Option<usize>,
    pub upload_bps: Option<std::num::NonZeroU32>,
    pub download_bps: Option<std::num::NonZeroU32>,
    /// Torrents downloading at once; further grabs wait in the outbox.
    pub max_active_downloads: usize,
    /// Share limits for torrents without an indexer seed goal (qBittorrent's global limits).
    pub ratio_limit: Option<f64>,
    pub seeding_time_limit_minutes: Option<i64>,
    pub metadata_timeout: Duration,
    pub movie_category: Option<String>,
    pub tv_category: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LibraryConfig {
    /// Local path of the imported movie root.
    pub movies: Option<PathBuf>,
    /// Local path of the imported TV root.
    pub tv: Option<PathBuf>,
    pub movies_native_root: Option<String>,
    pub tv_native_root: Option<String>,
    pub ffprobe: PathBuf,
    pub probe_timeout: Duration,
    pub sample_detection: bool,
    pub probes_per_scan: u32,
}

#[derive(Debug, Clone)]
pub struct AutomationConfig {
    /// `None` = the imported native interval.
    pub rss_interval_minutes: Option<u32>,
    pub searches_per_run: u32,
    pub request_retry: Duration,
    pub request_expiry: Duration,
    pub metadata_refresh: Duration,
    pub rescan: Duration,
    pub poll: Duration,
    pub stall_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct PlexConfig {
    pub base_url: Url,
    pub token: Secret,
    pub movie_section: Option<u32>,
    pub tv_section: Option<u32>,
    /// Plex-side paths of the library roots (absolute, no trailing `/`).
    pub movies_path: Option<String>,
    pub tv_path: Option<String>,
    pub timeout: Duration,
    /// Plex's own movie metadata service, read with the same token.
    pub discover_url: Url,
    pub metadata_url: Url,
    pub image_base_url: Url,
}

/// What a native indexer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexerKind {
    /// The Pirate Bay's JSON API (`q.php`, `precompiled/data_top100_recent.json`).
    ThePirateBay,
    /// Knaben's JSON search API (`POST v1`), an aggregate of public trackers.
    Knaben,
    /// A Torznab endpoint (caps, search, RSS; spec 1.3).
    Torznab,
}

impl IndexerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            IndexerKind::ThePirateBay => "thepiratebay",
            IndexerKind::Knaben => "knaben",
            IndexerKind::Torznab => "torznab",
        }
    }

    /// Providers whose releases are magnets built from an info-hash (no download link).
    pub fn builds_magnets(self) -> bool {
        matches!(self, IndexerKind::ThePirateBay | IndexerKind::Knaben)
    }
}

/// A native indexer connection. Policy (categories, priority, seeders, seed goals, RSS)
/// stays in the settings, under the same id. `Debug` shows origins only.
#[derive(Clone)]
pub struct IndexerConfig {
    pub id: u32,
    pub name: String,
    pub kind: IndexerKind,
    pub base_url: Url,
    pub api_key: Option<Secret>,
    pub api_path: String,
    /// Origins (scheme, host, port) `.torrent` links may be fetched from besides `base_url`.
    pub download_origins: Vec<Url>,
    /// Origins besides `base_url`'s that may receive the API key in tracker URLs.
    pub key_origins: Vec<Url>,
    pub prowlarr_definition: Option<String>,
    pub magnet_trackers: Vec<String>,
    pub uploaders: Vec<String>,
    pub recent_feed: bool,
    pub timeout: Duration,
    pub max_concurrent: usize,
    pub min_interval: Duration,
    pub cache_ttl: Duration,
    pub max_response_bytes: usize,
    pub max_pages: u32,
    pub page_size: u32,
    pub query_limit: Option<u32>,
    pub grab_limit: Option<u32>,
    pub limits_window: Duration,
}

impl fmt::Debug for IndexerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let origin = |u: &Url| u.origin().ascii_serialization();
        f.debug_struct("IndexerConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("origin", &origin(&self.base_url))
            .field("api_key", &self.api_key)
            .field(
                "download_origins",
                &self.download_origins.iter().map(origin).collect::<Vec<_>>(),
            )
            .field(
                "key_origins",
                &self.key_origins.iter().map(origin).collect::<Vec<_>>(),
            )
            .field(
                "magnet_trackers",
                &format_args!("<{} redacted>", self.magnet_trackers.len()),
            )
            .field("uploaders", &self.uploaders.len())
            .field("recent_feed", &self.recent_feed)
            .finish_non_exhaustive()
    }
}

/// Remote MCP endpoint settings.
#[derive(Debug, Clone)]
pub struct McpConfig {
    pub token: Secret,
    pub allowed_hosts: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub max_body_bytes: usize,
    pub max_concurrent: usize,
    pub timeout: Duration,
    pub oauth: Option<OAuthConfig>,
}

/// Validated `[mcp.oauth]`.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    /// The public origin without a trailing slash (`https://host[:port]`).
    pub public_url: String,
    pub google_client_id: String,
    pub google_client_secret: Secret,
    /// Lowercase exact addresses.
    pub allowed_emails: Vec<String>,
    /// URL prefixes (ending in `/`) under which client metadata documents are trusted.
    pub trusted_client_metadata: Vec<String>,
    pub google_authorization_endpoint: Url,
    pub google_token_endpoint: Url,
    pub access_token_ttl: Duration,
    pub refresh_token_ttl: Duration,
}

/// An https URL (http only for loopback hosts, for tests) without credentials or fragment.
fn oauth_url(raw: &str, what: &str) -> Result<Url> {
    let url = Url::parse(raw.trim()).with_context(|| format!("{what}: invalid URL"))?;
    let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|a| a.is_loopback());
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback)) {
        bail!("{what}: must be an https URL (http only for loopback hosts)");
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        bail!("{what}: must not contain credentials or a fragment");
    }
    Ok(url)
}

fn oauth_config(
    o: OAuthSection,
    allowed_hosts: &[String],
    read_secret: &dyn Fn(&str, &str) -> Result<String>,
) -> Result<OAuthConfig> {
    let public = oauth_url(&o.public_url, "mcp.oauth.public_url")?;
    if public.path() != "/" || public.query().is_some() {
        bail!("mcp.oauth.public_url must be an origin only (no path or query)");
    }
    let authority = match public.port() {
        Some(p) => format!("{}:{p}", public.host_str().unwrap_or_default()),
        None => public.host_str().unwrap_or_default().to_string(),
    };
    if !crate::mcp::host_allowed(&authority, allowed_hosts) {
        bail!("mcp.oauth.public_url host must be listed in mcp.allowed_hosts");
    }
    if o.google_client_id.trim().is_empty() {
        bail!("mcp.oauth.google_client_id must not be empty");
    }
    let secret = read_secret(
        &o.google_client_secret_file,
        "mcp.oauth.google_client_secret_file",
    )?;
    if secret.is_empty() {
        bail!("mcp.oauth.google_client_secret_file must not be empty");
    }
    if o.allowed_emails.is_empty() || o.allowed_emails.len() > 32 {
        bail!("mcp.oauth.allowed_emails must list 1 to 32 addresses");
    }
    let allowed_emails = o
        .allowed_emails
        .iter()
        .map(|e| {
            let e = e.trim().to_ascii_lowercase();
            let ok = e.matches('@').count() == 1
                && !e.starts_with('@')
                && !e.ends_with('@')
                && !e.contains(|c: char| c == '*' || c.is_whitespace() || c.is_control());
            if !ok {
                bail!("mcp.oauth.allowed_emails entries must be exact addresses (no wildcards)");
            }
            Ok(e)
        })
        .collect::<Result<Vec<_>>>()?;
    let prefixes = o
        .trusted_client_metadata
        .unwrap_or_else(|| vec!["https://chatgpt.com/oauth/".into()]);
    if prefixes.is_empty() || prefixes.len() > 16 {
        bail!("mcp.oauth.trusted_client_metadata must list 1 to 16 URL prefixes");
    }
    for p in &prefixes {
        let u = oauth_url(p, "mcp.oauth.trusted_client_metadata")?;
        if u.as_str() != p || !p.ends_with('/') || u.query().is_some() {
            bail!(
                "mcp.oauth.trusted_client_metadata entries must be normalized URL prefixes ending in '/' (got {p:?})"
            );
        }
    }
    let endpoint = |v: Option<String>, default: &str, what: &str| {
        oauth_url(v.as_deref().unwrap_or(default), what)
    };
    Ok(OAuthConfig {
        public_url: public.origin().ascii_serialization(),
        google_client_id: o.google_client_id.trim().to_string(),
        google_client_secret: Secret(secret),
        allowed_emails,
        trusted_client_metadata: prefixes,
        google_authorization_endpoint: endpoint(
            o.google_authorization_endpoint,
            "https://accounts.google.com/o/oauth2/v2/auth",
            "mcp.oauth.google_authorization_endpoint",
        )?,
        google_token_endpoint: endpoint(
            o.google_token_endpoint,
            "https://oauth2.googleapis.com/token",
            "mcp.oauth.google_token_endpoint",
        )?,
        access_token_ttl: Duration::from_secs(within(
            "mcp.oauth.access_token_ttl_secs",
            o.access_token_ttl_secs.unwrap_or(3600),
            60,
            86_400,
        )?),
        refresh_token_ttl: Duration::from_secs(
            86_400
                * within(
                    "mcp.oauth.refresh_token_ttl_days",
                    o.refresh_token_ttl_days.unwrap_or(30),
                    1,
                    365,
                )?,
        ),
    })
}

fn absolute_path(raw: &str, what: &str) -> Result<PathBuf> {
    let p = Path::new(raw.trim());
    let bad = !p.is_absolute()
        || p.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        });
    if bad {
        bail!("{what} must be an absolute path without '.' or '..' components");
    }
    Ok(p.to_path_buf())
}

/// A folder as another program (qBittorrent, Plex, perhaps in Docker) sees it: absolute in
/// Unix or Windows form, whatever this computer runs, without '.' or '..'. Trailing
/// separators are dropped ("/" stays "", for the caller to decide).
fn remote_path(raw: &str, what: &str) -> Result<String> {
    let s = raw.trim();
    let b = s.as_bytes();
    let unix = s.starts_with('/');
    let windows = (b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && matches!(b[2], b'/' | b'\\'))
        || s.starts_with("\\\\");
    if !(unix || windows) || s.split(['/', '\\']).any(|c| c == "." || c == "..") {
        bail!("{what} must be an absolute path without '.' or '..' components");
    }
    Ok(s.trim_end_matches(['/', '\\']).to_string())
}

fn category(raw: Option<String>, what: &str) -> Result<Option<String>> {
    match raw {
        None => Ok(None),
        Some(c) => {
            let c = c.trim().to_string();
            if c.is_empty()
                || c.len() > 64
                || c.chars()
                    .any(|ch| ch.is_control() || matches!(ch, '/' | '\\' | ','))
            {
                bail!("{what} must be 1-64 printable characters without '/', '\\' or ','");
            }
            Ok(Some(c))
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProwlarrConfig {
    pub base_url: Url,
    pub api_key: Secret,
    pub timeout: Duration,
    pub page_size: u32,
    pub max_pages: u32,
    pub max_concurrent: usize,
    pub cache_ttl: Duration,
    pub max_response_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct MetadataConfig {
    pub base_url: Url,
    /// The only host posters are fetched from.
    pub image_base_url: Url,
    pub timeout: Duration,
    pub credential: Option<Secret>,
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub listen: SocketAddr,
    pub api_token: Secret,
    pub max_body_bytes: usize,
    pub request_timeout: Duration,
    pub max_concurrent_requests: usize,
    pub state_dir: PathBuf,
    pub prowlarr: Option<ProwlarrConfig>,
    pub tvmaze: Option<MetadataConfig>,
    pub tmdb: Option<MetadataConfig>,
    /// Master switch for writes outside this server's state: torrent submission, file import,
    /// Plex refresh and scheduled acquisition searches.
    pub execution_enabled: bool,
    /// Follow the imported client cleanup policy (`[execution] download_cleanup`).
    pub download_cleanup: bool,
    pub migration: MigrationMode,
    pub qbittorrent: Option<QbitConfig>,
    pub embedded: Option<EmbeddedConfig>,
    pub library: Option<LibraryConfig>,
    pub automation: AutomationConfig,
    pub plex: Option<PlexConfig>,
    /// Native indexers (exclusive with `prowlarr`).
    pub indexers: Vec<IndexerConfig>,
    pub mcp: Option<McpConfig>,
    /// Send scrubbed error reports (`[reporting] enabled`, docs/PRIVACY.md).
    pub reporting: bool,
    /// Tests only: the id of the in-memory download client that replaces the embedded engine.
    #[cfg(feature = "test-client")]
    pub test_client: Option<String>,
}

impl RuntimeConfig {
    /// The configured release search backend: `native` (`[[indexer]]`) or `prowlarr`.
    pub fn search_backend(&self) -> Option<&'static str> {
        if !self.indexers.is_empty() {
            Some("native")
        } else if self.prowlarr.is_some() {
            Some("prowlarr")
        } else {
            None
        }
    }

    /// The download directory as the download client reports it, and the same directory as
    /// this server sees it (identical for the embedded engine).
    pub fn client_paths(&self) -> Option<(&str, &Path)> {
        match (&self.qbittorrent, &self.embedded) {
            (Some(q), _) => Some((q.save_path.as_str(), q.local_path.as_path())),
            (None, Some(e)) => Some((e.save_path.as_str(), e.download_dir.as_path())),
            (None, None) => None,
        }
    }

    /// The configured download backend: `qbittorrent` or `embedded`.
    pub fn download_backend(&self) -> Option<&'static str> {
        match (&self.qbittorrent, &self.embedded) {
            (Some(_), _) => Some("qbittorrent"),
            (None, Some(_)) => Some("embedded"),
            (None, None) => None,
        }
    }
}

/// `s` as a TOML string, quoted and escaped.
pub fn quote(s: &str) -> String {
    toml::Value::String(s.into()).to_string()
}

/// Validates an operator-supplied upstream base URL: http(s), host, no credentials,
/// no query/fragment. Returned with a trailing slash so `join` keeps any path prefix.
pub fn parse_base_url(raw: &str, what: &str) -> Result<Url> {
    let mut url = Url::parse(raw.trim()).with_context(|| format!("{what}: invalid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("{what}: scheme must be http or https");
    }
    if url.host_str().is_none_or(str::is_empty) {
        bail!("{what}: URL must include a host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("{what}: credentials must not be embedded in the URL; use the key file");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("{what}: URL must not contain a query or fragment");
    }
    if !url.path().ends_with('/') {
        let p = format!("{}/", url.path());
        url.set_path(&p);
    }
    Ok(url)
}

fn within(name: &str, v: u64, lo: u64, hi: u64) -> Result<u64> {
    if v < lo || v > hi {
        bail!("{name} must be between {lo} and {hi} (got {v})");
    }
    Ok(v)
}

fn resolve(base: &Path, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// How `parse_config` treats secret files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Secrets {
    /// Read every referenced secret file (serving and `check`).
    Read,
    /// Validate the configuration before its secret files exist (`setup`): a missing or
    /// unusable secret file is listed instead of failing, and its value is a placeholder.
    /// The result must never be used to serve.
    Defer,
}

const DEFERRED_SECRET: &str = "deferred-secret-placeholder-never-used";

pub fn load(path: &Path) -> Result<RuntimeConfig> {
    fsutil::check_private_file(path)?;
    let text = String::from_utf8(fsutil::read_bounded(path, 256 * 1024)?)
        .context("config is not UTF-8")?;
    let base = path
        .parent()
        .map(|p| {
            if p.as_os_str().is_empty() {
                Path::new(".")
            } else {
                p
            }
        })
        .unwrap_or(Path::new("."))
        .to_path_buf();
    parse_config(&text, &base, &path.display().to_string(), Secrets::Read).map(|(c, _)| c)
}

/// Validates configuration text whose relative paths resolve against `base`. Returns the
/// runtime configuration and, with `Secrets::Defer`, the secret files still missing.
pub fn parse_config(
    text: &str,
    base: &Path,
    name: &str,
    secrets: Secrets,
) -> Result<(RuntimeConfig, Vec<String>)> {
    let fc: FileConfig = toml::from_str(text).with_context(|| format!("invalid config {name}"))?;
    let base = base.to_path_buf();
    let pending = std::cell::RefCell::new(Vec::<String>::new());
    let read_secret = |file: &str, what: &str| -> Result<String> {
        let path = resolve(&base, file);
        match (fsutil::read_secret_file(&path), secrets) {
            (Ok(v), _) => Ok(v),
            (Err(e), Secrets::Read) => Err(e.context(what.to_string())),
            (Err(e), Secrets::Defer) => {
                pending
                    .borrow_mut()
                    .push(format!("{what} ({}): {e:#}", path.display()));
                Ok(DEFERRED_SECRET.into())
            }
        }
    };
    let state_dir = fc
        .state
        .dir
        .as_deref()
        .map(|d| resolve(&base, d))
        .unwrap_or_else(|| base.clone());
    fsutil::check_private_dir(&state_dir).context("state directory")?;

    let listen: SocketAddr = fc
        .server
        .listen
        .parse()
        .context("server.listen must be ip:port")?;
    if !listen.ip().is_loopback() && !fc.server.allow_non_loopback {
        bail!(
            "server.listen {listen} is not loopback; set server.allow_non_loopback = true to expose the API deliberately"
        );
    }
    let api_token = read_secret(&fc.server.api_token_file, "server.api_token_file")?;
    if api_token.len() < 24 {
        bail!("server.api_token_file must contain at least 24 characters");
    }
    let s = &fc.server;
    let max_body_bytes = within(
        "server.max_body_bytes",
        s.max_body_bytes as u64,
        1024,
        1024 * 1024,
    )? as usize;
    let request_timeout = Duration::from_secs(within(
        "server.request_timeout_secs",
        s.request_timeout_secs,
        1,
        900,
    )?);
    let max_concurrent_requests = within(
        "server.max_concurrent_requests",
        s.max_concurrent_requests as u64,
        1,
        1024,
    )? as usize;

    let prowlarr = match fc.prowlarr {
        None => None,
        Some(p) => Some(ProwlarrConfig {
            base_url: parse_base_url(&p.base_url, "prowlarr.base_url")?,
            api_key: Secret(read_secret(&p.api_key_file, "prowlarr.api_key_file")?),
            timeout: Duration::from_secs(within("prowlarr.timeout_secs", p.timeout_secs, 1, 300)?),
            page_size: within("prowlarr.page_size", u64::from(p.page_size), 1, 1000)? as u32,
            max_pages: within("prowlarr.max_pages", u64::from(p.max_pages), 1, 10)? as u32,
            max_concurrent: within("prowlarr.max_concurrent", p.max_concurrent as u64, 1, 16)?
                as usize,
            cache_ttl: Duration::from_secs(within(
                "prowlarr.cache_ttl_secs",
                p.cache_ttl_secs,
                0,
                86_400,
            )?),
            max_response_bytes: within(
                "prowlarr.max_response_bytes",
                p.max_response_bytes as u64,
                1024,
                64 * 1024 * 1024,
            )? as usize,
        }),
    };
    let tvmaze = if fc.tvmaze.enabled {
        Some(MetadataConfig {
            base_url: parse_base_url(&fc.tvmaze.base_url, "tvmaze.base_url")?,
            image_base_url: parse_base_url(&fc.tvmaze.image_base_url, "tvmaze.image_base_url")?,
            timeout: Duration::from_secs(within(
                "tvmaze.timeout_secs",
                fc.tvmaze.timeout_secs,
                1,
                120,
            )?),
            credential: None,
        })
    } else {
        None
    };
    let tmdb = if fc.tmdb.enabled {
        let file = fc
            .tmdb
            .credential_file
            .as_deref()
            .context("tmdb.enabled requires tmdb.credential_file (a TMDB v4 read access token or v3 API key)")?;
        Some(MetadataConfig {
            base_url: parse_base_url(&fc.tmdb.base_url, "tmdb.base_url")?,
            image_base_url: parse_base_url(&fc.tmdb.image_base_url, "tmdb.image_base_url")?,
            timeout: Duration::from_secs(within(
                "tmdb.timeout_secs",
                fc.tmdb.timeout_secs,
                1,
                120,
            )?),
            credential: Some(Secret(read_secret(file, "tmdb.credential_file")?)),
        })
    } else {
        None
    };
    let qbittorrent = match fc.qbittorrent {
        None => None,
        Some(q) => {
            let secret = |f: &Option<String>, what: &str| -> Result<Option<Secret>> {
                f.as_deref()
                    .map(|f| read_secret(f, what).map(Secret))
                    .transpose()
            };
            let username = secret(&q.username_file, "qbittorrent.username_file")?;
            let password = secret(&q.password_file, "qbittorrent.password_file")?;
            if username.is_some() != password.is_some() {
                bail!(
                    "qbittorrent.username_file and qbittorrent.password_file must be set together (or both omitted when qBittorrent bypasses authentication for this client)"
                );
            }
            let save_path = remote_path(&q.save_path, "qbittorrent.save_path")?;
            // Download-client paths are compared in Unix form; qBittorrent on Windows isn't
            // supported (on Windows, use the built-in downloader: [embedded]).
            if !q.save_path.trim().starts_with('/') {
                bail!(
                    "qbittorrent.save_path must be the folder as qBittorrent sees it, in Unix form such as /downloads (qBittorrent running on Windows isn't supported; use [embedded])"
                );
            }
            Some(QbitConfig {
                base_url: parse_base_url(&q.base_url, "qbittorrent.base_url")?,
                username,
                password,
                save_path: if save_path.is_empty() {
                    "/".into()
                } else {
                    save_path
                },
                local_path: absolute_path(&q.local_path, "qbittorrent.local_path")?,
                movie_category: category(q.movie_category, "qbittorrent.movie_category")?,
                tv_category: category(q.tv_category, "qbittorrent.tv_category")?,
                timeout: Duration::from_secs(within(
                    "qbittorrent.timeout_secs",
                    q.timeout_secs,
                    1,
                    120,
                )?),
            })
        }
    };
    if qbittorrent.is_some() && fc.embedded.is_some() {
        bail!(
            "configure either [qbittorrent] or [embedded], not both: the download backend is chosen explicitly and never falls back to the other"
        );
    }
    let embedded = match fc.embedded {
        None => None,
        Some(e) => {
            let download_dir = absolute_path(&e.download_dir, "embedded.download_dir")?;
            let save_path = crate::paths::client_form(&download_dir);
            if save_path.is_empty() || save_path == "/" || paths_overlap(&download_dir, &state_dir)
            {
                bail!(
                    "embedded.download_dir must be a dedicated directory, not / and neither inside nor around the state directory"
                );
            }
            let listen: SocketAddr = e.listen.trim().parse().with_context(|| {
                format!(
                    "embedded.listen must be an IP:port socket address (got {:?})",
                    e.listen
                )
            })?;
            if e.initial_peers.len() > 64 {
                bail!("embedded.initial_peers allows at most 64 addresses");
            }
            let initial_peers = e
                .initial_peers
                .iter()
                .map(|p| {
                    p.trim().parse::<SocketAddr>().with_context(|| {
                        format!("embedded.initial_peers entries must be IP:port (got {p:?})")
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if e.ratio_limit.is_some_and(|r| !(0.0..=1000.0).contains(&r)) {
                bail!("embedded.ratio_limit must be between 0 and 1000");
            }
            if e.seeding_time_limit_minutes
                .is_some_and(|m| !(0..=5_256_000).contains(&m))
            {
                bail!("embedded.seeding_time_limit_minutes must be between 0 and 5256000");
            }
            let nonzero = |v: Option<u32>, what: &str| -> Result<Option<std::num::NonZeroU32>> {
                match v {
                    None => Ok(None),
                    Some(v) => std::num::NonZeroU32::new(v)
                        .map(Some)
                        .with_context(|| format!("{what} must be positive (omit it for no limit)")),
                }
            };
            Some(EmbeddedConfig {
                download_dir,
                save_path,
                session_dir: state_dir.join("embedded"),
                listen,
                trackers: e.trackers,
                dht: e.dht,
                local_discovery: e.local_discovery,
                initial_peers,
                peer_limit: e
                    .peer_limit
                    .map(|p| within("embedded.peer_limit", u64::from(p), 1, 1000))
                    .transpose()?
                    .map(|p| p as usize),
                upload_bps: nonzero(
                    e.upload_limit_bytes_per_sec,
                    "embedded.upload_limit_bytes_per_sec",
                )?,
                download_bps: nonzero(
                    e.download_limit_bytes_per_sec,
                    "embedded.download_limit_bytes_per_sec",
                )?,
                max_active_downloads: within(
                    "embedded.max_active_downloads",
                    u64::from(e.max_active_downloads),
                    1,
                    100,
                )? as usize,
                ratio_limit: e.ratio_limit,
                seeding_time_limit_minutes: e.seeding_time_limit_minutes,
                metadata_timeout: Duration::from_secs(within(
                    "embedded.metadata_timeout_secs",
                    e.metadata_timeout_secs,
                    5,
                    600,
                )?),
                movie_category: category(e.movie_category, "embedded.movie_category")?,
                tv_category: category(e.tv_category, "embedded.tv_category")?,
            })
        }
    };
    let library = match fc.library {
        None => None,
        Some(l) => Some(LibraryConfig {
            movies: l
                .movies
                .as_deref()
                .map(|p| absolute_path(p, "library.movies"))
                .transpose()?,
            tv: l
                .tv
                .as_deref()
                .map(|p| absolute_path(p, "library.tv"))
                .transpose()?,
            movies_native_root: l
                .movies_native_root
                .as_deref()
                .map(|p| {
                    absolute_path(p, "library.movies_native_root")
                        .map(|p| p.to_string_lossy().trim_end_matches('/').to_string())
                })
                .transpose()?,
            tv_native_root: l
                .tv_native_root
                .as_deref()
                .map(|p| {
                    absolute_path(p, "library.tv_native_root")
                        .map(|p| p.to_string_lossy().trim_end_matches('/').to_string())
                })
                .transpose()?,
            ffprobe: absolute_path(&l.ffprobe, "library.ffprobe")?,
            probe_timeout: Duration::from_secs(within(
                "library.probe_timeout_secs",
                l.probe_timeout_secs,
                5,
                3600,
            )?),
            sample_detection: l.sample_detection,
            probes_per_scan: within(
                "library.probes_per_scan",
                u64::from(l.probes_per_scan),
                0,
                1000,
            )? as u32,
        }),
    };
    if let (Some(q), Some(l)) = (&qbittorrent, &library) {
        for root in l.movies.iter().chain(&l.tv) {
            if root.starts_with(&q.local_path) || q.local_path.starts_with(root) {
                bail!(
                    "qbittorrent.local_path and the library roots must not contain each other ({} vs {})",
                    q.local_path.display(),
                    root.display()
                );
            }
        }
    }
    if let (Some(e), Some(l)) = (&embedded, &library) {
        for root in l.movies.iter().chain(&l.tv) {
            if paths_overlap(root, &e.download_dir) {
                bail!(
                    "embedded.download_dir and the library roots must not contain each other ({} vs {})",
                    e.download_dir.display(),
                    root.display()
                );
            }
        }
    }
    let a = &fc.automation;
    let automation = AutomationConfig {
        rss_interval_minutes: a
            .rss_interval_minutes
            .map(|m| {
                if m == 0 {
                    Ok(0)
                } else {
                    within("automation.rss_interval_minutes", u64::from(m), 10, 1440)
                        .map(|m| m as u32)
                }
            })
            .transpose()?,
        searches_per_run: within(
            "automation.searches_per_run",
            u64::from(a.searches_per_run),
            1,
            100,
        )? as u32,
        request_retry: Duration::from_secs(
            60 * within(
                "automation.request_retry_minutes",
                a.request_retry_minutes,
                5,
                1440,
            )?,
        ),
        request_expiry: Duration::from_secs(
            86_400
                * within(
                    "automation.request_expiry_days",
                    a.request_expiry_days,
                    1,
                    365,
                )?,
        ),
        metadata_refresh: Duration::from_secs(
            3600 * within(
                "automation.metadata_refresh_hours",
                a.metadata_refresh_hours,
                1,
                720,
            )?,
        ),
        rescan: Duration::from_secs(
            3600 * within("automation.rescan_hours", a.rescan_hours, 1, 720)?,
        ),
        poll: Duration::from_secs(within("automation.poll_seconds", a.poll_seconds, 1, 3600)?),
        stall_timeout: Duration::from_secs(
            60 * within(
                "automation.stall_timeout_minutes",
                a.stall_timeout_minutes,
                5,
                100_000,
            )?,
        ),
    };
    let plex = match fc.plex {
        None => None,
        Some(p) => {
            let plex_path = |v: &Option<String>, what: &str| -> Result<Option<String>> {
                v.as_deref()
                    .map(|p| {
                        remote_path(p, what).map(|s| if s.is_empty() { "/".to_string() } else { s })
                    })
                    .transpose()
            };
            for (section, what) in [
                (p.movie_section, "plex.movie_section"),
                (p.tv_section, "plex.tv_section"),
            ] {
                if section == Some(0) {
                    bail!("{what} must be a Plex library section id (a positive number)");
                }
            }
            Some(PlexConfig {
                base_url: parse_base_url(&p.base_url, "plex.base_url")?,
                token: Secret(read_secret(&p.token_file, "plex.token_file")?),
                movie_section: p.movie_section,
                tv_section: p.tv_section,
                movies_path: plex_path(&p.movies_path, "plex.movies_path")?,
                tv_path: plex_path(&p.tv_path, "plex.tv_path")?,
                timeout: Duration::from_secs(within("plex.timeout_secs", p.timeout_secs, 1, 120)?),
                discover_url: parse_base_url(&p.discover_url, "plex.discover_url")?,
                metadata_url: parse_base_url(&p.metadata_url, "plex.metadata_url")?,
                image_base_url: parse_base_url(&p.image_base_url, "plex.image_base_url")?,
            })
        }
    };
    let indexers = fc
        .indexer
        .into_iter()
        .map(|ix| indexer_config(ix, &read_secret))
        .collect::<Result<Vec<_>>>()?;
    {
        let mut ids: Vec<u32> = indexers.iter().map(|i| i.id).collect();
        ids.sort_unstable();
        if let Some(w) = ids.windows(2).find(|w| w[0] == w[1]) {
            bail!("indexer id {} is configured twice", w[0]);
        }
        if indexers.len() > 32 {
            bail!("at most 32 [[indexer]] entries are supported");
        }
    }
    if !indexers.is_empty() && prowlarr.is_some() {
        bail!(
            "configure either [prowlarr] or [[indexer]] entries, not both: the release search backend is chosen explicitly and never falls back to the other"
        );
    }
    let mcp = match fc.mcp {
        Some(m) if m.enabled => {
            let file = m.token_file.as_deref().context(
                "mcp.enabled requires mcp.token_file (an owner-only file with the MCP bearer token)",
            )?;
            let token = read_secret(file, "mcp.token_file")?;
            if token.len() < 24 {
                bail!("mcp.token_file must contain at least 24 characters");
            }
            if token == api_token && token != DEFERRED_SECRET {
                bail!(
                    "mcp.token_file must hold a different token than server.api_token_file: the MCP token only reaches the six MCP tools"
                );
            }
            let allowed_hosts = match m.allowed_hosts {
                Some(h) => {
                    if h.is_empty() || h.len() > 32 {
                        bail!("mcp.allowed_hosts must list 1 to 32 host names");
                    }
                    h.iter().map(|x| mcp_host(x)).collect::<Result<Vec<_>>>()?
                }
                None if listen.ip().is_loopback() => {
                    vec!["localhost".into(), "127.0.0.1".into(), "::1".into()]
                }
                None => bail!(
                    "server.listen {listen} is not loopback: list the exact host names clients use in mcp.allowed_hosts (Host header validation is never disabled)"
                ),
            };
            if m.allowed_origins.len() > 32 {
                bail!("mcp.allowed_origins allows at most 32 entries");
            }
            let allowed_origins = m
                .allowed_origins
                .iter()
                .map(|o| mcp_origin(o))
                .collect::<Result<Vec<_>>>()?;
            let oauth = m
                .oauth
                .map(|o| oauth_config(o, &allowed_hosts, &read_secret))
                .transpose()?;
            Some(McpConfig {
                token: Secret(token),
                allowed_hosts,
                allowed_origins,
                max_body_bytes: within(
                    "mcp.max_body_bytes",
                    m.max_body_bytes as u64,
                    1024,
                    1024 * 1024,
                )? as usize,
                max_concurrent: within("mcp.max_concurrent", m.max_concurrent as u64, 1, 64)?
                    as usize,
                timeout: Duration::from_secs(within("mcp.timeout_secs", m.timeout_secs, 1, 600)?),
                oauth,
            })
        }
        _ => None,
    };
    let execution_enabled = fc.execution.enabled;
    if execution_enabled {
        if qbittorrent.is_none() && embedded.is_none() {
            bail!(
                "execution.enabled requires a [qbittorrent] section (base_url, save_path, local_path and credential files) or an [embedded] section (download_dir and listen)"
            );
        }
        if library
            .as_ref()
            .is_none_or(|l| l.movies.is_none() && l.tv.is_none())
        {
            bail!("execution.enabled requires [library] movies and/or tv local root paths");
        }
        if prowlarr.is_none() && indexers.is_empty() {
            bail!(
                "execution.enabled requires a release search backend: [[indexer]] entries (native) or a [prowlarr] section"
            );
        }
    }
    let migration = match fc.migration.mode.as_deref() {
        None if execution_enabled => bail!(
            "execution.enabled requires an explicit [migration] mode: add `[migration]` with `mode = \"coexistence\"`"
        ),
        None | Some("coexistence") => MigrationMode::Coexistence,
        Some("legacy") => bail!(
            "migration.mode = \"legacy\" was for importing from Sonarr/Radarr, which Arr no longer supports; set mode = \"coexistence\" or remove the line"
        ),
        Some(other) => {
            bail!("migration.mode must be \"coexistence\" (got {other:?})")
        }
    };
    let pending = pending.into_inner();
    Ok((
        RuntimeConfig {
            listen,
            api_token: Secret(api_token),
            max_body_bytes,
            request_timeout,
            max_concurrent_requests,
            state_dir,
            prowlarr,
            tvmaze,
            tmdb,
            execution_enabled,
            download_cleanup: fc.execution.download_cleanup,
            migration,
            qbittorrent,
            embedded,
            library,
            automation,
            plex,
            indexers,
            mcp,
            reporting: fc.reporting.enabled,
            #[cfg(feature = "test-client")]
            test_client: fc.test_client.map(|t| t.id),
        },
        pending,
    ))
}

/// `host` or `host:port` for `mcp.allowed_hosts`.
fn mcp_host(raw: &str) -> Result<String> {
    let h = raw.trim().to_ascii_lowercase();
    let ok = !h.is_empty()
        && h.len() <= 255
        && !h.contains(['/', '@', '?', '#', ' ', '*'])
        && axum::http::uri::Authority::try_from(h.as_str()).is_ok();
    if !ok {
        bail!("mcp.allowed_hosts entries must be host or host:port (got {raw:?})");
    }
    Ok(h)
}

/// `scheme://host[:port]` for `mcp.allowed_origins`.
fn mcp_origin(raw: &str) -> Result<String> {
    let url = Url::parse(raw.trim())
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .filter(|u| u.host_str().is_some())
        .filter(|u| u.username().is_empty() && u.password().is_none())
        .filter(|u| u.query().is_none() && u.fragment().is_none() && u.path() == "/")
        .with_context(|| {
            format!("mcp.allowed_origins entries must be scheme://host[:port] (got {raw:?})")
        })?;
    Ok(url.origin().ascii_serialization())
}

fn indexer_config(
    ix: IndexerSection,
    read_secret: &dyn Fn(&str, &str) -> Result<String>,
) -> Result<IndexerConfig> {
    let what = |k: &str| format!("indexer {} {k}", ix.id);
    if ix.id == 0 || ix.id > i32::MAX as u32 {
        bail!(
            "indexer id must be between 1 and {} (got {})",
            i32::MAX,
            ix.id
        );
    }
    let name = ix.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        bail!("{} must be 1-100 printable characters", what("name"));
    }
    let kind = match ix.kind.as_str() {
        "thepiratebay" => IndexerKind::ThePirateBay,
        "knaben" => IndexerKind::Knaben,
        "torznab" => IndexerKind::Torznab,
        other => bail!(
            "{} {other:?} is not supported natively: supported kinds are \"thepiratebay\", \"knaben\" and \"torznab\" (other Prowlarr/Cardigann definitions are not reproduced; keep them behind a Torznab endpoint)",
            what("kind")
        ),
    };
    let base_url = parse_base_url(&ix.base_url, &what("base_url"))?;
    let only = |set: bool, key: &str, kinds: &[IndexerKind]| -> Result<()> {
        if set && !kinds.contains(&kind) {
            let names: Vec<String> = kinds
                .iter()
                .map(|k| format!("\"{}\"", k.as_str()))
                .collect();
            bail!(
                "{} applies only to kind = {}",
                what(key),
                names.join(" or ")
            );
        }
        Ok(())
    };
    let torznab = [IndexerKind::Torznab];
    let magnets = [IndexerKind::ThePirateBay, IndexerKind::Knaben];
    only(ix.api_key_file.is_some(), "api_key_file", &torznab)?;
    only(ix.api_path.is_some(), "api_path", &torznab)?;
    only(ix.download_origins.is_some(), "download_origins", &torznab)?;
    only(ix.key_origins.is_some(), "key_origins", &torznab)?;
    only(
        ix.prowlarr_definition.is_some(),
        "prowlarr_definition",
        &torznab,
    )?;
    only(ix.max_pages.is_some(), "max_pages", &torznab)?;
    only(ix.page_size.is_some(), "page_size", &torznab)?;
    only(ix.magnet_trackers.is_some(), "magnet_trackers", &magnets)?;
    only(ix.recent_feed.is_some(), "recent_feed", &magnets)?;
    only(
        ix.uploaders.is_some(),
        "uploaders",
        &[IndexerKind::ThePirateBay],
    )?;
    let api_key = ix
        .api_key_file
        .as_deref()
        .map(|f| read_secret(f, &what("api_key_file")).map(Secret))
        .transpose()?;
    let api_path = ix.api_path.unwrap_or_else(|| "api".into());
    let api_path = api_path.trim().trim_matches('/').to_string();
    if api_path.is_empty()
        || api_path.len() > 200
        || api_path
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
        || !api_path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'))
    {
        bail!(
            "{} must be a plain relative path such as api",
            what("api_path")
        );
    }
    let origins = ix.download_origins.unwrap_or_default();
    if origins.len() > 16 {
        bail!("{} allows at most 16 origins", what("download_origins"));
    }
    let download_origins = origins
        .iter()
        .map(|o| {
            let u = parse_base_url(o, &what("download_origins"))?;
            if u.path() != "/" {
                bail!(
                    "{} entries are origins (scheme://host[:port]) without a path",
                    what("download_origins")
                );
            }
            Ok(u)
        })
        .collect::<Result<Vec<_>>>()?;
    let key_origin_list = ix.key_origins.unwrap_or_default();
    if key_origin_list.len() > 16 {
        bail!("{} allows at most 16 origins", what("key_origins"));
    }
    let key_origins = key_origin_list
        .iter()
        .map(|o| {
            let u = parse_base_url(o, &what("key_origins"))?;
            if u.path() != "/" {
                bail!(
                    "{} entries are origins (scheme://host[:port]) without a path",
                    what("key_origins")
                );
            }
            Ok(u)
        })
        .collect::<Result<Vec<_>>>()?;
    let prowlarr_definition = ix
        .prowlarr_definition
        .map(|d| d.trim().to_ascii_lowercase())
        .map(|d| {
            if d.is_empty()
                || d.len() > 100
                || !d
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            {
                bail!(
                    "{} must be a Prowlarr definition name",
                    what("prowlarr_definition")
                );
            }
            Ok(d)
        })
        .transpose()?;
    let trackers = ix.magnet_trackers.unwrap_or_default();
    if trackers.len() > 20 {
        bail!("{} allows at most 20 trackers", what("magnet_trackers"));
    }
    // Public trackers only: no userinfo, query (passkeys travel there) or fragment. The
    // value is never echoed, since a rejected entry may hold a credential.
    for (n, t) in trackers.iter().enumerate() {
        let ok = t.len() <= 512
            && Url::parse(t).is_ok_and(|u| {
                matches!(u.scheme(), "udp" | "http" | "https" | "wss")
                    && u.host_str().is_some_and(|h| !h.is_empty())
                    && u.username().is_empty()
                    && u.password().is_none()
                    && u.query().is_none()
                    && u.fragment().is_none()
            });
        if !ok {
            bail!(
                "{} entry {} must be a public udp://, http(s):// or wss:// tracker URL without credentials, query or fragment (value not shown)",
                what("magnet_trackers"),
                n + 1
            );
        }
    }
    let uploaders = ix.uploaders.unwrap_or_default();
    if uploaders.len() > 100
        || uploaders.iter().any(|u| {
            u.trim().is_empty() || u.chars().count() > 100 || u.chars().any(char::is_control)
        })
    {
        bail!(
            "{} must list at most 100 non-empty uploader names",
            what("uploaders")
        );
    }
    let limits_window = match ix.limits_unit.as_deref() {
        None | Some("day") => Duration::from_secs(86_400),
        Some("hour") => Duration::from_secs(3600),
        Some(other) => bail!(
            "{} must be \"day\" or \"hour\" (got {other:?})",
            what("limits_unit")
        ),
    };
    if ix.limits_unit.is_some() && ix.query_limit.is_none() && ix.grab_limit.is_none() {
        bail!(
            "{} needs query_limit and/or grab_limit",
            what("limits_unit")
        );
    }
    for (v, k) in [
        (ix.query_limit, "query_limit"),
        (ix.grab_limit, "grab_limit"),
    ] {
        if v == Some(0) {
            bail!("{} must be positive (omit it for no limit)", what(k));
        }
    }
    Ok(IndexerConfig {
        id: ix.id,
        name,
        kind,
        base_url,
        api_key,
        api_path,
        download_origins,
        key_origins,
        prowlarr_definition,
        magnet_trackers: trackers,
        uploaders: uploaders
            .into_iter()
            .map(|u| u.trim().to_string())
            .collect(),
        // The Pirate Bay: its top-100 recent list is a cheap static file. Knaben has no such
        // file (a recent read is a query on a volunteer-run service), so it is opt-in.
        recent_feed: ix.recent_feed.unwrap_or(kind != IndexerKind::Knaben),
        timeout: Duration::from_secs(within(&what("timeout_secs"), ix.timeout_secs, 1, 300)?),
        max_concurrent: within(&what("max_concurrent"), ix.max_concurrent as u64, 1, 8)? as usize,
        min_interval: Duration::from_millis(within(
            &what("min_interval_ms"),
            ix.min_interval_ms,
            0,
            600_000,
        )?),
        cache_ttl: Duration::from_secs(within(
            &what("cache_ttl_secs"),
            ix.cache_ttl_secs,
            0,
            86_400,
        )?),
        max_response_bytes: within(
            &what("max_response_bytes"),
            ix.max_response_bytes as u64,
            1024,
            64 * 1024 * 1024,
        )? as usize,
        max_pages: within(
            &what("max_pages"),
            u64::from(ix.max_pages.unwrap_or(2)),
            1,
            10,
        )? as u32,
        page_size: within(
            &what("page_size"),
            u64::from(ix.page_size.unwrap_or(100)),
            1,
            1000,
        )? as u32,
        query_limit: ix.query_limit,
        grab_limit: ix.grab_limit,
        limits_window,
    })
}

/// The standalone template: native indexers and the embedded engine first, everything that
/// writes off by default, credentials only as files.
pub fn standalone_template(listen: &str) -> String {
    let listen = quote(listen);
    format!(
        r#"# arr runtime configuration: standalone (native indexers, embedded downloader).
# Relative paths resolve against this file's directory. Credentials live only in files under
# secrets/ (chmod 600); never inline. Policy (profiles, seed goals, cleanup) is created with
# `arr setup` (or POST /api/v1/setup). See docs/STANDALONE.md.
[server]
listen = {listen}
allow_non_loopback = false
api_token_file = "secrets/api_token"
max_body_bytes = 65536
request_timeout_secs = 180
max_concurrent_requests = 16

[state]
dir = "."

# Native indexers, searched directly (no Prowlarr). Ids are stable policy keys.
# [[indexer]]
# id = 1
# name = "The Pirate Bay"
# kind = "thepiratebay"
# base_url = "https://apibay.org/"      # the exact API base URL you trust
# recent_feed = true                     # RSS from precompiled/data_top100_recent.json
# [[indexer]]
# id = 2
# name = "Knaben"
# kind = "knaben"                        # public trackers in one search (JSON API)
# base_url = "https://api.knaben.org/"
# [[indexer]]
# id = 3
# name = "Torznab example"
# kind = "torznab"
# base_url = "http://127.0.0.1:9117/api/v2.0/indexers/example/results/torznab/"
# api_key_file = "secrets/indexer_3_api_key"

[tvmaze]
# Free public TV metadata (no key): series discovery and full episode lists.
enabled = true
base_url = "https://api.tvmaze.com"

[tmdb]
# Movie discovery needs your own TMDB credential (v4 read token or v3 key) in a file.
enabled = false
base_url = "https://api.themoviedb.org"
# credential_file = "secrets/tmdb_credential"

[execution]
# Master switch for torrent submission, imports into [library], cleanup, RSS and Plex
# refreshes. Setup never turns it on; enable it deliberately once readiness passes.
enabled = false
download_cleanup = true

# The embedded BitTorrent engine (in this process; no qBittorrent).
# [embedded]
# download_dir = "/srv/arr/downloads"   # dedicated; not a library root
# listen = "0.0.0.0:6881"                          # BitTorrent TCP port
# trackers = true                                  # announce to the payload's trackers
# dht = true                                       # needed by trackerless magnets (TPB); private torrents never use it
# local_discovery = false
# max_active_downloads = 3

[migration]
# Still written so a rolled-back older release (which requires it) keeps working; "coexistence"
# is the only mode.
mode = "coexistence"

# [library]
# movies = "/srv/media/movies"
# tv = "/srv/media/tv"
# ffprobe = "/usr/bin/ffprobe"        # external dependency (Debian/Ubuntu: apt install ffmpeg)

# [plex]
# base_url = "http://127.0.0.1:32400"
# token_file = "secrets/plex_token"
# movie_section = 1
# tv_section = 2
# movies_path = "/data/movies"        # [library] movies as Plex sees it
# tv_path = "/data/tv"                # [library] tv as Plex sees it

# Remote MCP at /mcp (Streamable HTTP, static bearer token; optional Google OAuth sign-in,
# see config/standalone.example.toml and docs/MCP.md).
# [mcp]
# enabled = true
# token_file = "secrets/mcp_token"    # a different token than the API's
# allowed_hosts = ["localhost", "127.0.0.1"]
"#
    )
}

fn paths_overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Template written by `init`. Placeholders only; no secrets inline: the standalone layout
/// (native indexers, embedded engine).
pub fn template(listen: &str) -> String {
    standalone_template(listen)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(config: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        fsutil::ensure_private_dir(&dir).unwrap();
        fsutil::ensure_private_dir(&dir.join("secrets")).unwrap();
        fsutil::write_new_private_file(
            &dir.join("secrets/api_token"),
            b"0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        fsutil::write_new_private_file(
            &dir.join("secrets/prowlarr_api_key"),
            b"prowlarr-key-value",
        )
        .unwrap();
        let cfg = dir.join("config.toml");
        fsutil::write_new_private_file(&cfg, config.as_bytes()).unwrap();
        (tmp, cfg)
    }

    #[cfg(unix)] // fixtures use Unix absolute paths
    #[test]
    fn download_submission_is_the_old_name_of_enabled() {
        let base = "[server]\napi_token_file = \"secrets/api_token\"\n[prowlarr]\nbase_url = \"http://127.0.0.1:9696\"\napi_key_file = \"secrets/prowlarr_api_key\"\n[embedded]\ndownload_dir = \"/srv/um-dl\"\nlisten = \"127.0.0.1:0\"\n[library]\nmovies = \"/srv/movies\"\n[migration]\nmode = \"coexistence\"\n";
        let (_t, cfg) = setup(&format!("{base}[execution]\ndownload_submission = true\n"));
        assert!(load(&cfg).unwrap().execution_enabled);
        let (_t, cfg) = setup(&format!(
            "{base}[execution]\nenabled = false\ndownload_submission = true\n"
        ));
        let err = format!("{:#}", load(&cfg).unwrap_err());
        assert!(err.contains("duplicate field"), "{err}");
    }

    #[test]
    fn templates_quote_what_they_are_given() {
        assert!(standalone_template("127.0.0.1:8787").contains("listen = \"127.0.0.1:8787\""));
    }

    #[cfg(unix)] // fixtures use Unix absolute paths
    #[test]
    fn embedded_section_is_explicit_exclusive_and_validated() {
        let base = "[server]\napi_token_file = \"secrets/api_token\"\n[prowlarr]\nbase_url = \"http://127.0.0.1:9696\"\napi_key_file = \"secrets/prowlarr_api_key\"\n";
        let lib = "[library]\nmovies = \"/srv/movies\"\ntv = \"/srv/tv\"\n";
        let emb = "[embedded]\ndownload_dir = \"/srv/um-dl\"\nlisten = \"127.0.0.1:0\"\ntrackers = false\ninitial_peers = [\"127.0.0.1:51413\"]\nratio_limit = 2.0\n";
        let (_t, cfg) = setup(&format!(
            "{base}[execution]\nenabled = true\n{emb}{lib}[migration]\nmode = \"coexistence\"\n"
        ));
        let c = load(&cfg).unwrap();
        // No [qbittorrent] section, URL or credential is needed.
        assert!(c.execution_enabled && c.qbittorrent.is_none());
        assert_eq!(c.download_backend(), Some("embedded"));
        let e = c.embedded.as_ref().unwrap();
        assert_eq!(e.save_path, "/srv/um-dl");
        assert!(!e.trackers && !e.dht && !e.local_discovery);
        assert_eq!(e.initial_peers, ["127.0.0.1:51413".parse().unwrap()]);
        assert_eq!(e.session_dir, cfg.parent().unwrap().join("embedded"));
        assert_eq!(
            c.client_paths(),
            Some(("/srv/um-dl", Path::new("/srv/um-dl")))
        );
        let qb = "[qbittorrent]\nbase_url = \"http://127.0.0.1:8080\"\nsave_path = \"/downloads\"\nlocal_path = \"/srv/dl\"\n";
        // Every setup's state directory lives below the system temp directory.
        let around_state = std::env::temp_dir().display().to_string();
        let cases = [
            (
                format!("{base}{qb}{emb}"),
                "either [qbittorrent] or [embedded], not both",
            ),
            (
                format!(
                    "{base}[embedded]\ndownload_dir = \"/srv/movies/dl\"\nlisten = \"127.0.0.1:0\"\n{lib}"
                ),
                "must not contain each other",
            ),
            (
                format!(
                    "{base}[embedded]\ndownload_dir = '{around_state}'\nlisten = \"127.0.0.1:0\"\n"
                ),
                "state directory",
            ),
            (
                format!("{base}[embedded]\ndownload_dir = \"/\"\nlisten = \"127.0.0.1:0\"\n"),
                "dedicated directory",
            ),
            (
                format!("{base}[embedded]\ndownload_dir = \"dl\"\nlisten = \"127.0.0.1:0\"\n"),
                "absolute",
            ),
            (
                format!(
                    "{base}[embedded]\ndownload_dir = \"/srv/um-dl\"\nlisten = \"localhost\"\n"
                ),
                "embedded.listen",
            ),
            (
                format!(
                    "{base}[embedded]\ndownload_dir = \"/srv/um-dl\"\nlisten = \"127.0.0.1:0\"\ninitial_peers = [\"peer.example\"]\n"
                ),
                "embedded.initial_peers",
            ),
            (
                format!(
                    "{base}[embedded]\ndownload_dir = \"/srv/um-dl\"\nlisten = \"127.0.0.1:0\"\nratio_limit = -1.0\n"
                ),
                "embedded.ratio_limit",
            ),
            (
                format!(
                    "{base}[embedded]\ndownload_dir = \"/srv/um-dl\"\nlisten = \"127.0.0.1:0\"\nupload_limit_bytes_per_sec = 0\n"
                ),
                "must be positive",
            ),
            (
                format!(
                    "{base}[embedded]\ndownload_dir = \"/srv/um-dl\"\nlisten = \"127.0.0.1:0\"\nupnp = true\n"
                ),
                "unknown field",
            ),
            (
                format!("{base}[embedded]\nlisten = \"127.0.0.1:0\"\n"),
                "download_dir",
            ),
        ];
        for (text, needle) in cases {
            let (_t, cfg) = setup(&text);
            let err = format!("{:#}", load(&cfg).unwrap_err());
            assert!(err.contains(needle), "{needle}: {err}");
        }
    }

    #[cfg(unix)] // fixtures use Unix absolute paths
    #[test]
    fn execution_sections_are_validated() {
        let base = "[server]\napi_token_file = \"secrets/api_token\"\n[prowlarr]\nbase_url = \"http://127.0.0.1:9696\"\napi_key_file = \"secrets/prowlarr_api_key\"\n";
        let qb = "[qbittorrent]\nbase_url = \"http://127.0.0.1:8080\"\nsave_path = \"/downloads/\"\nlocal_path = \"/srv/dl\"\n";
        let lib = "[library]\nmovies = \"/srv/movies\"\ntv = \"/srv/tv\"\n";
        let (_t, cfg) = setup(&format!(
            "{base}[execution]\nenabled = true\ndownload_cleanup = false\n{qb}{lib}[automation]\nrss_interval_minutes = 0\n[migration]\nmode = \"coexistence\"\n"
        ));
        let c = load(&cfg).unwrap();
        assert!(c.execution_enabled && !c.download_cleanup);
        assert_eq!(c.migration, MigrationMode::Coexistence);
        // Legacy mode (Sonarr/Radarr import) is refused with a pointer to the fix.
        let (_t5, cfg5) = setup(&format!("{base}[migration]\nmode = \"legacy\"\n"));
        let err = format!("{:#}", load(&cfg5).unwrap_err());
        assert!(
            err.contains("no longer supports") && err.contains("coexistence"),
            "{err}"
        );
        // Execution needs an explicit mode; without execution the default is coexistence.
        let (_t2, cfg2) = setup(&format!("{base}[execution]\nenabled = true\n{qb}{lib}"));
        let err = format!("{:#}", load(&cfg2).unwrap_err());
        assert!(err.contains("explicit [migration] mode"), "{err}");
        let (_t3, cfg3) = setup(base);
        assert_eq!(load(&cfg3).unwrap().migration, MigrationMode::Coexistence);
        let (_t4, cfg4) = setup(&format!("{base}[migration]\nmode = \"cutover\"\n"));
        assert!(format!("{:#}", load(&cfg4).unwrap_err()).contains("migration.mode"));
        assert_eq!(c.qbittorrent.as_ref().unwrap().save_path, "/downloads");
        assert_eq!(c.automation.rss_interval_minutes, Some(0));
        let cases = [
            (
                format!("{base}[execution]\nenabled = true\n{lib}"),
                "[qbittorrent]",
            ),
            (
                format!("{base}[execution]\nenabled = true\n{qb}"),
                "[library]",
            ),
            (
                format!("{base}{qb}[library]\nmovies = \"/srv/dl/movies\"\n"),
                "must not contain each other",
            ),
            (
                format!("{base}{qb}[library]\nmovies = \"relative\"\n"),
                "absolute",
            ),
            (
                format!("{base}[automation]\nrss_interval_minutes = 5\n"),
                "rss_interval_minutes",
            ),
            (
                format!(
                    "{base}[qbittorrent]\nbase_url = \"http://h/\"\nsave_path = \"/d\"\nlocal_path = \"/l\"\nusername_file = \"secrets/api_token\"\n"
                ),
                "set together",
            ),
        ];
        for (text, needle) in cases {
            let (_t, cfg) = setup(&text);
            let err = format!("{:#}", load(&cfg).unwrap_err());
            assert!(err.contains(needle), "{needle}: {err}");
        }
    }

    /// Uncomments `# key = value` lines and `# [section]` headers.
    fn uncomment(text: &str) -> String {
        text.lines()
            .map(|l| match l.strip_prefix("# ") {
                Some(rest)
                    if rest.starts_with('[')
                        || rest
                            .split_once(" = ")
                            .is_some_and(|(k, _)| !k.contains(' ')) =>
                {
                    rest
                }
                _ => l,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn standalone_template_and_example_are_real_and_safe() {
        let (_t, cfg) = setup(&standalone_template("127.0.0.1:8787"));
        let c = load(&cfg).unwrap();
        assert!(!c.execution_enabled && c.prowlarr.is_none() && c.indexers.is_empty());
        assert!(c.tvmaze.is_some() && c.mcp.is_none());
        let fc: FileConfig =
            toml::from_str(&uncomment(&standalone_template("127.0.0.1:8787"))).unwrap();
        assert_eq!(fc.indexer.len(), 3, "all documented indexers are real keys");
        let e = fc.embedded.as_ref().unwrap();
        assert!(
            e.dht,
            "the documented default discovers peers for trackerless magnets"
        );
        assert!(fc.plex.as_ref().unwrap().movies_path.is_some() && fc.mcp.is_some());
        assert!(fc.prowlarr.is_none() && fc.qbittorrent.is_none());
        let example = include_str!("../config/standalone.example.toml");
        let fc: FileConfig = toml::from_str(example).unwrap();
        assert!(fc.prowlarr.is_none() && fc.qbittorrent.is_none() && !fc.execution.enabled);
        assert!(fc.embedded.as_ref().is_some_and(|e| e.dht));
        toml::from_str::<FileConfig>(&uncomment(example)).unwrap();
    }

    #[test]
    fn indexer_entries_are_validated_without_echoing_values() {
        let base = "[server]\napi_token_file = \"secrets/api_token\"\n";
        let tpb = "[[indexer]]\nid = 1\nname = \"The Pirate Bay\"\nkind = \"thepiratebay\"\nbase_url = \"https://apibay.example/\"\n";
        let (_t, cfg) = setup(&format!("{base}{tpb}"));
        let c = load(&cfg).unwrap();
        assert_eq!(c.search_backend(), Some("native"));
        let ix = &c.indexers[0];
        assert!(ix.recent_feed && ix.magnet_trackers.is_empty() && ix.uploaders.is_empty());
        assert_eq!(ix.min_interval, Duration::from_millis(2000));
        let cases = [
            (
                format!(
                    "{base}{tpb}[prowlarr]\nbase_url = \"http://127.0.0.1:9696\"\napi_key_file = \"secrets/prowlarr_api_key\"\n"
                ),
                "not both",
            ),
            (format!("{base}{tpb}{tpb}"), "configured twice"),
            (
                format!(
                    "{base}[[indexer]]\nid = 3\nname = \"x\"\nkind = \"1337x\"\nbase_url = \"https://x.example/\"\n"
                ),
                "not supported natively",
            ),
            (
                format!("{base}{tpb}api_key_file = \"secrets/prowlarr_api_key\"\n"),
                "applies only to kind = \"torznab\"",
            ),
            (
                format!(
                    "{base}[[indexer]]\nid = 2\nname = \"t\"\nkind = \"torznab\"\nbase_url = \"https://t.example/\"\nrecent_feed = false\n"
                ),
                "applies only to kind = \"thepiratebay\"",
            ),
            (
                format!(
                    "{base}[[indexer]]\nid = 0\nname = \"x\"\nkind = \"thepiratebay\"\nbase_url = \"https://x.example/\"\n"
                ),
                "between 1 and",
            ),
            (
                format!("{base}{tpb}limits_unit = \"week\"\n"),
                "limits_unit",
            ),
            (
                format!(
                    "{base}[[indexer]]\nid = 2\nname = \"t\"\nkind = \"torznab\"\nbase_url = \"https://t.example/\"\ndownload_origins = [\"https://dl.example/path\"]\n"
                ),
                "without a path",
            ),
        ];
        for (text, needle) in cases {
            let (_t, cfg) = setup(&text);
            let err = format!("{:#}", load(&cfg).unwrap_err());
            assert!(err.contains(needle), "{needle}: {err}");
        }
        // Knaben: the same magnet keys as The Pirate Bay, its own feed default, no uploaders.
        let kn = "[[indexer]]\nid = 2\nname = \"Knaben\"\nkind = \"knaben\"\nbase_url = \"https://api.knaben.example/\"\n";
        let (_t, cfg) = setup(&format!("{base}{tpb}{kn}"));
        let c = load(&cfg).unwrap();
        assert_eq!(c.indexers[1].kind, IndexerKind::Knaben);
        assert!(c.indexers[0].recent_feed && !c.indexers[1].recent_feed);
        assert_eq!(c.indexers[1].kind.as_str(), "knaben");
        let (_t, cfg) = setup(&format!(
            "{base}{kn}recent_feed = true\nmagnet_trackers = [\"udp://ok.example:1337/announce\"]\n"
        ));
        let c = load(&cfg).unwrap();
        assert!(c.indexers[0].recent_feed && c.indexers[0].magnet_trackers.len() == 1);
        for (extra, needle) in [
            (
                "uploaders = [\"x\"]\n",
                "applies only to kind = \"thepiratebay\"",
            ),
            (
                "api_key_file = \"secrets/k\"\n",
                "applies only to kind = \"torznab\"",
            ),
        ] {
            let (_t, cfg) = setup(&format!("{base}{kn}{extra}"));
            let err = format!("{:#}", load(&cfg).unwrap_err());
            assert!(err.contains(needle), "{needle}: {err}");
        }
        // A rejected tracker URL is never echoed (it may carry a passkey).
        let (_t, cfg) = setup(&format!(
            "{base}{tpb}magnet_trackers = [\"udp://ok.example:1337/announce\", \"https://t.example/announce?passkey=SECRETPASS\"]\n"
        ));
        let err = format!("{:#}", load(&cfg).unwrap_err());
        assert!(
            err.contains("entry 2") && !err.contains("SECRETPASS"),
            "{err}"
        );
        let (_t, cfg) = setup(&format!(
            "{base}{tpb}magnet_trackers = [\"udp://ok.example:1337/announce\"]\n"
        ));
        let c = load(&cfg).unwrap();
        let dbg = format!("{:?}", c.indexers[0]);
        assert!(
            !dbg.contains("ok.example") && dbg.contains("redacted"),
            "{dbg}"
        );
    }

    #[test]
    fn mcp_section_is_explicit_and_bounded() {
        let base = "[server]\napi_token_file = \"secrets/api_token\"\n";
        let mk = |dir: &Path, token: &str| {
            fsutil::write_new_private_file(&dir.join("secrets/mcp_token"), token.as_bytes())
                .unwrap();
        };
        let (_t, cfg) = setup(&format!(
            "{base}[mcp]\nenabled = true\ntoken_file = \"secrets/mcp_token\"\n"
        ));
        mk(cfg.parent().unwrap(), "mcp-token-0123456789abcdef0123");
        let c = load(&cfg).unwrap();
        let m = c.mcp.as_ref().unwrap();
        assert_eq!(m.allowed_hosts, ["localhost", "127.0.0.1", "::1"]);
        assert!(m.allowed_origins.is_empty());
        let (_t, cfg) = setup(&format!(
            "{base}[mcp]\nenabled = true\ntoken_file = \"secrets/api_token\"\n"
        ));
        assert!(format!("{:#}", load(&cfg).unwrap_err()).contains("different token"));
        let (_t, cfg) = setup(
            "[server]\nlisten = \"0.0.0.0:8787\"\nallow_non_loopback = true\napi_token_file = \"secrets/api_token\"\n[mcp]\nenabled = true\ntoken_file = \"secrets/mcp_token\"\n",
        );
        mk(cfg.parent().unwrap(), "mcp-token-0123456789abcdef0123");
        assert!(
            format!("{:#}", load(&cfg).unwrap_err()).contains("mcp.allowed_hosts"),
            "a public listener needs explicit hosts"
        );
        let (_t, cfg) = setup(&format!("{base}[mcp]\nenabled = true\n"));
        assert!(format!("{:#}", load(&cfg).unwrap_err()).contains("mcp.token_file"));
        let (_t, cfg) = setup(&format!("{base}[mcp]\nenabled = false\n"));
        assert!(
            load(&cfg).unwrap().mcp.is_none(),
            "disabled means no endpoint"
        );
    }

    #[test]
    fn oauth_section_is_validated() {
        let base = "[server]\napi_token_file = \"secrets/api_token\"\n[mcp]\nenabled = true\ntoken_file = \"secrets/mcp_token\"\nallowed_hosts = [\"media.example.org\"]\n[mcp.oauth]\ngoogle_client_id = \"id\"\ngoogle_client_secret_file = \"secrets/g\"\n";
        let try_load = |extra: &str| {
            let (_t, cfg) = setup(&format!("{base}{extra}"));
            let d = cfg.parent().unwrap();
            for (n, v) in [
                ("mcp_token", "mcp-token-0123456789abcdef0123"),
                ("g", "secret"),
            ] {
                fsutil::write_new_private_file(&d.join("secrets").join(n), v.as_bytes()).unwrap();
            }
            load(&cfg).map_err(|e| format!("{e:#}"))
        };
        let ok =
            "public_url = \"https://media.example.org\"\nallowed_emails = [\"You@Example.com\"]\n";
        let c = try_load(ok).unwrap();
        let o = c.mcp.unwrap().oauth.unwrap();
        assert_eq!(o.public_url, "https://media.example.org");
        assert_eq!(o.allowed_emails, ["you@example.com"]);
        assert_eq!(o.trusted_client_metadata, ["https://chatgpt.com/oauth/"]);
        for (bad, why) in [
            (
                "public_url = \"https://other.example.org\"\nallowed_emails = [\"a@b.c\"]\n",
                "allowed_hosts",
            ),
            (
                "public_url = \"http://media.example.org\"\nallowed_emails = [\"a@b.c\"]\n",
                "https",
            ),
            (
                "public_url = \"https://media.example.org/x\"\nallowed_emails = [\"a@b.c\"]\n",
                "origin only",
            ),
            (
                "public_url = \"https://media.example.org\"\nallowed_emails = [\"*@b.c\"]\n",
                "exact",
            ),
            (
                "public_url = \"https://media.example.org\"\nallowed_emails = []\n",
                "1 to 32",
            ),
            (
                "public_url = \"https://media.example.org\"\nallowed_emails = [\"a@b.c\"]\ngoogle_token_endpoint = \"http://example.com/t\"\n",
                "https",
            ),
        ] {
            let e = try_load(bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
    }

    #[cfg(unix)] // fixtures use Unix absolute paths
    #[test]
    fn deferred_secrets_list_missing_files_without_reading_values() {
        let text = "[server]\napi_token_file = \"secrets/api_token\"\n[plex]\nbase_url = \"http://127.0.0.1:32400\"\ntoken_file = \"secrets/plex_token\"\nmovies_path = \"/data/movies/\"\n";
        let (_t, cfg) = setup(text);
        let base = cfg.parent().unwrap();
        assert!(load(&cfg).is_err(), "serving needs the secret");
        let (c, pending) = parse_config(text, base, "x", Secrets::Defer).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].contains("plex.token_file"));
        assert_eq!(
            c.plex.as_ref().unwrap().movies_path.as_deref(),
            Some("/data/movies")
        );
    }

    #[test]
    fn template_loads_and_defaults_are_safe() {
        let (_t, cfg) = setup(&template("127.0.0.1:8787"));
        let c = load(&cfg).unwrap();
        assert!(c.listen.ip().is_loopback());
        assert!(c.prowlarr.is_none());
        assert!(c.tmdb.is_none());
        assert!(!c.execution_enabled);
        assert!(c.qbittorrent.is_none() && c.library.is_none() && c.plex.is_none());
        assert!(
            !format!("{c:?}").contains("prowlarr-key-value"),
            "Debug must redact secrets"
        );
        assert!(!format!("{c:?}").contains("0123456789abcdef"));
    }

    #[test]
    fn rejects_unsafe_settings() {
        let bad = [
            (
                "[server]\nlisten = \"0.0.0.0:8787\"\napi_token_file = \"secrets/api_token\"\n",
                "not loopback",
            ),
            (
                "[server]\napi_token_file = \"secrets/api_token\"\nunknown_key = 1\n",
                "unknown field",
            ),
            (
                "[server]\napi_token_file = \"secrets/api_token\"\n[prowlarr]\nbase_url = \"http://user:pw@host:9696\"\napi_key_file = \"secrets/prowlarr_api_key\"\n",
                "credentials",
            ),
            (
                "[server]\napi_token_file = \"secrets/api_token\"\n[prowlarr]\nbase_url = \"http://host:9696/?apikey=x\"\napi_key_file = \"secrets/prowlarr_api_key\"\n",
                "query",
            ),
            (
                "[server]\napi_token_file = \"secrets/api_token\"\n[prowlarr]\nbase_url = \"file:///etc/passwd\"\napi_key_file = \"secrets/prowlarr_api_key\"\n",
                "scheme",
            ),
            (
                "[server]\napi_token_file = \"secrets/api_token\"\n[tmdb]\nenabled = true\n",
                "credential_file",
            ),
            (
                "[server]\napi_token_file = \"secrets/missing\"\n",
                "api_token_file",
            ),
        ];
        for (cfg, expect) in bad {
            let (_t, path) = setup(cfg);
            let err = format!("{:#}", load(&path).unwrap_err());
            assert!(err.contains(expect), "expected '{expect}' in: {err}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_loopback_requires_explicit_opt_in_and_secret_modes_are_checked() {
        use std::os::unix::fs::PermissionsExt;
        let (_t, cfg) = setup(
            "[server]\nlisten = \"0.0.0.0:8787\"\nallow_non_loopback = true\napi_token_file = \"secrets/api_token\"\n",
        );
        assert!(load(&cfg).is_ok());
        let token = cfg.parent().unwrap().join("secrets/api_token");
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(format!("{:#}", load(&cfg).unwrap_err()).contains("chmod 600"));
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(&cfg).is_err(), "config file itself must be private");
    }
}
