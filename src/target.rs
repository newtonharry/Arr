//! Resolves a media id into what matching and planning need: titles, aliases, years,
//! runtimes, episode lists and ownership, from the imported catalog first and the optional
//! metadata providers second.

use crate::catalog::{AltTitle, CatalogIndex, Movie, Series};
use crate::error::{ApiError, ErrorCode};
use crate::ids::{MediaId, SeriesKey};
use crate::metadata::{self, Movies, Tvmaze, TvmazeShow};
use crate::settings::MediaKind;
use axum::http::StatusCode;
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Ownership {
    Owned,
    Missing,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Catalog,
    Tvmaze,
    Tmdb,
}

#[derive(Debug, Clone, Serialize)]
pub struct MovieTarget {
    pub id: MediaId,
    pub title: String,
    pub alternate_titles: Vec<String>,
    pub year: Option<u16>,
    pub secondary_year: Option<u16>,
    pub runtime_minutes: Option<u32>,
    pub original_language: Option<String>,
    pub is_available: Option<bool>,
    pub ownership: Ownership,
    pub quality_profile: Option<String>,
    pub root_folder: Option<String>,
    pub identity_conflict: bool,
    pub metadata_source: Source,
    pub in_catalog: bool,
    /// Evidence about the owned file (quality, revision, managed path).
    pub existing: Option<crate::catalog::FileEvidence>,
    /// Covered by an active download.
    pub queued: bool,
    /// Folder relative to the root, when known.
    pub folder: Option<String>,
    /// Why the movie is not available yet (release evidence), when it is not.
    #[serde(default)]
    pub availability_note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeriesTarget {
    pub id: MediaId,
    pub title: String,
    pub alternate_titles: Vec<AltTitle>,
    pub year: Option<u16>,
    pub runtime_minutes: Option<u32>,
    pub original_language: Option<String>,
    pub series_type: String,
    pub scene_numbering: bool,
    pub quality_profile: Option<String>,
    pub root_folder: Option<String>,
    pub identity_conflict: bool,
    pub metadata_source: Source,
    pub in_catalog: bool,
    pub tvdb_id: Option<u32>,
    pub tvmaze_id: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EpisodeInfo {
    pub number: u16,
    pub title: Option<String>,
    pub air_date_utc: Option<String>,
    pub aired: bool,
    pub runtime_minutes: Option<u32>,
    pub ownership: Ownership,
    pub existing: Option<crate::catalog::FileEvidence>,
    pub queued: bool,
    /// The grab covering this episode may be replaced by a season pack (see
    /// `catalog::Episode::replaceable`).
    pub replaceable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeasonTarget {
    pub series: SeriesTarget,
    pub season: u16,
    /// `None` when no episode list could be obtained.
    pub episodes: Option<Vec<EpisodeInfo>>,
    pub episode_list_source: Option<Source>,
    pub ownership_source: String,
    pub notes: Vec<String>,
}

impl SeasonTarget {
    pub fn episode(&self, n: u16) -> Option<&EpisodeInfo> {
        self.episodes.as_ref()?.iter().find(|e| e.number == n)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    Movie(MovieTarget),
    Season(SeasonTarget),
    Episode { season: SeasonTarget, episode: u16 },
}

impl Target {
    pub fn media(&self) -> MediaKind {
        match self {
            Target::Movie(_) => MediaKind::Movie,
            _ => MediaKind::Series,
        }
    }

    pub fn quality_profile(&self) -> Option<&str> {
        match self {
            Target::Movie(m) => m.quality_profile.as_deref(),
            Target::Season(s) | Target::Episode { season: s, .. } => {
                s.series.quality_profile.as_deref()
            }
        }
    }

    pub fn root_folder(&self) -> Option<&str> {
        match self {
            Target::Movie(m) => m.root_folder.as_deref(),
            Target::Season(s) | Target::Episode { season: s, .. } => {
                s.series.root_folder.as_deref()
            }
        }
    }
}

pub struct Resolver<'a> {
    pub catalog: Option<&'a CatalogIndex>,
    pub tvmaze: Option<&'a Tvmaze>,
    pub movies: Option<&'a Movies>,
    pub now: OffsetDateTime,
    /// Radarr `availabilityDelay` (days) for movie availability.
    pub availability_delay_days: i64,
}

fn aired(date: Option<&str>, now: OffsetDateTime) -> bool {
    date.and_then(|d| OffsetDateTime::parse(d, &Rfc3339).ok())
        .is_some_and(|t| t <= now)
}

fn movie_from_catalog(m: &Movie, now: OffsetDateTime, delay_days: i64) -> MovieTarget {
    use crate::catalog::Availability;
    let (is_available, availability_note) = match m.availability(now, delay_days) {
        Availability::Available => (Some(true), None),
        Availability::NotYet(at) => (
            Some(false),
            Some(format!(
                "not released yet: available from {at} (minimum availability released)"
            )),
        ),
        Availability::Unknown(why) | Availability::Unsupported(why) => (Some(false), Some(why)),
    };
    MovieTarget {
        availability_note,
        id: m.id,
        title: m.title.clone(),
        alternate_titles: m
            .original_title
            .iter()
            .chain(&m.alternate_titles)
            .cloned()
            .collect(),
        year: m.year,
        secondary_year: m.secondary_year,
        runtime_minutes: m.runtime_minutes,
        original_language: m.original_language.clone(),
        is_available,
        ownership: if m.ownership_unknown {
            Ownership::Unknown
        } else if m.has_file {
            Ownership::Owned
        } else {
            Ownership::Missing
        },
        existing: m.file.clone(),
        queued: m.queued,
        folder: m.folder.clone(),
        quality_profile: m.quality_profile.clone(),
        root_folder: m.root_folder.clone(),
        identity_conflict: m.identity_conflict,
        metadata_source: Source::Catalog,
        in_catalog: true,
    }
}

fn series_from_catalog(s: &Series) -> SeriesTarget {
    SeriesTarget {
        id: s.id,
        title: s.title.clone(),
        alternate_titles: s.alternate_titles.clone(),
        year: s.year,
        runtime_minutes: s.runtime_minutes,
        original_language: s.original_language.clone(),
        series_type: s.series_type.clone(),
        scene_numbering: s.scene_numbering,
        quality_profile: s.quality_profile.clone(),
        root_folder: s.root_folder.clone(),
        identity_conflict: s.identity_conflict,
        metadata_source: Source::Catalog,
        in_catalog: true,
        tvdb_id: Some(s.tvdb_id),
        tvmaze_id: s.tvmaze_id,
    }
}

pub fn series_from_tvmaze(show: &TvmazeShow) -> SeriesTarget {
    let id = match show.externals.thetvdb {
        Some(tvdb) if tvdb > 0 => MediaId::Series(SeriesKey::Tvdb(tvdb)),
        _ => MediaId::Series(SeriesKey::Tvmaze(show.id)),
    };
    SeriesTarget {
        id,
        title: show.name.clone(),
        alternate_titles: vec![],
        year: show.year(),
        runtime_minutes: show.runtime.or(show.average_runtime).filter(|r| *r > 0),
        original_language: show.language.as_deref().map(str::to_lowercase),
        series_type: match show.kind.as_deref() {
            Some("Animation") if show.language.as_deref() == Some("Japanese") => "anime".into(),
            Some("Talk Show" | "News") => "daily".into(),
            _ => "standard".into(),
        },
        scene_numbering: false,
        quality_profile: None,
        root_folder: None,
        identity_conflict: false,
        metadata_source: Source::Tvmaze,
        in_catalog: false,
        tvdb_id: show.externals.thetvdb.filter(|t| *t > 0),
        tvmaze_id: Some(show.id),
    }
}

fn metadata_unavailable(msg: String) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, ErrorCode::MetadataUnavailable, msg)
}

impl Resolver<'_> {
    pub async fn movie(&self, tmdb: u32) -> Result<MovieTarget, ApiError> {
        if let Some(m) = self.catalog.and_then(|c| c.movie(tmdb)) {
            return Ok(movie_from_catalog(
                m,
                self.now,
                self.availability_delay_days,
            ));
        }
        let Some(client) = self.movies else {
            return Err(metadata_unavailable(format!(
                "movie:tmdb:{tmdb} is not in the imported catalog and no movie metadata source (Plex or TMDB) is configured, so its title/year/runtime are unknown"
            )));
        };
        let m = client
            .movie(tmdb)
            .await
            .map_err(|e| e.into_api("movie metadata"))?
            .ok_or_else(|| metadata_unavailable(format!("no movie metadata for TMDB id {tmdb}")))?;
        Ok(MovieTarget {
            id: MediaId::Movie { tmdb },
            existing: None,
            queued: false,
            folder: None,
            availability_note: None,
            alternate_titles: {
                // Plex lists the original title among the alternatives too: once is enough.
                let mut v: Vec<String> = vec![];
                for t in m
                    .original_title
                    .iter()
                    .filter(|t| **t != m.title)
                    .cloned()
                    .chain(m.alternative_titles.titles.iter().map(|t| t.title.clone()))
                {
                    if !v.contains(&t) {
                        v.push(t);
                    }
                }
                v
            },
            year: metadata::year_of(m.release_date.as_deref()),
            secondary_year: None,
            runtime_minutes: m.runtime.filter(|r| *r > 0),
            original_language: m
                .original_language
                .as_deref()
                .and_then(metadata::language_name)
                .map(str::to_owned),
            is_available: None,
            ownership: Ownership::Missing,
            quality_profile: None,
            root_folder: None,
            identity_conflict: false,
            metadata_source: Source::Tmdb,
            in_catalog: false,
            title: m.title,
        })
    }

    async fn tvmaze_show(
        &self,
        key: SeriesKey,
        catalog: Option<&Series>,
    ) -> Result<Option<TvmazeShow>, ApiError> {
        let Some(tv) = self.tvmaze else {
            return Ok(None);
        };
        let r = match (catalog.and_then(|s| s.tvmaze_id), key) {
            (Some(id), _) | (None, SeriesKey::Tvmaze(id)) => tv.by_id(id).await,
            (None, SeriesKey::Tvdb(id)) => tv.by_tvdb(id).await,
        };
        r.map_err(|e| e.into_api("tvmaze"))
    }

    pub async fn series(
        &self,
        key: SeriesKey,
    ) -> Result<(SeriesTarget, Option<TvmazeShow>), ApiError> {
        if let Some(s) = self.catalog.and_then(|c| c.series(key)) {
            return Ok((series_from_catalog(s), None));
        }
        if self.tvmaze.is_none() {
            return Err(metadata_unavailable(format!(
                "series:{key} is not in the imported catalog and TVmaze is disabled"
            )));
        }
        let show = self
            .tvmaze_show(key, None)
            .await?
            .ok_or_else(|| metadata_unavailable(format!("TVmaze has no show for series:{key}")))?;
        // A TVmaze show whose TVDB id is in the catalog resolves to the catalog entry.
        if let Some(s) = show
            .externals
            .thetvdb
            .and_then(|t| self.catalog.and_then(|c| c.series(SeriesKey::Tvdb(t))))
        {
            return Ok((series_from_catalog(s), Some(show)));
        }
        let mut target = series_from_tvmaze(&show);
        if matches!(key, SeriesKey::Tvdb(_)) {
            target.id = MediaId::Series(key);
        }
        Ok((target, Some(show)))
    }

    pub async fn season(&self, key: SeriesKey, season: u16) -> Result<SeasonTarget, ApiError> {
        let (series, show) = self.series(key).await?;
        let catalog_series = self.catalog.and_then(|c| c.series(key)).or_else(|| {
            series
                .tvdb_id
                .and_then(|t| self.catalog.and_then(|c| c.series(SeriesKey::Tvdb(t))))
        });
        let mut notes = vec![];
        let series_runtime = series.runtime_minutes;

        // 1. Episode samples from the snapshot carry per-episode ownership.
        if let Some(eps) = catalog_series.and_then(|s| s.episodes.as_ref()) {
            let mut list: Vec<EpisodeInfo> = eps
                .iter()
                .filter(|e| e.season == season)
                .map(|e| EpisodeInfo {
                    number: e.number,
                    title: e.title.clone(),
                    air_date_utc: e.air_date_utc.clone(),
                    aired: aired(e.air_date_utc.as_deref(), self.now),
                    runtime_minutes: e.runtime_minutes.or(series_runtime),
                    ownership: if e.ownership_unknown {
                        Ownership::Unknown
                    } else if e.has_file {
                        Ownership::Owned
                    } else {
                        Ownership::Missing
                    },
                    existing: e.file.clone(),
                    queued: e.queued,
                    replaceable: e.replaceable,
                })
                .collect();
            if !list.is_empty() {
                list.sort_by_key(|e| e.number);
                return Ok(SeasonTarget {
                    series,
                    season,
                    episodes: Some(list),
                    episode_list_source: Some(Source::Catalog),
                    ownership_source: "per-episode hasFile from the imported Sonarr snapshot"
                        .into(),
                    notes,
                });
            }
        }

        // 2. Ownership from season statistics (counts only), episode list from TVmaze.
        let (season_ownership, ownership_source) = match catalog_series {
            None => (
                Ownership::Missing,
                "series is not in the imported catalog; assumed not owned (files outside Sonarr are unknown)".to_string(),
            ),
            Some(s) => match s.seasons.iter().find(|x| x.number == season) {
                None => (Ownership::Missing, "season is absent from the Sonarr snapshot; assumed not owned".to_string()),
                Some(stats) => match (stats.episode_file_count, stats.total_episode_count) {
                    (Some(0), _) => (Ownership::Missing, "Sonarr season statistics report 0 episode files".to_string()),
                    (Some(files), Some(total)) if total > 0 && files >= total => {
                        (Ownership::Owned, format!("Sonarr season statistics report all {total} episodes on disk"))
                    }
                    (files, total) => (
                        Ownership::Unknown,
                        format!("Sonarr season statistics report {files:?} of {total:?} episode files but the snapshot has no per-episode sample for this season, so ownership per episode is unknown"),
                    ),
                },
            },
        };
        let show = match show {
            Some(s) => Some(s),
            None => self.tvmaze_show(key, catalog_series).await?,
        };
        let Some(show) = show else {
            notes.push(if self.tvmaze.is_none() {
                "episode list unavailable: no snapshot sample for this season and TVmaze is disabled".into()
            } else {
                "episode list unavailable: TVmaze has no matching show".into()
            });
            return Ok(SeasonTarget {
                series,
                season,
                episodes: None,
                episode_list_source: None,
                ownership_source,
                notes,
            });
        };
        let tv = self.tvmaze.expect("show implies tvmaze");
        let eps = tv
            .episodes(show.id)
            .await
            .map_err(|e| e.into_api("tvmaze"))?;
        let mut list: Vec<EpisodeInfo> = eps
            .iter()
            .filter(|e| e.season == Some(u32::from(season)))
            .filter_map(|e| {
                let number = u16::try_from(e.number?).ok().filter(|n| *n > 0)?;
                Some(EpisodeInfo {
                    number,
                    title: e.name.clone(),
                    air_date_utc: e.airstamp.clone(),
                    aired: aired(e.airstamp.as_deref(), self.now),
                    runtime_minutes: e.runtime.filter(|r| *r > 0).or(series_runtime),
                    ownership: season_ownership,
                    existing: None,
                    queued: false,
                    replaceable: false,
                })
            })
            .collect();
        list.sort_by_key(|e| e.number);
        list.dedup_by_key(|e| e.number);
        let mut ownership_source = ownership_source;
        if let Some(stats) =
            catalog_series.and_then(|s| s.seasons.iter().find(|x| x.number == season))
            && let Some(total) = stats.total_episode_count
            && total as usize != list.len()
        {
            notes.push(format!(
                "Sonarr snapshot counts {total} episodes in season {season} but TVmaze lists {}",
                list.len()
            ));
            // "All owned" only covers the episodes Sonarr knew about.
            if season_ownership == Ownership::Owned {
                list.iter_mut()
                    .for_each(|e| e.ownership = Ownership::Unknown);
                ownership_source = format!(
                    "{ownership_source}, but the episode counts disagree, so per-episode ownership is unknown"
                );
            }
        }
        if list.is_empty() {
            notes.push(format!(
                "TVmaze lists no regular episodes for season {season}"
            ));
        }
        Ok(SeasonTarget {
            series,
            season,
            episodes: (!list.is_empty()).then_some(list),
            episode_list_source: Some(Source::Tvmaze),
            ownership_source,
            notes,
        })
    }

    pub async fn target(&self, id: MediaId) -> Result<Target, ApiError> {
        match id {
            MediaId::Movie { tmdb } => Ok(Target::Movie(self.movie(tmdb).await?)),
            MediaId::Season(key, s) => Ok(Target::Season(self.season(key, s).await?)),
            MediaId::Episode(key, s, e) => Ok(Target::Episode {
                season: self.season(key, s).await?,
                episode: e,
            }),
            MediaId::Series(_) => Err(ApiError::bad_request(
                ErrorCode::InvalidMediaId,
                "searches and plans target a movie, a season or an episode (whole-series requests are not supported)",
            )),
        }
    }
}
