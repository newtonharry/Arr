//! Torznab (spec 1.3) for native indexers, and the hardened Torznab/RSS 2.0 parser shared
//! with Prowlarr's feeds.
//!
//! - `t=caps` is read (and cached for a day) to learn the search modes, the result limit and
//!   the categories the endpoint offers. A search refuses an endpoint whose caps say
//!   `search` is unavailable.
//! - `t=search&q=…&cat=…&limit=…&offset=…` pages until a short page, the `total` the
//!   endpoint reports, or `max_pages` (then the result is reported truncated). An empty `q`
//!   is the latest releases (RSS).
//! - `<error code=…>` answers map to typed failures by their code; the endpoint's own
//!   description is never repeated (it can quote a credential).
//! - The API key goes only to the configured origin, as the `apikey` parameter the spec
//!   defines. Item links are kept only when they are magnets or on the configured origin or
//!   an explicitly configured download origin; a link that would carry the key to another
//!   origin is refused. `.torrent` downloads follow at most one redirect, to a magnet or an
//!   allowed origin, are size-bounded and must parse as a torrent.
//!
//! XML is parsed without DTDs (no entity expansion, nothing fetched), with node and item
//! bounds, reading only the RSS 2.0 item children and Torznab/Newznab `attr` elements needed.

use crate::import::rfc3339;
use crate::indexer::{FeedWindow, Indexer, Release};
use crate::prowlarr::{Category, Payload, PrivateLink, SearchOutcome, SearchRequest};
use crate::upstream::{self, UpstreamError};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use time::format_description::well_known::{Rfc2822, Rfc3339};

pub const TORZNAB_NS: &str = "http://torznab.com/schemas/2015/feed";
pub const NEWZNAB_NS: &str = "http://www.newznab.com/DTD/2010/feeds/attributes/";
/// Items read from one document at most.
pub const MAX_ITEMS: usize = 500;
const MAX_NODES: u32 = 200_000;
const MAX_TORRENT_BYTES: usize = 10 * 1024 * 1024;
const CAPS_TTL: Duration = Duration::from_secs(86_400);

/// A parse failure: the endpoint's own Torznab error, or an unusable document.
#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    /// `<error code="…"/>` (the code only; the description is never kept).
    Torznab(String),
    Invalid(String),
}

impl ParseError {
    pub fn message(&self) -> String {
        match self {
            ParseError::Torznab(code) => format!(
                "the indexer answered with Torznab error {code}{}",
                error_meaning(code)
                    .map(|m| format!(" ({m})"))
                    .unwrap_or_default()
            ),
            ParseError::Invalid(why) => why.clone(),
        }
    }

    /// The typed failure for a Torznab error code (spec section "Error codes").
    pub fn upstream(&self) -> UpstreamError {
        match self {
            ParseError::Torznab(code) => match code.as_str() {
                "100" => UpstreamError::Auth { status: 401 },
                "101" | "102" => UpstreamError::Auth { status: 403 },
                "500" | "501" => UpstreamError::Throttled {
                    reason: self.message(),
                },
                "900" | "910" => UpstreamError::Unavailable {
                    reason: self.message(),
                },
                _ => UpstreamError::BadResponse {
                    reason: self.message(),
                },
            },
            ParseError::Invalid(why) => UpstreamError::BadResponse {
                reason: why.clone(),
            },
        }
    }
}

fn error_meaning(code: &str) -> Option<&'static str> {
    Some(match code {
        "100" => "incorrect user credentials",
        "101" => "account suspended",
        "102" => "insufficient privileges",
        "200" => "missing parameter",
        "201" => "incorrect parameter",
        "202" => "no such function",
        "203" => "function not available",
        "300" => "no such item",
        "500" => "request limit reached",
        "501" => "download limit reached",
        "900" => "unknown error",
        "910" => "API disabled",
        _ => return None,
    })
}

/// One parsed document.
#[derive(Debug, Default)]
pub struct Page {
    pub items: Vec<Release>,
    /// `<item>` elements in the document (before any was dropped).
    pub raw: usize,
    pub links_dropped: usize,
    pub truncated: bool,
    /// `torznab:response`/`newznab:response` paging attributes, when present.
    pub offset: Option<u64>,
    pub total: Option<u64>,
}

fn child_text<'a>(n: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    n.children()
        .find(|c| {
            c.is_element() && c.tag_name().name() == name && c.tag_name().namespace().is_none()
        })
        .and_then(|c| c.text())
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

fn rfc2822_to_3339(s: &str) -> Option<String> {
    OffsetDateTime::parse(s.trim(), &Rfc2822)
        .ok()
        .or_else(|| OffsetDateTime::parse(s.trim(), &Rfc3339).ok())
        .map(rfc3339)
}

/// Which item links are kept.
pub struct LinkPolicy<'a> {
    /// Origins HTTP links may be on (the endpoint's own first).
    pub allowed: &'a [reqwest::Url],
    /// The endpoint's API key, which may appear only on `key_origins` (never in a link,
    /// tracker or web seed on any other origin).
    pub key: Option<&'a str>,
    pub key_origins: &'a [reqwest::Url],
}

impl<'a> LinkPolicy<'a> {
    /// Links on `allowed` origins, no key checks (Prowlarr's feeds).
    pub fn origins(allowed: &'a [reqwest::Url]) -> Self {
        LinkPolicy {
            allowed,
            key: None,
            key_origins: &[],
        }
    }
}

/// Whether any of `urls` carries `key` (raw or percent-decoded) on an origin outside
/// `key_origins`. A value that names the key but is not a URL counts as a leak.
pub fn leaks_key<'u>(
    key: Option<&str>,
    key_origins: &[reqwest::Url],
    urls: impl IntoIterator<Item = &'u str>,
) -> bool {
    let Some(key) = key.filter(|k| !k.is_empty()) else {
        return false;
    };
    urls.into_iter().any(|u| {
        let decoded = percent_decoded(u);
        if !u.contains(key) && !decoded.contains(key) {
            return false;
        }
        match reqwest::Url::parse(u) {
            Ok(url) => !key_origins.iter().any(|o| o.origin() == url.origin()),
            Err(_) => true,
        }
    })
}

/// Every parameter value of a magnet except its info-hash (trackers, web seeds, sources).
fn magnet_values(magnet: &str) -> Vec<String> {
    reqwest::Url::parse(magnet)
        .map(|u| {
            u.query_pairs()
                .filter(|(k, _)| k != "xt")
                .map(|(_, v)| v.into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Announce and web-seed URLs of a `.torrent`.
pub fn torrent_urls(torrent: &[u8]) -> Vec<String> {
    let Ok(doc) = crate::torrent::parse(torrent) else {
        return vec![];
    };
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let mut out = vec![];
    for key in ["announce", "url-list"] {
        match doc.get(key) {
            Some((crate::torrent::Bencode::Bytes(b), _)) => out.push(text(b)),
            Some((crate::torrent::Bencode::List(l), _)) => {
                out.extend(l.iter().filter_map(|x| x.bytes()).map(text))
            }
            _ => {}
        }
    }
    if let Some((crate::torrent::Bencode::List(tiers), _)) = doc.get("announce-list") {
        for t in tiers.iter().take(100) {
            if let crate::torrent::Bencode::List(l) = t {
                out.extend(l.iter().take(100).filter_map(|x| x.bytes()).map(text));
            }
        }
    }
    out
}

/// Keeps a link only if it is a valid magnet or on one of the allowed origins, and it does
/// not carry the endpoint's key to another origin.
fn trusted_link(
    raw: &str,
    policy: &LinkPolicy,
) -> Option<(Option<PrivateLink>, Option<PrivateLink>)> {
    let raw = raw.trim();
    if raw.len() > 16 * 1024 {
        return None;
    }
    if raw.starts_with("magnet:?") {
        crate::torrent::magnet_hash(raw).ok()?;
        let values = magnet_values(raw);
        if leaks_key(
            policy.key,
            policy.key_origins,
            values.iter().map(String::as_str),
        ) {
            return None;
        }
        return Some((None, Some(PrivateLink::new(raw.to_string()))));
    }
    let url = reqwest::Url::parse(raw).ok()?;
    let ok = matches!(url.scheme(), "http" | "https")
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && policy.allowed.iter().any(|a| a.origin() == url.origin())
        && !leaks_key(policy.key, policy.key_origins, [raw]);
    ok.then(|| (Some(PrivateLink::new(raw.to_string())), None))
}

fn parse_doc(xml: &[u8]) -> Result<roxmltree::Document<'_>, ParseError> {
    let text = std::str::from_utf8(xml.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(xml))
        .map_err(|_| ParseError::Invalid("the feed is not UTF-8".into()))?;
    let opts = roxmltree::ParsingOptions {
        allow_dtd: false,
        nodes_limit: MAX_NODES,
        ..Default::default()
    };
    roxmltree::Document::parse_with_options(text, opts).map_err(|e| {
        ParseError::Invalid(match e {
            roxmltree::Error::DtdDetected => "the feed contains a DTD (refused)".into(),
            roxmltree::Error::NodesLimitReached => "the feed has too many XML nodes".into(),
            _ => "the feed is not well-formed XML".into(),
        })
    })
}

fn torznab_error(root: roxmltree::Node) -> Option<ParseError> {
    (root.tag_name().name() == "error").then(|| {
        ParseError::Torznab(
            root.attribute("code")
                .filter(|c| !c.is_empty() && c.len() <= 8 && c.bytes().all(|b| b.is_ascii_digit()))
                .unwrap_or("?")
                .to_string(),
        )
    })
}

/// Parses an untrusted Torznab/RSS 2.0 document. Links are kept only as `policy` allows;
/// items carry `indexer_id` and the endpoint's indexer name (or `default_name`).
pub fn parse_feed(
    xml: &[u8],
    policy: &LinkPolicy,
    indexer_id: u32,
    default_name: Option<&str>,
) -> Result<Page, ParseError> {
    let doc = parse_doc(xml)?;
    let root = doc.root_element();
    if let Some(e) = torznab_error(root) {
        return Err(e);
    }
    if root.tag_name().name() != "rss" {
        return Err(ParseError::Invalid(
            "the document is not an RSS feed".into(),
        ));
    }
    let channel = root
        .children()
        .find(|c| c.is_element() && c.tag_name().name() == "channel")
        .ok_or_else(|| ParseError::Invalid("the feed has no channel".into()))?;
    let mut page = Page::default();
    if let Some(r) = channel.children().find(|c| {
        c.is_element()
            && c.tag_name().name() == "response"
            && matches!(c.tag_name().namespace(), Some(TORZNAB_NS | NEWZNAB_NS))
    }) {
        page.offset = r.attribute("offset").and_then(|v| v.trim().parse().ok());
        page.total = r.attribute("total").and_then(|v| v.trim().parse().ok());
    }
    let items: Vec<_> = channel
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "item")
        .collect();
    page.raw = items.len();
    page.truncated = items.len() > MAX_ITEMS;
    for item in items.into_iter().take(MAX_ITEMS) {
        let Some(title) = child_text(item, "title") else {
            continue;
        };
        if title.chars().count() > 1000 || title.chars().any(char::is_control) {
            continue;
        }
        let mut attrs: Vec<(&str, &str)> = vec![];
        for c in item
            .children()
            .filter(|c| c.is_element() && c.tag_name().name() == "attr")
        {
            if matches!(c.tag_name().namespace(), Some(TORZNAB_NS | NEWZNAB_NS))
                && let (Some(n), Some(v)) = (c.attribute("name"), c.attribute("value"))
            {
                attrs.push((n, v));
            }
        }
        let attr = |n: &str| {
            attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(n))
                .map(|(_, v)| v.trim())
        };
        let enclosure = item.children().find(|c| {
            c.is_element()
                && c.tag_name().name() == "enclosure"
                && c.tag_name().namespace().is_none()
        });
        let mut download = None;
        let mut magnet = None;
        for raw in [
            attr("magneturl"),
            enclosure.and_then(|e| e.attribute("url")),
            child_text(item, "link"),
        ]
        .into_iter()
        .flatten()
        {
            match trusted_link(raw, policy) {
                Some((Some(d), None)) if download.is_none() => download = Some(d),
                Some((None, Some(m))) if magnet.is_none() => magnet = Some(m),
                Some(_) => {}
                None => page.links_dropped += 1,
            }
        }
        let size = attr("size")
            .and_then(|s| s.parse::<i64>().ok())
            .or_else(|| child_text(item, "size").and_then(|s| s.parse().ok()))
            .or_else(|| {
                enclosure
                    .and_then(|e| e.attribute("length"))
                    .and_then(|s| s.parse().ok())
            })
            .filter(|s| *s > 0);
        let mut categories: Vec<Category> = attrs
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("category"))
            .filter_map(|(_, v)| v.trim().parse::<i64>().ok())
            .map(|id| Category { id: Some(id) })
            .collect();
        for c in item.children().filter(|c| {
            c.is_element()
                && c.tag_name().name() == "category"
                && c.tag_name().namespace().is_none()
        }) {
            if let Some(id) = c.text().and_then(|t| t.trim().parse::<i64>().ok())
                && !categories.iter().any(|x| x.id == Some(id))
            {
                categories.push(Category { id: Some(id) });
            }
        }
        let seeders = attr("seeders")
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|s| *s >= 0);
        let peers = attr("peers").and_then(|s| s.parse::<i64>().ok());
        let leechers = attr("leechers")
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|l| *l >= 0)
            .or(match (peers, seeders) {
                (Some(p), Some(s)) if p >= s => Some(p - s),
                _ => None,
            });
        let info_hash = attr("infohash")
            .map(str::to_ascii_lowercase)
            .filter(|h| crate::torrent::is_info_hash(h));
        let float = |n: &str| {
            attr(n)
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0)
        };
        page.items.push(Release {
            minimum_ratio: float("minimumratio"),
            minimum_seed_time: attr("minimumseedtime")
                .and_then(|v| v.parse::<i64>().ok())
                .filter(|v| *v >= 0),
            download_volume_factor: float("downloadvolumefactor"),
            upload_volume_factor: float("uploadvolumefactor"),
            guid: child_text(item, "guid").map(|g| g.chars().take(2048).collect()),
            title: Some(title.to_string()),
            size,
            indexer_id: Some(i64::from(indexer_id)),
            indexer: child_text(item, "prowlarrindexer")
                .or_else(|| child_text(item, "jackettindexer"))
                .or(default_name)
                .map(|n| n.chars().take(200).collect()),
            seeders,
            leechers,
            protocol: Some("torrent".into()),
            publish_date: child_text(item, "pubDate").and_then(rfc2822_to_3339),
            info_hash,
            download_url: download,
            magnet_url: magnet,
            categories: Some(categories),
        });
    }
    Ok(page)
}

/// What an endpoint's `t=caps` says.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Caps {
    pub search: bool,
    pub tv_search: bool,
    pub movie_search: bool,
    pub limit_max: Option<u32>,
    pub limit_default: Option<u32>,
    /// Category ids offered (subcategories included).
    pub categories: Vec<u32>,
}

impl Caps {
    pub fn summary(&self) -> Value {
        json!({
            "search": self.search,
            "tv_search": self.tv_search,
            "movie_search": self.movie_search,
            "limit_max": self.limit_max,
            "limit_default": self.limit_default,
            "categories": self.categories.len(),
        })
    }
}

pub fn parse_caps(xml: &[u8]) -> Result<Caps, ParseError> {
    let doc = parse_doc(xml)?;
    let root = doc.root_element();
    if let Some(e) = torznab_error(root) {
        return Err(e);
    }
    if root.tag_name().name() != "caps" {
        return Err(ParseError::Invalid(
            "the caps document has no <caps> root".into(),
        ));
    }
    fn child<'a, 'i>(n: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
        n.children()
            .find(|c| c.is_element() && c.tag_name().name() == name)
    }
    let mut caps = Caps::default();
    if let Some(l) = child(root, "limits") {
        caps.limit_max = l.attribute("max").and_then(|v| v.parse().ok());
        caps.limit_default = l.attribute("default").and_then(|v| v.parse().ok());
    }
    if let Some(s) = child(root, "searching") {
        let available =
            |name: &str| child(s, name).is_some_and(|n| n.attribute("available") == Some("yes"));
        caps.search = available("search");
        caps.tv_search = available("tv-search");
        caps.movie_search = available("movie-search");
    }
    if let Some(c) = child(root, "categories") {
        for cat in c
            .descendants()
            .filter(|n| n.is_element() && matches!(n.tag_name().name(), "category" | "subcat"))
        {
            if let Some(id) = cat.attribute("id").and_then(|v| v.parse().ok()) {
                caps.categories.push(id);
            }
            if caps.categories.len() >= 2000 {
                break;
            }
        }
    }
    Ok(caps)
}

fn api_url(ix: &Indexer, params: &[(&str, String)]) -> Result<reqwest::Url, UpstreamError> {
    let mut url =
        ix.cfg
            .base_url
            .join(&ix.cfg.api_path)
            .map_err(|_| UpstreamError::BadResponse {
                reason: "invalid base URL".into(),
            })?;
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in params {
            q.append_pair(k, v);
        }
        if let Some(k) = &ix.cfg.api_key {
            q.append_pair("apikey", k.expose());
        }
    }
    Ok(url)
}

/// One API request (`query` counts against the query limit), parsed as a feed or caps.
async fn api_get(
    ix: &Indexer,
    params: &[(&str, String)],
    query: bool,
) -> Result<Vec<u8>, UpstreamError> {
    let url = api_url(ix, params)?;
    let resp = ix
        .send(
            ix.http()
                .get(url)
                .header("Accept", "application/rss+xml, application/xml, text/xml"),
            query,
        )
        .await?;
    upstream::read_bytes(resp, ix.cfg.max_response_bytes).await
}

/// The endpoint's caps (cached for a day; refreshed after expiry).
pub async fn caps(ix: &Indexer) -> Result<Caps, UpstreamError> {
    if let Some((at, c)) = ix.caps.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        && at.elapsed() < CAPS_TTL
    {
        return Ok(c.clone());
    }
    let body = api_get(ix, &[("t", "caps".into())], false).await?;
    let c = parse_caps(&body).map_err(|e| e.upstream())?;
    *ix.caps.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), c.clone()));
    Ok(c)
}

fn allowed_origins(ix: &Indexer) -> Vec<reqwest::Url> {
    std::iter::once(ix.cfg.base_url.clone())
        .chain(ix.cfg.download_origins.iter().cloned())
        .collect()
}

/// Where the API key may appear: the endpoint itself and explicitly configured key origins.
fn key_origins(ix: &Indexer) -> Vec<reqwest::Url> {
    std::iter::once(ix.cfg.base_url.clone())
        .chain(ix.cfg.key_origins.iter().cloned())
        .collect()
}

fn policy<'a>(
    ix: &'a Indexer,
    allowed: &'a [reqwest::Url],
    keys: &'a [reqwest::Url],
) -> LinkPolicy<'a> {
    LinkPolicy {
        allowed,
        key: ix.cfg.api_key.as_ref().map(|k| k.expose()),
        key_origins: keys,
    }
}

fn cats_param(categories: &[u32]) -> String {
    categories
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

pub async fn search(ix: &Indexer, req: &SearchRequest) -> Result<SearchOutcome, UpstreamError> {
    let c = caps(ix).await?;
    if !c.search {
        return Err(UpstreamError::BadResponse {
            reason: "the endpoint's caps say t=search is not available".into(),
        });
    }
    let limit = c
        .limit_max
        .map_or(ix.cfg.page_size, |m| ix.cfg.page_size.min(m.max(1)));
    let allowed = allowed_origins(ix);
    let keys = key_origins(ix);
    let link_policy = policy(ix, &allowed, &keys);
    let mut seen = HashSet::new();
    let mut releases = vec![];
    let (mut raw, mut pages, mut dropped) = (0usize, 0u32, 0usize);
    let mut partial_error = None;
    let mut truncated = false;
    let mut offset: u64 = 0;
    for page in 0..ix.cfg.max_pages {
        let mut params = vec![("t", "search".to_string()), ("q", req.query.clone())];
        if !req.categories.is_empty() {
            params.push(("cat", cats_param(&req.categories)));
        }
        params.push(("limit", limit.to_string()));
        params.push(("offset", offset.to_string()));
        let parsed = match api_get(ix, &params, true).await {
            Ok(body) => parse_feed(&body, &link_policy, ix.cfg.id, Some(&ix.cfg.name))
                .map_err(|e| e.upstream()),
            Err(e) => Err(e),
        };
        let p = match parsed {
            Ok(p) => p,
            Err(e) if page == 0 => return Err(e),
            Err(e) => {
                partial_error = Some(e);
                break;
            }
        };
        pages += 1;
        raw += p.raw;
        dropped += p.links_dropped;
        let mut new = 0;
        for r in p.items {
            if seen.insert(r.dedupe_key()) {
                releases.push(r);
                new += 1;
            }
        }
        offset += p.raw as u64;
        let more = p.raw >= limit as usize && new > 0 && p.total.is_none_or(|t| offset < t);
        if !more {
            break;
        }
        if page + 1 == ix.cfg.max_pages {
            truncated = true;
        }
    }
    Ok(SearchOutcome {
        confirmed_empty: raw == 0 && partial_error.is_none(),
        releases: Arc::new(releases),
        pages_fetched: pages,
        raw_count: raw,
        partial_error,
        truncated_by_page_limit: truncated,
        fetched_at: OffsetDateTime::now_utc(),
        from_cache: false,
        dropped,
    })
}

pub async fn feed(
    ix: &Indexer,
    categories: &[u32],
    limit: u32,
) -> Result<FeedWindow, UpstreamError> {
    let c = caps(ix).await?;
    if !c.search {
        return Err(UpstreamError::BadResponse {
            reason: "the endpoint's caps say t=search is not available".into(),
        });
    }
    let limit = c.limit_max.map_or(limit, |m| limit.min(m.max(1)));
    let mut params = vec![("t", "search".to_string())];
    if !categories.is_empty() {
        params.push(("cat", cats_param(categories)));
    }
    params.push(("limit", limit.to_string()));
    let body = api_get(ix, &params, true).await?;
    let (allowed, keys) = (allowed_origins(ix), key_origins(ix));
    let p = parse_feed(
        &body,
        &policy(ix, &allowed, &keys),
        ix.cfg.id,
        Some(&ix.cfg.name),
    )
    .map_err(|e| e.upstream())?;
    let dates = p.items.iter().filter_map(|i| i.publish_date.clone());
    let (oldest, newest) = (dates.clone().min(), dates.max());
    Ok(FeedWindow {
        window: p.raw,
        oldest,
        newest,
        links_dropped: p.links_dropped,
        dropped: 0,
        truncated: p.truncated,
        items: p.items,
    })
}

/// Percent-decodes `s` (and `+` as a space), lossily, for comparisons only.
fn percent_decoded(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (
            b[i],
            b.get(i + 1).copied().and_then(hex),
            b.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(h), Some(l)) => {
                out.push(h << 4 | l);
                i += 3;
            }
            (b'+', _, _) => {
                out.push(b' ');
                i += 1;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Checks one hop of a download: http(s), no URL credentials, no fragment, an allowed origin,
/// and no key on an origin that may not receive it.
fn check_hop(ix: &Indexer, url: &reqwest::Url) -> Result<(), UpstreamError> {
    let bad = |reason: &str| UpstreamError::BadResponse {
        reason: reason.into(),
    };
    if !matches!(url.scheme(), "http" | "https") {
        return Err(bad("download link is not http(s)"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad("download link carries credentials"));
    }
    if url.fragment().is_some() {
        return Err(bad("download link has a fragment"));
    }
    let home = url.origin() == ix.cfg.base_url.origin();
    if !home
        && !ix
            .cfg
            .download_origins
            .iter()
            .any(|o| o.origin() == url.origin())
    {
        return Err(bad(
            "download link is not on the indexer's origin or a configured download origin",
        ));
    }
    let key = ix.cfg.api_key.as_ref().map(|k| k.expose());
    if !home && leaks_key(key, &key_origins(ix), [url.as_str()]) {
        return Err(bad(
            "download link would carry the indexer's API key to another origin",
        ));
    }
    Ok(())
}

/// A same-origin download link the endpoint signed without its key gets the key (the spec's
/// `apikey` parameter); links on any other origin never do.
fn with_key(ix: &Indexer, mut url: reqwest::Url) -> reqwest::Url {
    if url.origin() == ix.cfg.base_url.origin()
        && let Some(k) = &ix.cfg.api_key
        && !url
            .query_pairs()
            .any(|(n, _)| n.eq_ignore_ascii_case("apikey"))
    {
        url.query_pairs_mut().append_pair("apikey", k.expose());
    }
    url
}

/// Resolves one of this endpoint's download links to a validated torrent or magnet whose
/// trackers and web seeds do not carry the key to another origin.
pub async fn fetch(ix: &Indexer, url: &str) -> Result<Payload, UpstreamError> {
    let bad = |reason: &str| UpstreamError::BadResponse {
        reason: reason.into(),
    };
    let key = ix.cfg.api_key.as_ref().map(|k| k.expose());
    let keys = key_origins(ix);
    let leak = || {
        bad(
            "the payload's trackers or web seeds would carry the indexer's API key to another origin (add that origin to key_origins only if the tracker is meant to receive it)",
        )
    };
    let mut target = reqwest::Url::parse(url).map_err(|_| bad("download link is not a URL"))?;
    for hop in 0..2 {
        check_hop(ix, &target)?;
        let resp = ix
            .send(
                ix.http()
                    .get(with_key(ix, target.clone()))
                    .header("Accept", "application/x-bittorrent"),
                false,
            )
            .await?;
        if resp.status().is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| bad("redirect without a location"))?
                .to_string();
            if location.starts_with("magnet:?") {
                crate::torrent::magnet_hash(&location)
                    .map_err(|_| bad("redirect to an invalid magnet"))?;
                let values = magnet_values(&location);
                if leaks_key(key, &keys, values.iter().map(String::as_str)) {
                    return Err(leak());
                }
                return Ok(Payload::Magnet(location));
            }
            if hop == 0 {
                target = target
                    .join(&location)
                    .map_err(|_| bad("invalid redirect"))?;
                continue;
            }
            return Err(bad("too many redirects"));
        }
        let bytes = upstream::read_bytes(resp, MAX_TORRENT_BYTES).await?;
        crate::torrent::info_hash(&bytes).map_err(|_| bad("download is not a valid .torrent"))?;
        let urls = torrent_urls(&bytes);
        if leaks_key(key, &keys, urls.iter().map(String::as_str)) {
            return Err(leak());
        }
        return Ok(Payload::Torrent(bytes));
    }
    Err(bad("too many redirects"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_and_errors() {
        let caps = br#"<?xml version="1.0" encoding="UTF-8"?>
<caps>
  <server version="1.0" title="Example" />
  <limits max="100" default="50" />
  <searching>
    <search available="yes" supportedParams="q" />
    <tv-search available="yes" supportedParams="q,season,ep" />
    <movie-search available="no" supportedParams="q" />
  </searching>
  <categories>
    <category id="2000" name="Movies"><subcat id="2040" name="Movies/HD" /></category>
    <category id="5000" name="TV"><subcat id="5040" name="TV/HD" /></category>
  </categories>
</caps>"#;
        let c = parse_caps(caps).unwrap();
        assert!(c.search && c.tv_search && !c.movie_search);
        assert_eq!((c.limit_max, c.limit_default), (Some(100), Some(50)));
        assert_eq!(c.categories, [2000, 2040, 5000, 5040]);
        let err = br#"<error code="100" description="Incorrect user credentials: apikey=SECRET"/>"#;
        let e = parse_caps(err).unwrap_err();
        assert_eq!(e, ParseError::Torznab("100".into()));
        assert_eq!(e.upstream(), UpstreamError::Auth { status: 401 });
        assert!(!e.message().contains("SECRET"));
        let none = LinkPolicy::origins(&[]);
        let limit =
            parse_feed(br#"<error code="500" description="x"/>"#, &none, 1, None).unwrap_err();
        assert!(matches!(limit.upstream(), UpstreamError::Throttled { .. }));
        assert!(matches!(
            parse_feed(br#"<error code="201"/>"#, &none, 1, None)
                .unwrap_err()
                .upstream(),
            UpstreamError::BadResponse { .. }
        ));
        assert!(parse_caps(b"<rss/>").is_err());
    }

    #[test]
    fn paging_attributes_and_origins() {
        let feed = br#"<?xml version="1.0"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel>
  <torznab:response offset="100" total="250" />
  <item><title>A 2026 1080p WEB-DL</title><guid>a</guid><link>https://dl.example/get/1.torrent</link>
    <torznab:attr name="seeders" value="5"/><torznab:attr name="leechers" value="2"/></item>
  <item><title>B 2026 1080p WEB-DL</title><guid>b</guid><link>https://api.example/dl?id=2</link></item>
  <item><title>C 2026 1080p WEB-DL</title><guid>c</guid><link>https://user:pw@api.example/dl?id=3</link></item>
</channel></rss>"#;
        let api = reqwest::Url::parse("https://api.example/").unwrap();
        let p = parse_feed(
            feed,
            &LinkPolicy::origins(std::slice::from_ref(&api)),
            4,
            Some("Example"),
        )
        .unwrap();
        assert_eq!((p.offset, p.total, p.raw), (Some(100), Some(250), 3));
        assert!(p.items[0].download_url.is_none(), "foreign origin dropped");
        assert_eq!(p.items[0].leechers, Some(2));
        assert_eq!(p.items[0].indexer.as_deref(), Some("Example"));
        assert!(p.items[1].download_url.is_some());
        assert!(p.items[2].download_url.is_none(), "URL credentials dropped");
        assert_eq!(p.links_dropped, 2);
        let dl = reqwest::Url::parse("https://dl.example/").unwrap();
        let p2 = parse_feed(feed, &LinkPolicy::origins(&[api, dl]), 4, None).unwrap();
        assert!(
            p2.items[0].download_url.is_some(),
            "configured download origin kept"
        );
        assert_eq!(
            percent_decoded("/a%20b/KEY%2Fx"),
            "/a b/KEY/x",
            "paths are compared decoded"
        );
    }

    #[test]
    fn the_api_key_never_reaches_another_origin() {
        const KEY: &str = "FAKE-TORZNAB-KEY-0001";
        let hash = "c12fe1c06bba254a9dc9f519b335aa7c1367a88a";
        let api = reqwest::Url::parse("https://api.example/").unwrap();
        let dl = reqwest::Url::parse("https://dl.example/").unwrap();
        let tracker = reqwest::Url::parse("https://tracker.example/").unwrap();
        let allowed = [api.clone(), dl.clone()];
        let only_api = [api.clone()];
        let item = |link: &str| {
            format!(
                r#"<rss><channel><item><title>A 2026 1080p WEB-DL</title><link>{}</link><attr xmlns="http://torznab.com/schemas/2015/feed" name="minimumratio" value="1.5"/><attr xmlns="http://torznab.com/schemas/2015/feed" name="minimumseedtime" value="259200"/><attr xmlns="http://torznab.com/schemas/2015/feed" name="downloadvolumefactor" value="0"/></item></channel></rss>"#,
                link.replace('&', "&amp;")
            )
        };
        let policy = LinkPolicy {
            allowed: &allowed,
            key: Some(KEY),
            key_origins: &only_api,
        };
        let parsed =
            |link: &str, p: &LinkPolicy| parse_feed(item(link).as_bytes(), p, 9, None).unwrap();
        // A magnet whose tracker carries the key to a foreign origin is dropped.
        let leaky = format!(
            "magnet:?xt=urn:btih:{hash}&tr=https%3A%2F%2Ftracker.example%2Fannounce%3Fpasskey%3D{KEY}"
        );
        let p = parsed(&leaky, &policy);
        assert!(p.items[0].magnet_url.is_none() && p.links_dropped == 1);
        // ... unless the operator named that origin in key_origins.
        let both = [api.clone(), tracker.clone()];
        let p = parsed(
            &leaky,
            &LinkPolicy {
                allowed: &allowed,
                key: Some(KEY),
                key_origins: &both,
            },
        );
        assert!(p.items[0].magnet_url.is_some());
        // Ordinary trackers are untouched (nothing is stripped).
        let public = format!("magnet:?xt=urn:btih:{hash}&tr=udp%3A%2F%2Fopen.example%3A1337");
        assert!(parsed(&public, &policy).items[0].magnet_url.is_some());
        // A key in an HTTP link on a non-key origin is dropped; on the endpoint it is kept.
        assert!(
            parsed(
                &format!("https://dl.example/get?apikey={KEY}&id=1"),
                &policy
            )
            .items[0]
                .download_url
                .is_none()
        );
        assert!(
            parsed(
                &format!("https://api.example/get?apikey={KEY}&id=1"),
                &policy
            )
            .items[0]
                .download_url
                .is_some()
        );
        // Tracker seed requirements and volume factors are read, not ignored.
        let p = parsed(&public, &policy);
        assert_eq!(p.items[0].minimum_ratio, Some(1.5));
        assert_eq!(p.items[0].minimum_seed_time, Some(259_200));
        assert_eq!(p.items[0].download_volume_factor, Some(0.0));
        // .torrent announce and web-seed URLs are checked the same way.
        let mut t = b"d8:announce".to_vec();
        let ann = format!("https://tracker.example/announce?passkey={KEY}");
        t.extend_from_slice(format!("{}:{ann}", ann.len()).as_bytes());
        t.extend_from_slice(
            b"4:infod6:lengthi1e4:name1:a12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee",
        );
        let urls = torrent_urls(&t);
        assert_eq!(urls, std::slice::from_ref(&ann));
        assert!(leaks_key(
            Some(KEY),
            &only_api,
            urls.iter().map(String::as_str)
        ));
        assert!(!leaks_key(
            Some(KEY),
            &both,
            urls.iter().map(String::as_str)
        ));
        assert!(!leaks_key(None, &only_api, urls.iter().map(String::as_str)));
        assert!(
            !format!("{:?}", p.items[0]).contains(KEY),
            "links stay redacted"
        );
    }
}
