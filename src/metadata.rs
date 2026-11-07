//! Optional external metadata: TVmaze (free, no key) and TMDB (operator credential).
//! Both are read-only GETs to the configured base URL; data is cached briefly.

use crate::config::MetadataConfig;
use crate::upstream::{self, TtlCache, UpstreamError};
use reqwest::Url;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::time::Duration;

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
}

impl TvmazeShow {
    pub fn year(&self) -> Option<u16> {
        self.premiered
            .as_deref()
            .and_then(|d| d.get(..4))
            .and_then(|y| y.parse().ok())
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

pub struct Tvmaze {
    base: Url,
    http: reqwest::Client,
    shows: TtlCache<String, Option<TvmazeShow>>,
    episodes: TtlCache<u32, Vec<TvmazeEpisode>>,
}

impl Tvmaze {
    pub fn new(cfg: &MetadataConfig) -> Self {
        Tvmaze {
            base: cfg.base_url.clone(),
            http: upstream::http_client(cfg.timeout, reqwest::redirect::Policy::none()),
            shows: TtlCache::new(METADATA_TTL, 512),
            episodes: TtlCache::new(METADATA_TTL, 128),
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
