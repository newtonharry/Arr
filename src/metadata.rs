//! External metadata: TVmaze (free, no key) for TV; Plex's own metadata service (with the
//! Plex sign-in) and, optionally, TMDB (operator credential) for movies.
//! Both are read-only GETs to the configured base URL; data is cached briefly.

use crate::config::{MetadataConfig, PlexConfig, Secret};
use crate::upstream::{self, TtlCache, UpstreamError};
use reqwest::Url;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;
const METADATA_TTL: Duration = Duration::from_secs(3600);
/// Alternative titles read from one TVmaze AKA list at most.
const MAX_AKAS: usize = 50;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TvmazeExternals {
    #[serde(default)]
    pub thetvdb: Option<u32>,
    #[serde(default)]
    pub imdb: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TvmazeShow {
    pub id: u32,
    pub name: String,
    #[serde(default)]
    pub premiered: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub runtime: Option<u32>,
    #[serde(default)]
    pub average_runtime: Option<u32>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub externals: TvmazeExternals,
    #[serde(default)]
    pub image: Option<TvmazeImage>,
    /// HTML; read it with `plain_summary`.
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub network: Option<TvmazeNetwork>,
    #[serde(default, rename = "webChannel")]
    pub web_channel: Option<TvmazeNetwork>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TvmazeNetwork {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TvmazeImage {
    #[serde(default)]
    pub medium: Option<String>,
}

impl TvmazeShow {
    pub fn year(&self) -> Option<u16> {
        self.premiered
            .as_deref()
            .and_then(|d| d.get(..4))
            .and_then(|y| y.parse().ok())
    }

    /// The network or streaming service that shows it.
    pub fn network_name(&self) -> Option<String> {
        [&self.network, &self.web_channel]
            .into_iter()
            .flatten()
            .find_map(|n| n.name.clone().filter(|n| !n.trim().is_empty()))
    }
}

/// One entry of a show's official alternative titles (`/shows/{id}/akas`).
#[derive(Debug, Clone, Deserialize)]
struct TvmazeAka {
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TvmazeEpisode {
    #[serde(default)]
    pub season: Option<u32>,
    #[serde(default)]
    pub number: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub airstamp: Option<String>,
    #[serde(default)]
    pub runtime: Option<u32>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

#[derive(Deserialize)]
struct SearchHit {
    show: TvmazeShow,
}

/// A popular show with a new episode this week (TVmaze web schedule).
#[derive(Debug, Clone)]
pub struct WebShow {
    pub id: u32,
    pub tvdb: Option<u32>,
    pub name: String,
    pub year: Option<u16>,
    pub network: Option<String>,
    pub summary: Option<String>,
}

#[derive(Deserialize)]
struct ScheduleEntry {
    #[serde(default, rename = "_embedded")]
    embedded: Option<ScheduleEmbedded>,
}

#[derive(Deserialize)]
struct ScheduleEmbedded {
    show: Option<ScheduleShow>,
}

#[derive(Deserialize)]
struct ScheduleShow {
    #[serde(flatten)]
    show: TvmazeShow,
    #[serde(default)]
    weight: Option<f64>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    network: Option<ScheduleNetwork>,
    #[serde(default, rename = "webChannel")]
    web_channel: Option<ScheduleNetwork>,
}

#[derive(Deserialize)]
struct ScheduleNetwork {
    #[serde(default)]
    name: Option<String>,
}

/// Plain text of a TVmaze summary (HTML), at most `max` characters.
pub fn plain_summary(html: &str, max: usize) -> Option<String> {
    let mut text = String::new();
    let mut tag = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let name = tag.trim_start_matches('/').to_ascii_lowercase();
                if ["p", "br", "li", "div"].iter().any(|b| {
                    name.strip_prefix(b)
                        .is_some_and(|r| !r.starts_with(char::is_alphanumeric))
                }) {
                    text.push(' ');
                }
            }
            c if in_tag => tag.push(c),
            c => text.push(c),
        }
    }
    let text = text
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        None
    } else if text.chars().count() <= max {
        Some(text)
    } else {
        let cut: String = text.chars().take(max - 1).collect();
        Some(format!("{}\u{2026}", cut.trim_end()))
    }
}

const IDEAS_TTL: Duration = Duration::from_secs(6 * 3600);
const IDEAS_MAX: usize = 24;

pub struct Tvmaze {
    base: Url,
    http: reqwest::Client,
    shows: TtlCache<String, Option<TvmazeShow>>,
    episodes: TtlCache<u32, Vec<TvmazeEpisode>>,
    ideas: Mutex<Option<(Instant, Vec<WebShow>)>>,
}

impl Tvmaze {
    pub fn new(cfg: &MetadataConfig) -> Self {
        Tvmaze {
            base: cfg.base_url.clone(),
            http: upstream::http_client(cfg.timeout, reqwest::redirect::Policy::none()),
            shows: TtlCache::new(METADATA_TTL, 512),
            episodes: TtlCache::new(METADATA_TTL, 128),
            ideas: Mutex::new(None),
        }
    }

    fn url(&self, path: &str, query: &[(&str, &str)]) -> Result<Url, UpstreamError> {
        let mut url = self
            .base
            .join(path)
            .map_err(|_| UpstreamError::BadResponse {
                reason: "invalid path".into(),
            })?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    /// GET with one polite retry on 429 and same-origin redirects only (lookup endpoint).
    async fn get<T: DeserializeOwned>(&self, url: Url) -> Result<T, UpstreamError> {
        let mut url = url;
        let mut retried = false;
        for _ in 0..3 {
            let resp = upstream::send(
                self.http
                    .get(url.clone())
                    .header("Accept", "application/json"),
            )
            .await?;
            let status = resp.status();
            if status.is_redirection() {
                let next = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|loc| url.join(loc).ok())
                    .filter(|u| u.origin() == self.base.origin());
                match next {
                    Some(n) => {
                        url = n;
                        continue;
                    }
                    None => {
                        return Err(UpstreamError::BadResponse {
                            reason: "redirect to an unexpected location".into(),
                        });
                    }
                }
            }
            if status.as_u16() == 429 && !retried {
                retried = true;
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            return upstream::read_json(resp, MAX_METADATA_BYTES).await;
        }
        Err(UpstreamError::BadResponse {
            reason: "too many redirects".into(),
        })
    }

    /// Popular shows with an episode on the web schedule in the last seven days, most popular
    /// first (cached for six hours; a failed refresh serves the cached answer).
    pub async fn ideas(&self) -> Result<Vec<WebShow>, UpstreamError> {
        let cached = self.ideas.lock().unwrap().clone();
        if let Some((at, shows)) = &cached
            && at.elapsed() < IDEAS_TTL
        {
            return Ok(shows.clone());
        }
        let today = time::OffsetDateTime::now_utc().date();
        let fmt = time::macros::format_description!("[year]-[month]-[day]");
        let mut ranked: Vec<(f64, WebShow)> = vec![];
        let mut last_err = None;
        let mut answered = false;
        for back in 0..7 {
            let day = today - time::Duration::days(back);
            let date = day.format(&fmt).unwrap_or_default();
            let entries: Vec<ScheduleEntry> = match self
                .get(self.url("schedule/web", &[("date", &date)])?)
                .await
            {
                Ok(e) => e,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            answered = true;
            for show in entries
                .into_iter()
                .filter_map(|e| e.embedded?.show)
                .filter(|s| !s.show.name.trim().is_empty())
            {
                if ranked.iter().any(|(_, r)| r.id == show.show.id) {
                    continue;
                }
                let network = show
                    .network
                    .or(show.web_channel)
                    .and_then(|n| n.name)
                    .filter(|n| !n.is_empty());
                ranked.push((
                    show.weight.unwrap_or(0.0),
                    WebShow {
                        id: show.show.id,
                        tvdb: show.show.externals.thetvdb,
                        name: show.show.name.trim().to_string(),
                        year: show.show.year(),
                        network,
                        summary: show.summary.as_deref().and_then(|s| plain_summary(s, 200)),
                    },
                ));
            }
        }
        if !answered {
            return match cached {
                Some((_, shows)) => Ok(shows),
                None => Err(last_err.unwrap_or(UpstreamError::BadResponse {
                    reason: "no schedule".into(),
                })),
            };
        }
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
        let shows: Vec<WebShow> = ranked.into_iter().map(|(_, s)| s).take(IDEAS_MAX).collect();
        *self.ideas.lock().unwrap() = Some((Instant::now(), shows.clone()));
        Ok(shows)
    }

    pub async fn search(&self, query: &str) -> Result<Vec<TvmazeShow>, UpstreamError> {
        let hits: Vec<SearchHit> = self.get(self.url("search/shows", &[("q", query)])?).await?;
        Ok(hits.into_iter().map(|h| h.show).collect())
    }

    async fn cached_show(
        &self,
        key: String,
        url: Url,
        fresh: bool,
    ) -> Result<Option<TvmazeShow>, UpstreamError> {
        if !fresh && let Some(hit) = self.shows.get(&key) {
            return Ok(hit);
        }
        let show = match self.get::<TvmazeShow>(url).await {
            Ok(s) => Some(s),
            Err(UpstreamError::NotFound) => None,
            Err(e) => return Err(e),
        };
        self.shows.insert(key, show.clone());
        Ok(show)
    }

    pub async fn by_tvdb(&self, tvdb: u32) -> Result<Option<TvmazeShow>, UpstreamError> {
        self.by_tvdb_with(tvdb, false).await
    }

    /// `fresh` bypasses the cache (an operator-requested refresh must not see stale data).
    pub async fn by_tvdb_with(
        &self,
        tvdb: u32,
        fresh: bool,
    ) -> Result<Option<TvmazeShow>, UpstreamError> {
        let id = tvdb.to_string();
        let url = self.url("lookup/shows", &[("thetvdb", &id)])?;
        self.cached_show(format!("tvdb:{tvdb}"), url, fresh).await
    }

    pub async fn by_id(&self, id: u32) -> Result<Option<TvmazeShow>, UpstreamError> {
        self.by_id_with(id, false).await
    }

    pub async fn by_id_with(
        &self,
        id: u32,
        fresh: bool,
    ) -> Result<Option<TvmazeShow>, UpstreamError> {
        let url = self.url(&format!("shows/{id}"), &[])?;
        self.cached_show(format!("id:{id}"), url, fresh).await
    }

    /// The show's official alternative titles (TVmaze AKAs), exactly as listed: at most
    /// `MAX_AKAS`, without empty, overlong or control-character names. A show TVmaze does not
    /// know has none.
    pub async fn akas(&self, show_id: u32) -> Result<Vec<String>, UpstreamError> {
        let akas: Vec<TvmazeAka> = match self
            .get(self.url(&format!("shows/{show_id}/akas"), &[])?)
            .await
        {
            Ok(a) => a,
            Err(UpstreamError::NotFound) => vec![],
            Err(e) => return Err(e),
        };
        let mut out: Vec<String> = vec![];
        for name in akas.into_iter().map(|a| a.name.trim().to_string()) {
            if !name.is_empty()
                && name.chars().count() <= 200
                && !name.chars().any(char::is_control)
                && !out.contains(&name)
            {
                out.push(name);
            }
            if out.len() >= MAX_AKAS {
                break;
            }
        }
        Ok(out)
    }

    /// Regular (non-special) episodes in airing order.
    pub async fn episodes(&self, show_id: u32) -> Result<Vec<TvmazeEpisode>, UpstreamError> {
        self.episodes_with(show_id, false).await
    }

    pub async fn episodes_with(
        &self,
        show_id: u32,
        fresh: bool,
    ) -> Result<Vec<TvmazeEpisode>, UpstreamError> {
        if !fresh && let Some(hit) = self.episodes.get(&show_id) {
            return Ok(hit);
        }
        let eps: Vec<TvmazeEpisode> = self
            .get(self.url(&format!("shows/{show_id}/episodes"), &[])?)
            .await?;
        let eps: Vec<TvmazeEpisode> = eps
            .into_iter()
            .filter(|e| e.number.is_some() && e.kind.as_deref().is_none_or(|k| k == "regular"))
            .collect();
        self.episodes.insert(show_id, eps.clone());
        Ok(eps)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TmdbSearchResult {
    pub id: u32,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub original_title: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub original_language: Option<String>,
}

#[derive(Deserialize)]
struct TmdbSearchPage {
    #[serde(default)]
    results: Vec<TmdbSearchResult>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TmdbAltTitles {
    #[serde(default)]
    pub titles: Vec<TmdbAltTitle>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TmdbAltTitle {
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TmdbMovie {
    pub id: u32,
    pub title: String,
    #[serde(default)]
    pub original_title: Option<String>,
    #[serde(default)]
    pub original_language: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub runtime: Option<u32>,
    #[serde(default)]
    pub imdb_id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub alternative_titles: TmdbAltTitles,
    #[serde(default)]
    pub release_dates: TmdbReleaseDates,
    #[serde(default)]
    pub poster_path: Option<String>,
    /// A plain-text summary (TMDB's overview, or Plex's summary).
    #[serde(default)]
    pub overview: Option<String>,
    /// Plex's poster address (Plex answers only; never read from TMDB).
    #[serde(default, skip_deserializing)]
    pub plex_thumb: Option<String>,
    /// The only digital date is "Plex shows it for sale or streaming today", not a published
    /// date: a refresh keeps an earlier recorded date instead of moving it forward.
    #[serde(default, skip_deserializing)]
    pub observed_digital: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TmdbReleaseDates {
    #[serde(default)]
    pub results: Vec<TmdbCountryReleases>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TmdbCountryReleases {
    #[serde(default)]
    pub release_dates: Vec<TmdbReleaseDate>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TmdbReleaseDate {
    /// 1 premiere, 2 limited theatrical, 3 theatrical, 4 digital, 5 physical, 6 TV.
    #[serde(default, rename = "type")]
    pub kind: i64,
    #[serde(default)]
    pub release_date: Option<String>,
}

impl TmdbMovie {
    /// Release evidence: TMDB's primary release date as in-cinemas, and the earliest
    /// digital (type 4) and physical (type 5) releases across countries. Nothing is invented.
    pub fn release(&self) -> crate::catalog::ReleaseDates {
        let earliest = |kind: i64| {
            self.release_dates
                .results
                .iter()
                .flat_map(|c| &c.release_dates)
                .filter(|d| d.kind == kind)
                .filter_map(|d| provider_date(d.release_date.as_deref()))
                .min()
        };
        crate::catalog::ReleaseDates {
            in_cinemas: provider_date(self.release_date.as_deref()),
            physical_release: earliest(5),
            digital_release: earliest(4),
        }
    }
}

/// The `YYYY-MM-DD` date at the start of a provider value, if it is a real date. Malformed
/// values (wrong shape, impossible dates, non-ASCII) are unknown evidence, never a panic.
fn provider_date(v: Option<&str>) -> Option<String> {
    let d = v?.trim().get(..10)?;
    time::Date::parse(d, time::macros::format_description!("[year]-[month]-[day]")).ok()?;
    Some(d.to_string())
}

pub fn year_of(date: Option<&str>) -> Option<u16> {
    date.and_then(|d| d.get(..4)).and_then(|y| y.parse().ok())
}

/// ISO 639-1 -> the English language names used by the release parser.
pub fn language_name(code: &str) -> Option<&'static str> {
    Some(match code {
        "en" => "english",
        "fr" => "french",
        "de" => "german",
        "es" => "spanish",
        "it" => "italian",
        "ru" => "russian",
        "uk" => "ukrainian",
        "pl" => "polish",
        "pt" => "portuguese",
        "nl" => "dutch",
        "sv" => "swedish",
        "no" | "nb" | "nn" => "norwegian",
        "da" => "danish",
        "fi" => "finnish",
        "is" => "icelandic",
        "cs" => "czech",
        "hu" => "hungarian",
        "el" => "greek",
        "tr" => "turkish",
        "ar" => "arabic",
        "he" => "hebrew",
        "fa" => "persian",
        "hi" => "hindi",
        "ta" => "tamil",
        "te" => "telugu",
        "ml" => "malayalam",
        "kn" => "kannada",
        "bn" => "bengali",
        "ja" => "japanese",
        "ko" => "korean",
        "zh" | "cn" => "chinese",
        "th" => "thai",
        "vi" => "vietnamese",
        "ro" => "romanian",
        "bg" => "bulgarian",
        _ => return None,
    })
}

pub struct Tmdb {
    base: Url,
    http: reqwest::Client,
    credential: crate::config::Secret,
    movies: TtlCache<u32, Option<TmdbMovie>>,
}

impl Tmdb {
    pub fn new(cfg: &MetadataConfig) -> Option<Self> {
        Some(Tmdb {
            base: cfg.base_url.clone(),
            http: upstream::http_client(cfg.timeout, reqwest::redirect::Policy::none()),
            credential: cfg.credential.clone()?,
            movies: TtlCache::new(METADATA_TTL, 512),
        })
    }

    /// v4 read-access tokens (JWT, `eyJ...`) go in a Bearer header; v3 keys use `api_key`
    /// (the URL is never logged: upstream errors are built `without_url`).
    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, UpstreamError> {
        let mut url = self
            .base
            .join(path)
            .map_err(|_| UpstreamError::BadResponse {
                reason: "invalid path".into(),
            })?;
        let bearer = self.credential.expose().starts_with("eyJ");
        {
            let mut q = url.query_pairs_mut();
            q.extend_pairs(query);
            if !bearer {
                q.append_pair("api_key", self.credential.expose());
            }
        }
        let mut req = self.http.get(url).header("Accept", "application/json");
        if bearer {
            req = req.bearer_auth(self.credential.expose());
        }
        upstream::read_json(upstream::send(req).await?, MAX_METADATA_BYTES).await
    }

    pub async fn search(
        &self,
        query: &str,
        year: Option<u16>,
    ) -> Result<Vec<TmdbSearchResult>, UpstreamError> {
        let year = year.map(|y| y.to_string());
        let mut q = vec![("query", query), ("include_adult", "false")];
        if let Some(y) = &year {
            q.push(("year", y));
        }
        let page: TmdbSearchPage = self.get("3/search/movie", &q).await?;
        Ok(page.results)
    }

    /// The TMDB movie id for an IMDB id (`GET 3/find/{imdb}?external_source=imdb_id`).
    pub async fn find_imdb(&self, imdb: &str) -> Result<Option<u32>, UpstreamError> {
        if !(imdb.starts_with("tt")
            && imdb.len() <= 12
            && imdb[2..].bytes().all(|b| b.is_ascii_digit()))
        {
            return Ok(None);
        }
        #[derive(Deserialize)]
        struct Found {
            #[serde(default)]
            movie_results: Vec<FoundMovie>,
        }
        #[derive(Deserialize)]
        struct FoundMovie {
            id: u32,
        }
        let found: Found = self
            .get(&format!("3/find/{imdb}"), &[("external_source", "imdb_id")])
            .await?;
        Ok(match found.movie_results.as_slice() {
            [only] => Some(only.id),
            _ => None,
        })
    }

    pub async fn movie(&self, id: u32) -> Result<Option<TmdbMovie>, UpstreamError> {
        self.movie_with(id, false).await
    }

    /// `fresh` bypasses the cache (metadata refresh).
    pub async fn movie_with(
        &self,
        id: u32,
        fresh: bool,
    ) -> Result<Option<TmdbMovie>, UpstreamError> {
        if !fresh && let Some(hit) = self.movies.get(&id) {
            return Ok(hit);
        }
        let movie = match self
            .get::<TmdbMovie>(
                &format!("3/movie/{id}"),
                &[("append_to_response", "alternative_titles,release_dates")],
            )
            .await
        {
            Ok(m) => Some(m),
            Err(UpstreamError::NotFound) => None,
            Err(e) => return Err(e),
        };
        self.movies.insert(id, movie.clone());
        Ok(movie)
    }
}

/// Most Plex search hits looked up in full (each needs a details call for its TMDB id).
const PLEX_SEARCH_MAX: usize = 10;
/// Details calls in flight at once for one search.
const PLEX_LOOKUPS: usize = 4;

#[derive(Deserialize)]
struct PlexEnvelope<T> {
    #[serde(rename = "MediaContainer")]
    container: T,
}

#[derive(Deserialize, Default)]
struct PlexSearchContainer {
    #[serde(rename = "SearchResults", default)]
    groups: Vec<PlexSearchGroup>,
}

#[derive(Deserialize)]
struct PlexSearchGroup {
    #[serde(rename = "SearchResult", default)]
    hits: Vec<PlexSearchHit>,
}

#[derive(Deserialize)]
struct PlexSearchHit {
    #[serde(rename = "Metadata")]
    metadata: Option<PlexMeta>,
}

#[derive(Deserialize, Default)]
struct PlexMetaContainer {
    #[serde(rename = "Metadata", default)]
    items: Vec<PlexMeta>,
}

#[derive(Deserialize, Default)]
struct PlexOffers {
    #[serde(rename = "Availability", default)]
    offers: Vec<serde::de::IgnoredAny>,
}

#[derive(Debug, Clone, Deserialize)]
struct PlexGuid {
    id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PlexCountry {
    tag: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PlexMeta {
    #[serde(default)]
    title: String,
    #[serde(rename = "originalTitle", default)]
    original_title: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    year: Option<u16>,
    #[serde(rename = "ratingKey", default)]
    rating_key: String,
    /// `plex://movie/<key>`.
    #[serde(default)]
    guid: Option<String>,
    #[serde(rename = "originallyAvailableAt", default)]
    originally_available_at: Option<String>,
    /// Milliseconds.
    #[serde(default)]
    duration: Option<u64>,
    #[serde(rename = "Guid", default)]
    guids: Vec<PlexGuid>,
    #[serde(rename = "Country", default)]
    countries: Vec<PlexCountry>,
    #[serde(default)]
    thumb: Option<String>,
    #[serde(default)]
    summary: Option<String>,
}

impl PlexMeta {
    fn guid(&self, scheme: &str) -> Option<&str> {
        self.guids
            .iter()
            .find_map(|g| g.id.strip_prefix(scheme)?.strip_prefix("://"))
    }

    /// The key Plex's other endpoints use for this movie.
    fn key(&self) -> &str {
        if self.rating_key.is_empty() {
            self.guid
                .as_deref()
                .and_then(|g| g.strip_prefix("plex://movie/"))
                .unwrap_or("")
        } else {
            &self.rating_key
        }
    }

    fn tmdb_id(&self) -> Option<u32> {
        self.guid("tmdb")?.parse().ok().filter(|id| *id > 0)
    }
}

/// The original-language code for the first production country Plex lists. Only common
/// countries with one dominant language are mapped; the rest are unknown, never guessed.
fn language_of_country(country: &str) -> Option<&'static str> {
    Some(match country {
        "United States of America"
        | "United Kingdom"
        | "Canada"
        | "Australia"
        | "Ireland"
        | "New Zealand" => "en",
        "France" => "fr",
        "Germany" | "Austria" => "de",
        "Spain" | "Mexico" | "Argentina" => "es",
        "Italy" => "it",
        "Japan" => "ja",
        "South Korea" => "ko",
        "China" | "Taiwan" => "zh",
        // India (Hindi, Tamil, Telugu, …) and Hong Kong (mostly Cantonese) have no single language.
        "Denmark" => "da",
        "Sweden" => "sv",
        "Norway" => "no",
        "Finland" => "fi",
        "Netherlands" => "nl",
        "Brazil" | "Portugal" => "pt",
        "Russia" => "ru",
        "Poland" => "pl",
        "Turkey" => "tr",
        "Iran" => "fa",
        "Thailand" => "th",
        "Indonesia" => "id",
        "Israel" => "he",
        _ => return None,
    })
}

/// Plex's movie metadata service (the search and details Plex itself shows), read with the
/// Plex sign-in. Movies are keyed by the TMDB id in Plex's guid list; a Plex movie without
/// one is invisible to Arr.
#[derive(Clone)]
pub struct PlexMovies {
    inner: Arc<PlexInner>,
}

struct PlexInner {
    discover: Url,
    metadata: Url,
    http: reqwest::Client,
    token: Secret,
    client_id: String,
    /// Plex details by Plex key.
    metas: TtlCache<String, Option<PlexMeta>>,
    /// Finished movies by TMDB id.
    movies: TtlCache<u32, Option<TmdbMovie>>,
}

/// A Plex key is letters and digits only; anything else is never put in a path.
fn plex_key(key: &str) -> Option<&str> {
    (!key.is_empty() && key.len() <= 64 && key.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then_some(key)
}

impl PlexMovies {
    pub fn new(cfg: &PlexConfig, client_id: String) -> Self {
        PlexMovies {
            inner: Arc::new(PlexInner {
                discover: cfg.discover_url.clone(),
                metadata: cfg.metadata_url.clone(),
                http: upstream::http_client(cfg.timeout, reqwest::redirect::Policy::none()),
                token: cfg.token.clone(),
                client_id,
                metas: TtlCache::new(METADATA_TTL, 512),
                movies: TtlCache::new(METADATA_TTL, 512),
            }),
        }
    }

    async fn get<T: DeserializeOwned>(
        &self,
        base: &Url,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, UpstreamError> {
        let mut url = base.join(path).map_err(|_| UpstreamError::BadResponse {
            reason: "invalid path".into(),
        })?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let req = self
            .inner
            .http
            .get(url)
            .header("Accept", "application/json")
            .header("X-Plex-Token", self.inner.token.expose())
            .header("X-Plex-Product", "Arr")
            .header("X-Plex-Version", env!("CARGO_PKG_VERSION"))
            .header("X-Plex-Client-Identifier", &self.inner.client_id);
        let env: PlexEnvelope<T> =
            upstream::read_json(upstream::send(req).await?, MAX_METADATA_BYTES).await?;
        Ok(env.container)
    }

    /// The first item of a metadata answer; a 404 is "no such movie".
    async fn first_meta(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Option<PlexMeta>, UpstreamError> {
        match self
            .get::<PlexMetaContainer>(&self.inner.metadata, path, query)
            .await
        {
            Ok(c) => Ok(c.items.into_iter().next()),
            Err(UpstreamError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn meta_by_key(&self, key: &str) -> Result<Option<PlexMeta>, UpstreamError> {
        let Some(key) = plex_key(key) else {
            return Ok(None);
        };
        if let Some(hit) = self.inner.metas.get(&key.to_string()) {
            return Ok(hit);
        }
        let meta = self
            .first_meta(&format!("library/metadata/{key}"), &[])
            .await?;
        self.inner.metas.insert(key.to_string(), meta.clone());
        Ok(meta)
    }

    /// `guid` is `tmdb://<id>` or `imdb://tt...`.
    async fn meta_by_guid(&self, guid: &str) -> Result<Option<PlexMeta>, UpstreamError> {
        self.first_meta("library/metadata/matches", &[("type", "1"), ("guid", guid)])
            .await
    }

    /// Whether Plex lists at least one place to buy, rent or stream the movie.
    async fn offered(&self, key: &str) -> Result<bool, UpstreamError> {
        let Some(key) = plex_key(key) else {
            return Ok(false);
        };
        match self
            .get::<PlexOffers>(
                &self.inner.discover,
                &format!("library/metadata/{key}/availabilities"),
                &[],
            )
            .await
        {
            Ok(o) => Ok(!o.offers.is_empty()),
            Err(UpstreamError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub async fn search(&self, query: &str) -> Result<Vec<TmdbSearchResult>, UpstreamError> {
        let limit = PLEX_SEARCH_MAX.to_string();
        let found: PlexSearchContainer = self
            .get(
                &self.inner.discover,
                "library/search",
                &[
                    ("query", query),
                    ("searchTypes", "movies"),
                    ("searchProviders", "discover"),
                    ("includeMetadata", "1"),
                    ("limit", &limit),
                ],
            )
            .await?;
        let keys: Vec<String> = found
            .groups
            .into_iter()
            .flat_map(|g| g.hits)
            .filter_map(|h| h.metadata)
            .filter(|m| m.kind.as_deref().is_none_or(|k| k == "movie"))
            .map(|m| m.key().to_string())
            .filter(|k| plex_key(k).is_some())
            .take(PLEX_SEARCH_MAX)
            .collect();
        let mut out: Vec<Option<TmdbSearchResult>> = vec![None; keys.len()];
        let mut failure = None;
        for chunk in keys.chunks(PLEX_LOOKUPS).enumerate() {
            let mut set = tokio::task::JoinSet::new();
            for (i, key) in chunk.1.iter().enumerate() {
                let (this, key) = (self.clone(), key.clone());
                let at = chunk.0 * PLEX_LOOKUPS + i;
                set.spawn(async move { (at, this.meta_by_key(&key).await) });
            }
            while let Some(done) = set.join_next().await {
                let Ok((at, meta)) = done else { continue };
                match meta {
                    Ok(Some(m)) => {
                        out[at] = m.tmdb_id().map(|id| TmdbSearchResult {
                            id,
                            title: Some(m.title.clone()),
                            original_title: m.original_title.clone(),
                            release_date: m
                                .originally_available_at
                                .clone()
                                .or(m.year.map(|y| y.to_string())),
                            original_language: None,
                        })
                    }
                    Ok(None) => {}
                    Err(e) => failure = Some(e),
                }
            }
        }
        let results: Vec<TmdbSearchResult> = out.into_iter().flatten().collect();
        match failure {
            Some(e) if results.is_empty() => Err(e),
            _ => Ok(results),
        }
    }

    pub async fn find_imdb(&self, imdb: &str) -> Result<Option<u32>, UpstreamError> {
        if !imdb_shape(imdb) {
            return Ok(None);
        }
        Ok(self
            .meta_by_guid(&format!("imdb://{imdb}"))
            .await?
            .and_then(|m| m.tmdb_id()))
    }

    /// A movie by TMDB id. `fresh` bypasses the cache (metadata refresh).
    pub async fn movie_with(
        &self,
        id: u32,
        fresh: bool,
    ) -> Result<Option<TmdbMovie>, UpstreamError> {
        if !fresh && let Some(hit) = self.inner.movies.get(&id) {
            return Ok(hit);
        }
        let Some(meta) = self.meta_by_guid(&format!("tmdb://{id}")).await? else {
            self.inner.movies.insert(id, None);
            return Ok(None);
        };
        let key = meta.key().to_string();
        let on_sale = self.offered(&key).await?;
        let movie = Some(plex_movie(id, &meta, on_sale));
        self.inner.metas.insert(key, Some(meta));
        self.inner.movies.insert(id, movie.clone());
        Ok(movie)
    }
}

fn imdb_shape(imdb: &str) -> bool {
    imdb.starts_with("tt") && imdb.len() <= 12 && imdb[2..].bytes().all(|b| b.is_ascii_digit())
}

/// Plex's answer in the movie shape the rest of Arr reads. Nothing is invented: the cinema
/// date is Plex's own, and "released digitally" is only claimed (as today) while Plex lists
/// at least one offer.
fn plex_movie(id: u32, m: &PlexMeta, on_sale: bool) -> TmdbMovie {
    let today = time::OffsetDateTime::now_utc().date().to_string();
    let original = m
        .original_title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty() && *t != m.title);
    TmdbMovie {
        id,
        title: m.title.clone(),
        original_title: original.map(str::to_owned),
        original_language: m
            .countries
            .first()
            .and_then(|c| language_of_country(&c.tag))
            .map(str::to_owned),
        release_date: m
            .originally_available_at
            .clone()
            .or(m.year.map(|y| y.to_string())),
        runtime: m.duration.map(|d| (d / 60_000) as u32).filter(|r| *r > 0),
        imdb_id: m.guid("imdb").map(str::to_owned),
        status: None,
        alternative_titles: TmdbAltTitles {
            titles: original
                .map(|t| TmdbAltTitle {
                    title: t.to_string(),
                })
                .into_iter()
                .collect(),
        },
        release_dates: TmdbReleaseDates {
            results: if on_sale {
                vec![TmdbCountryReleases {
                    release_dates: vec![TmdbReleaseDate {
                        kind: 4,
                        release_date: Some(today),
                    }],
                }]
            } else {
                vec![]
            },
        },
        poster_path: None,
        overview: m.summary.clone().filter(|t| !t.trim().is_empty()),
        plex_thumb: m.thumb.clone(),
        observed_digital: on_sale,
    }
}

/// Where movies come from: Plex's metadata service first, TMDB (with its own key) as the
/// backup, and as the richer source for language, alternative titles and release dates when
/// both answer.
pub struct Movies {
    plex: Option<PlexMovies>,
    tmdb: Option<Tmdb>,
}

/// What each provider said to one search.
pub type Answers = Vec<(&'static str, Result<usize, UpstreamError>)>;

impl Movies {
    /// None when neither Plex nor TMDB can answer.
    pub fn new(plex: Option<PlexMovies>, tmdb: Option<Tmdb>) -> Option<Self> {
        (plex.is_some() || tmdb.is_some()).then_some(Movies { plex, tmdb })
    }

    pub fn has_tmdb(&self) -> bool {
        self.tmdb.is_some()
    }

    /// TMDB alone, for callers that must not need the network for every Plex movie.
    pub fn tmdb(&self) -> Option<&Tmdb> {
        self.tmdb.as_ref()
    }

    pub fn has_plex(&self) -> bool {
        self.plex.is_some()
    }

    /// Results from Plex, or from TMDB when Plex fails or finds nothing, with what each
    /// provider asked said.
    pub async fn search(&self, query: &str, year: Option<u16>) -> (Vec<TmdbSearchResult>, Answers) {
        let mut answers: Answers = vec![];
        if let Some(p) = &self.plex {
            // Plex searches without a year: hits from other years must not stop the TMDB fallback.
            let in_year = |r: &TmdbSearchResult| {
                year.is_none_or(|y| year_of(r.release_date.as_deref()) == Some(y))
            };
            match p
                .search(query)
                .await
                .map(|r| r.into_iter().filter(in_year).collect::<Vec<_>>())
            {
                Ok(r) if !r.is_empty() || self.tmdb.is_none() => {
                    answers.push(("plex", Ok(r.len())));
                    return (r, answers);
                }
                Ok(_) => answers.push(("plex", Ok(0))),
                Err(e) => answers.push(("plex", Err(e))),
            }
        }
        let mut results = vec![];
        if let Some(t) = &self.tmdb {
            match t.search(query, year).await {
                Ok(r) => {
                    answers.push(("tmdb", Ok(r.len())));
                    results = r;
                }
                Err(e) => answers.push(("tmdb", Err(e))),
            }
        }
        (results, answers)
    }

    pub async fn find_imdb(&self, imdb: &str) -> Result<Option<u32>, UpstreamError> {
        let mut failed = None;
        if let Some(p) = &self.plex {
            match p.find_imdb(imdb).await {
                Ok(Some(id)) => return Ok(Some(id)),
                Ok(None) => {}
                Err(e) => failed = Some(e),
            }
        }
        match &self.tmdb {
            Some(t) => t.find_imdb(imdb).await,
            None => failed.map_or(Ok(None), Err),
        }
    }

    pub async fn movie(&self, id: u32) -> Result<Option<TmdbMovie>, UpstreamError> {
        self.movie_with(id, false).await
    }

    pub async fn movie_with(
        &self,
        id: u32,
        fresh: bool,
    ) -> Result<Option<TmdbMovie>, UpstreamError> {
        let plex = match &self.plex {
            Some(p) => p.movie_with(id, fresh).await,
            None => Ok(None),
        };
        let tmdb = match &self.tmdb {
            Some(t) => t.movie_with(id, fresh).await,
            None => Ok(None),
        };
        match (plex, tmdb) {
            (Ok(Some(p)), Ok(Some(t))) => Ok(Some(merge(p, t))),
            (Ok(Some(m)), _) | (_, Ok(Some(m))) => Ok(Some(m)),
            (_, Err(e)) | (Err(e), _) => Err(e),
            (Ok(None), Ok(None)) => Ok(None),
        }
    }
}

/// TMDB's richer language, alternative titles and release dates, with Plex filling what TMDB
/// leaves out (a poster first, and "on sale today" when TMDB has no digital date).
fn merge(plex: TmdbMovie, mut tmdb: TmdbMovie) -> TmdbMovie {
    tmdb.plex_thumb = plex.plex_thumb;
    tmdb.overview = plex
        .overview
        .or(tmdb.overview.filter(|t| !t.trim().is_empty()));
    tmdb.original_title = tmdb.original_title.or(plex.original_title);
    tmdb.original_language = tmdb
        .original_language
        .filter(|l| !l.trim().is_empty())
        .or(plex.original_language);
    // TMDB sends "" for an unknown date: that must not hide Plex's real one.
    tmdb.release_date = tmdb
        .release_date
        .filter(|d| provider_date(Some(d)).is_some())
        .or(plex.release_date);
    tmdb.runtime = tmdb.runtime.filter(|r| *r > 0).or(plex.runtime);
    tmdb.imdb_id = tmdb.imdb_id.or(plex.imdb_id);
    if tmdb.alternative_titles.titles.is_empty() {
        tmdb.alternative_titles = plex.alternative_titles;
    }
    if plex.observed_digital && tmdb.release().digital_release.is_none() {
        tmdb.release_dates
            .results
            .extend(plex.release_dates.results);
        tmdb.observed_digital = true;
    }
    tmdb
}

#[cfg(test)]
mod release_date_tests {
    use super::*;

    fn movie(release_date: &str, dates: &[(i64, &str)]) -> TmdbMovie {
        serde_json::from_value(serde_json::json!({
            "id": 1, "title": "x", "release_date": release_date,
            "release_dates": { "results": [{ "release_dates": dates.iter().map(|(k, d)| serde_json::json!({ "type": k, "release_date": d })).collect::<Vec<_>>() }] },
        }))
        .unwrap()
    }

    #[test]
    fn a_blank_tmdb_date_does_not_hide_plexs() {
        let merged = merge(movie("2024-02-27", &[]), movie("", &[]));
        assert_eq!(merged.release_date.as_deref(), Some("2024-02-27"));
        assert_eq!(merged.release().in_cinemas.as_deref(), Some("2024-02-27"));
    }

    #[test]
    fn malformed_provider_dates_are_unknown_not_panics() {
        // Valid values (with a time part) are kept as dates.
        let r = movie(
            "2025-06-01",
            &[
                (4, "2025-08-15T00:00:00.000Z"),
                (4, "2025-07-01T00:00:00.000Z"),
                (5, "2025-09-01"),
            ],
        )
        .release();
        assert_eq!(r.in_cinemas.as_deref(), Some("2025-06-01"));
        assert_eq!(r.digital_release.as_deref(), Some("2025-07-01"));
        assert_eq!(r.physical_release.as_deref(), Some("2025-09-01"));
        // A multi-byte character at the 10th byte, impossible dates and junk are ignored.
        let r = movie(
            "2025-06-0é",
            &[
                (4, "2025-0€-15T00:00:00Z"),
                (4, "2025-13-40"),
                (5, "soon soon soon"),
                (5, "2025"),
            ],
        )
        .release();
        assert_eq!(r, crate::catalog::ReleaseDates::default());
        // One bad entry does not hide a good one.
        let r = movie("", &[(4, "not a date at all"), (4, "2026-02-03")]).release();
        assert_eq!(r.digital_release.as_deref(), Some("2026-02-03"));
        assert_eq!(r.in_cinemas, None);
    }
}
