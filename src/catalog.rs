//! Imported catalog observation (`catalog.json`) and in-memory lookup index.
//!
//! The catalog records what Radarr/Sonarr reported at snapshot time. It is evidence about
//! ownership, not the current state of the library.

use crate::ids::{MediaId, SeriesKey};
use crate::quality::Quality;
use crate::title;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const CATALOG_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub source_app: String,
    pub native_id: i64,
    pub import_id: String,
    pub observed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Movie {
    pub id: MediaId,
    pub tmdb_id: u32,
    pub imdb_id: Option<String>,
    pub title: String,
    pub original_title: Option<String>,
    pub alternate_titles: Vec<String>,
    pub year: Option<u16>,
    pub secondary_year: Option<u16>,
    pub runtime_minutes: Option<u32>,
    pub original_language: Option<String>,
    pub status: Option<String>,
    pub is_available: Option<bool>,
    pub monitored: bool,
    pub quality_profile: Option<String>,
    pub root_folder: Option<String>,
    pub has_file: bool,
    pub file_quality: Option<Quality>,
    pub identity_conflict: bool,
    pub provenance: Provenance,
    /// Folder relative to the root (e.g. "The Matrix (1999)").
    #[serde(default)]
    pub folder: Option<String>,
    /// Evidence about the owned file, when known.
    #[serde(default)]
    pub file: Option<FileEvidence>,
    /// Ownership cannot be established from the available evidence.
    #[serde(default)]
    pub ownership_unknown: bool,
    /// Covered by an active download (managed catalog only).
    #[serde(default)]
    pub queued: bool,
    /// Native `minimumAvailability` (`released` when added here). Only `released` is
    /// implemented; other modes are reported unsupported.
    #[serde(default)]
    pub minimum_availability: Option<String>,
    /// Release evidence (RFC 3339 or `YYYY-MM-DD`) from the native import, TMDB or the
    /// operator. Never invented.
    #[serde(default)]
    pub in_cinemas: Option<String>,
    #[serde(default)]
    pub physical_release: Option<String>,
    #[serde(default)]
    pub digital_release: Option<String>,
    /// Acquisition ownership in the managed catalog (set by the index, never imported):
    /// `adopted`, `legacy_managed`, `observed` or `released` (see `acquisition_state`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquisition: Option<String>,
}

/// Acquisition ownership of a managed-catalog row.
///
/// - `adopted`: this server acquires it (adopted through `POST /library`, or created here).
/// - `legacy_managed`: seeded as managed by an import in legacy mode, never adopted. It
///   acquires in legacy mode only.
/// - `observed`: imported in coexistence mode and never adopted. Catalog and ownership
///   evidence only; the native app acquires it.
/// - `released`: removed from management after an adoption.
pub fn acquisition_state(managed: bool, origin: &str, adopted: bool) -> &'static str {
    match (managed, adopted || origin == "api") {
        (true, true) => "adopted",
        (true, false) => "legacy_managed",
        (false, false) => "observed",
        (false, true) => "released",
    }
}

/// Release evidence for a movie added here (TMDB or operator-supplied).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleaseDates {
    pub in_cinemas: Option<String>,
    pub physical_release: Option<String>,
    pub digital_release: Option<String>,
}

/// Whether a movie may be grabbed now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Available,
    /// Not before this instant (RFC 3339).
    NotYet(String),
    /// No usable release evidence.
    Unknown(String),
    /// A `minimumAvailability` mode this build does not implement.
    Unsupported(String),
}

fn parse_date(s: &str) -> Option<time::OffsetDateTime> {
    use time::format_description::well_known::Rfc3339;
    let s = s.trim();
    time::OffsetDateTime::parse(s, &Rfc3339).ok().or_else(|| {
        let d = time::Date::parse(
            s.get(..10)?,
            time::macros::format_description!("[year]-[month]-[day]"),
        )
        .ok()?;
        Some(d.midnight().assume_utc())
    })
}

impl Movie {
    /// Native Radarr `IsAvailable` for `minimumAvailability = released`: the earliest of the
    /// physical and digital releases, else in-cinemas + 90 days, plus the availability delay.
    /// With no release evidence the movie is not available, unless the native snapshot itself
    /// reported it available.
    pub fn availability(&self, now: time::OffsetDateTime, delay_days: i64) -> Availability {
        let mode = self.minimum_availability.as_deref().unwrap_or("released");
        if !mode.eq_ignore_ascii_case("released") {
            return Availability::Unsupported(format!(
                "minimum availability '{mode}' is not implemented (only 'released')"
            ));
        }
        let release = [
            self.physical_release.as_deref(),
            self.digital_release.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter_map(parse_date)
        .min()
        .or_else(|| {
            self.in_cinemas
                .as_deref()
                .and_then(parse_date)
                .map(|d| d + time::Duration::days(90))
        });
        match release {
            Some(d) => {
                let at = d + time::Duration::days(delay_days.max(0));
                if now >= at {
                    Availability::Available
                } else {
                    Availability::NotYet(crate::import::rfc3339(at))
                }
            }
            None if self.is_available == Some(true) => Availability::Available,
            None => Availability::Unknown(
                "no release date is known (physical/digital release or in-cinemas); configure [tmdb] or supply metadata release dates".into(),
            ),
        }
    }
}

/// What is known about an owned media file. Paths are relative to the media root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEvidence {
    pub path: Option<String>,
    pub size: Option<u64>,
    pub quality: Option<Quality>,
    /// `None` = unknown (never guessed; revision upgrades fail closed).
    pub revision: Option<crate::release::Revision>,
    /// `snapshot`, `scan` or `import`.
    pub source: String,
    pub observed_at: String,
    /// Media probe result: `ok`, `failed: <reason>` or `None` when not probed yet.
    #[serde(default)]
    pub probe: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AltTitle {
    pub title: String,
    /// Season the alias applies to; `None` = all seasons.
    pub season: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeasonInfo {
    pub number: u16,
    pub monitored: bool,
    pub episode_file_count: Option<u32>,
    pub episode_count: Option<u32>,
    pub total_episode_count: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Episode {
    pub season: u16,
    pub number: u16,
    pub title: Option<String>,
    pub air_date_utc: Option<String>,
    pub runtime_minutes: Option<u32>,
    pub has_file: bool,
    pub monitored: bool,
    pub file_quality: Option<Quality>,
    #[serde(default)]
    pub file: Option<FileEvidence>,
    #[serde(default)]
    pub ownership_unknown: bool,
    #[serde(default)]
    pub queued: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Series {
    pub id: MediaId,
    pub tvdb_id: u32,
    pub tvmaze_id: Option<u32>,
    pub tmdb_id: Option<u32>,
    pub imdb_id: Option<String>,
    pub title: String,
    pub alternate_titles: Vec<AltTitle>,
    pub year: Option<u16>,
    pub series_type: String,
    pub scene_numbering: bool,
    pub runtime_minutes: Option<u32>,
    pub original_language: Option<String>,
    pub status: Option<String>,
    pub monitored: bool,
    pub quality_profile: Option<String>,
    pub root_folder: Option<String>,
    pub seasons: Vec<SeasonInfo>,
    /// Present only for series included in `episode_samples`.
    pub episodes: Option<Vec<Episode>>,
    pub identity_conflict: bool,
    pub provenance: Provenance,
    /// Folder relative to the root (e.g. "Slow Horses").
    #[serde(default)]
    pub folder: Option<String>,
    /// Native Sonarr `monitorNewItems` (`all` or `none`): whether seasons that appear later
    /// are monitored. Absent in older imports (treated as `all`, the native default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor_new_items: Option<String>,
    /// Acquisition ownership in the managed catalog (set by the index, never imported).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquisition: Option<String>,
    /// Specials the native app effectively monitors (latest import; set by the index).
    /// Season 0 is never acquired here, so these stay with the native app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_specials_monitored: Option<u32>,
    /// Active metadata numbering hold (set by the index): the latest refresh found episodes
    /// that contradict the imported numbering, and no new season is appended until a refresh
    /// finds none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_hold: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub schema_version: u32,
    pub import_id: String,
    pub observed_at: String,
    pub movies: Vec<Movie>,
    pub series: Vec<Series>,
}

/// Lookup index built once at startup.
pub struct CatalogIndex {
    pub catalog: Catalog,
    movies_by_tmdb: HashMap<u32, Vec<usize>>,
    series_by_tvdb: HashMap<u32, Vec<usize>>,
    series_by_tvmaze: HashMap<u32, Vec<usize>>,
    movie_keys: HashMap<String, Vec<usize>>,
    series_keys: HashMap<String, Vec<usize>>,
}

/// Title keys for matching, shared by release decisions, payload identity, scans, RSS and
/// catalog lookup: the title, its "(YYYY)"-stripped base, and aliases.
/// The original title counts as an alias, as it does for decision targets.
pub fn movie_title_keys(m: &Movie) -> Vec<String> {
    let aliases: Vec<&str> = m
        .original_title
        .iter()
        .chain(&m.alternate_titles)
        .map(String::as_str)
        .collect();
    title_keys(&m.title, &aliases)
}

pub fn series_title_keys(s: &Series, season: Option<u16>) -> Vec<String> {
    series_keys(&s.title, &s.alternate_titles, season)
}

/// Series keys; with a season, aliases scoped to other seasons are left out.
pub fn series_keys(title: &str, alternates: &[AltTitle], season: Option<u16>) -> Vec<String> {
    let aliases: Vec<&str> = alternates
        .iter()
        .filter(|alt| alt.season.is_none() || season.is_none() || alt.season == season)
        .map(|alt| alt.title.as_str())
        .collect();
    title_keys(title, &aliases)
}

/// Keys of the title and each alias with and without a trailing "(YYYY)", then the
/// franchise-prefixed alias forms the metadata itself implies (`title::prefixed_aliases`).
pub fn title_keys(canonical: &str, aliases: &[&str]) -> Vec<String> {
    let mut keys = vec![];
    let mut push = |k: String| {
        if !k.is_empty() && !keys.contains(&k) {
            keys.push(k);
        }
    };
    for t in std::iter::once(&canonical).chain(aliases) {
        let (base, _) = title::split_trailing_year(t);
        push(title::key(t));
        push(title::key(base));
    }
    for composite in title::prefixed_aliases(canonical, aliases) {
        push(title::key_from_tokens(&composite));
    }
    keys
}

impl CatalogIndex {
    pub fn new(catalog: Catalog) -> Self {
        let mut idx = CatalogIndex {
            movies_by_tmdb: HashMap::new(),
            series_by_tvdb: HashMap::new(),
            series_by_tvmaze: HashMap::new(),
            movie_keys: HashMap::new(),
            series_keys: HashMap::new(),
            catalog,
        };
        for (i, m) in idx.catalog.movies.iter().enumerate() {
            idx.movies_by_tmdb.entry(m.tmdb_id).or_default().push(i);
            for k in movie_title_keys(m) {
                idx.movie_keys.entry(k).or_default().push(i);
            }
        }
        for (i, s) in idx.catalog.series.iter().enumerate() {
            idx.series_by_tvdb.entry(s.tvdb_id).or_default().push(i);
            if let Some(tvmaze) = s.tvmaze_id {
                idx.series_by_tvmaze.entry(tvmaze).or_default().push(i);
            }
            for k in series_title_keys(s, None) {
                idx.series_keys.entry(k).or_default().push(i);
            }
        }
        idx
    }

    pub fn movie(&self, tmdb: u32) -> Option<&Movie> {
        self.movies_by_tmdb
            .get(&tmdb)
            .and_then(|v| v.first())
            .map(|&i| &self.catalog.movies[i])
    }

    pub fn series(&self, key: SeriesKey) -> Option<&Series> {
        let hits = match key {
            SeriesKey::Tvdb(id) => self.series_by_tvdb.get(&id),
            SeriesKey::Tvmaze(id) => self.series_by_tvmaze.get(&id),
        };
        hits.and_then(|v| v.first())
            .map(|&i| &self.catalog.series[i])
    }

    /// Other catalog movies sharing a title key, excluding `except`.
    pub fn movies_with_key(&self, key: &str, except: Option<MediaId>) -> Vec<&Movie> {
        self.movie_keys
            .get(key)
            .into_iter()
            .flatten()
            .map(|&i| &self.catalog.movies[i])
            .filter(|m| Some(m.id) != except)
            .collect()
    }

    pub fn series_with_key(&self, key: &str, except: Option<MediaId>) -> Vec<&Series> {
        self.series_keys
            .get(key)
            .into_iter()
            .flatten()
            .map(|&i| &self.catalog.series[i])
            .filter(|s| Some(s.id) != except)
            .collect()
    }

    /// Substring search over folded titles/aliases; exact key matches first, then by title.
    pub fn search_movies(&self, query: &str) -> Vec<&Movie> {
        let (q_tokens, q_key) = query_terms(query);
        let mut hits: Vec<(bool, &Movie)> = self
            .catalog
            .movies
            .iter()
            .filter_map(|m| {
                let keys = movie_title_keys(m);
                let names = std::iter::once(&m.title)
                    .chain(m.original_title.iter())
                    .chain(m.alternate_titles.iter());
                matches_query(&q_tokens, &q_key, &keys, names).map(|exact| (exact, m))
            })
            .collect();
        hits.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.title.cmp(&b.1.title))
                .then(a.1.id.cmp(&b.1.id))
        });
        hits.into_iter().map(|(_, m)| m).collect()
    }

    pub fn search_series(&self, query: &str) -> Vec<&Series> {
        let (q_tokens, q_key) = query_terms(query);
        let mut hits: Vec<(bool, &Series)> = self
            .catalog
            .series
            .iter()
            .filter_map(|s| {
                let keys = series_title_keys(s, None);
                let names =
                    std::iter::once(&s.title).chain(s.alternate_titles.iter().map(|a| &a.title));
                matches_query(&q_tokens, &q_key, &keys, names).map(|exact| (exact, s))
            })
            .collect();
        hits.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.title.cmp(&b.1.title))
                .then(a.1.id.cmp(&b.1.id))
        });
        hits.into_iter().map(|(_, s)| s).collect()
    }
}

fn query_terms(query: &str) -> (Vec<String>, String) {
    (title::tokens(query), title::key(query))
}

/// `Some(true)` = exact key match, `Some(false)` = all query tokens present, `None` = no match.
fn matches_query<'a>(
    q_tokens: &[String],
    q_key: &str,
    keys: &[String],
    names: impl Iterator<Item = &'a String>,
) -> Option<bool> {
    if q_tokens.is_empty() {
        return None;
    }
    if keys.iter().any(|k| k == q_key) {
        return Some(true);
    }
    for name in names {
        let toks = title::tokens(name);
        if q_tokens
            .iter()
            .all(|q| toks.iter().any(|t| t.starts_with(q.as_str())))
        {
            return Some(false);
        }
    }
    None
}
