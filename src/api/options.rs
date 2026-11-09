//! "Download options" on a movie or show page: the quality level of a title (Smaller,
//! Standard or Best) and a list of the copies Arr found, one of which a person can pick.
//! Nothing here deletes or replaces a file in the library: a level only changes what Arr
//! looks for next, and a copy that would replace a file already owned is never offered.

use super::*;
use crate::catalog::Series;
use crate::settings::Level;

const MIB: f64 = 1024.0 * 1024.0;
/// The most copies the list shows.
const MAX_COPIES: usize = 12;

/// The profile id for a level of a title's media kind, when this setup has the levels.
pub(super) fn level_profile(st: &AppState, id: MediaId, level: &str) -> Result<String, ApiError> {
    let l = st.loaded()?;
    let media = downloads::media_kind(id);
    let profile = Level::parse(level)
        .map(|lv| lv.profile_id(media))
        .filter(|p| l.settings.profile(p).is_some_and(|x| x.media == media))
        .ok_or_else(|| {
            ApiError::bad_request(
                ErrorCode::InvalidRequest,
                "level must be smaller, standard or best, and this setup must offer it",
            )
        })?;
    Ok(profile)
}

/// Points a title already in the catalog at `profile`. Returns whether it was there.
pub(super) async fn apply_profile(
    st: &Arc<AppState>,
    id: MediaId,
    profile: &str,
) -> Result<bool, ApiError> {
    let index = st.index();
    let stored = match id {
        MediaId::Movie { tmdb } => index.movie(tmdb).map(|m| m.id),
        other => other
            .series_key()
            .and_then(|k| index.series(k))
            .map(|s| s.id),
    };
    let Some(stored) = stored else {
        return Ok(false);
    };
    let _guard = st.library_lock.lock().await;
    let (profile, now) = (profile.to_string(), OffsetDateTime::now_utc());
    let changed = st
        .store
        .with(move |c| library::set_quality_profile(c, stored, &profile, now))
        .await
        .map_err(internal("quality"))?;
    if changed {
        st.reload_index().await.map_err(internal("index"))?;
    }
    Ok(changed)
}

/// What a page says about quality: the title's level, what each level looks like for it, and
/// whether Arr already has the title (so a change is saved now rather than with the next Get).
/// `levels` is empty when this setup has no built-in levels.
pub(super) fn page_fields(
    v: &mut Value,
    settings: &Settings,
    media: MediaKind,
    profile: Option<&str>,
    tracked: bool,
    runtime: Option<u32>,
) {
    let offered = Level::ALL
        .iter()
        .all(|l| settings.profile(&l.profile_id(media)).is_some());
    let levels: Vec<Value> = if offered {
        Level::ALL
            .iter()
            .map(|l| {
                let (label, resolution) = match l {
                    Level::Smaller => ("Smaller", "720p"),
                    Level::Standard => ("Standard", "1080p"),
                    Level::Best => ("Best", "4K"),
                };
                let size = runtime.filter(|m| *m > 0).map(|m| {
                    (f64::from(m) * crate::setup::typical_mib_per_minute(*l) * MIB).round() as u64
                });
                json!({ "level": l.as_str(), "label": label, "resolution": resolution, "size_bytes": size })
            })
            .collect()
    } else {
        vec![]
    };
    v["level"] = json!(
        profile
            .and_then(Level::of)
            .filter(|_| offered)
            .map(Level::as_str)
    );
    v["levels"] = json!(levels);
    v["tracked"] = json!(tracked);
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QualityBody {
    media_id: String,
    level: String,
}

/// `POST /quality`: the quality level of a movie or show Arr already knows. It changes what
/// Arr looks for next; files already in Plex are not touched.
pub(super) async fn quality(State(st): St, ApiJson(body): ApiJson<QualityBody>) -> ApiResult {
    st.loaded()?;
    let id = parse_media_id(&body.media_id)?;
    if !matches!(id, MediaId::Movie { .. } | MediaId::Series(_)) {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidMediaId,
            "set the quality of a movie or a whole show",
        ));
    }
    let profile = level_profile(&st, id, &body.level)?;
    if !apply_profile(&st, id, &profile).await? {
        return Err(ApiError::not_found(format!(
            "{id} is not on Arr's list yet; send the level with the Get"
        )));
    }
    ok(json!({ "media_id": id, "level": body.level }))
}

/// The plain word for the first reason a copy was skipped, and how it ranks as a reason
/// (a copy that is the wrong title is that before it is anything else).
fn skip_reason(code: &str) -> (u8, &'static str) {
    match code {
        "title_mismatch"
        | "year_mismatch"
        | "year_missing"
        | "target_year_unknown"
        | "ambiguous_title"
        | "identity_conflict"
        | "not_a_movie"
        | "multi_movie_bundle" => (0, "not the right title"),
        "wrong_season"
        | "multi_season_pack"
        | "partial_season_pack"
        | "complete_series_pack"
        | "season_pack_for_episode"
        | "wrong_episode"
        | "unknown_episode"
        | "not_tv_numbered"
        | "ambiguous_numbering"
        | "unsupported_numbering"
        | "unsupported_series_type"
        | "episode_list_unknown" => (1, "not the right episodes"),
        "episode_not_aired" | "season_not_fully_aired" | "not_available" => (2, "not out yet"),
        "covers_owned" | "covers_queued" | "ownership_unknown" => {
            (3, "you already have it or it's on its way")
        }
        "blocklisted" => (4, "failed before"),
        "quality_not_allowed" | "quality_unknown" => (5, "not the quality you chose"),
        "language_mismatch" | "language_ambiguous_multi" | "language_unknown" => {
            (6, "wrong language")
        }
        "size_unknown"
        | "size_limits_missing"
        | "size_unverifiable_runtime"
        | "size_below_minimum"
        | "size_above_maximum"
        | "size_above_global_maximum" => (7, "odd size"),
        "below_minimum_seeders" | "seeders_unknown" | "seed_requirement_unmet" => {
            (8, "hardly anyone sharing it")
        }
        "sample_or_extra" | "three_d" | "hardcoded_subs" | "dangerous_file_type" => {
            (9, "not a normal copy")
        }
        _ => (10, "didn't pass Arr's checks"),
    }
}

fn source_word(q: crate::quality::Quality) -> Option<&'static str> {
    let n = q.as_str();
    if n.starts_with("webdl") || n.starts_with("webrip") {
        Some("Web")
    } else if n.starts_with("bluray") || n.starts_with("remux") || n == "brdisk" {
        Some("Blu-ray")
    } else if n.starts_with("hdtv") || n == "sdtv" {
        Some("TV")
    } else {
        None
    }
}

fn resolution_word(q: crate::quality::Quality) -> Option<&'static str> {
    Some(match q.resolution()? {
        2160 => "4K",
        1080 => "1080p",
        720 => "720p",
        576 => "576p",
        _ => "480p",
    })
}

fn speed_word(seeders: Option<i64>) -> &'static str {
    match seeders {
        None => "ok",
        Some(n) if n >= 10 => "fast",
        Some(n) if n >= 3 => "ok",
        Some(_) => "slow",
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CopiesQuery {
    /// For a title Arr does not have yet: the level the person has picked on the page.
    level: Option<String>,
}

fn unit(id: MediaId) -> Result<MediaId, ApiError> {
    match id {
        MediaId::Series(_) => Err(ApiError::bad_request(
            ErrorCode::InvalidMediaId,
            "pick a copy of a movie, a season or an episode",
        )),
        other => Ok(other),
    }
}

/// `GET /copies/{media_id}`: the copies Arr found for a movie, a season or an episode, in
/// plain words, with the one Arr would pick marked, and why the others were skipped.
pub(super) async fn copies(
    State(st): St,
    p: Result<Path<String>, PathRejection>,
    ApiQuery(q): ApiQuery<CopiesQuery>,
) -> ApiResult {
    let id = unit(media_id(p)?)?;
    let want = match &q.level {
        Some(l) => Some(level_profile(&st, id, l)?),
        None => None,
    };
    // A title Arr has keeps its own level; the page's choice applies to one it does not.
    let index = st.index();
    let known = match id {
        MediaId::Movie { tmdb } => index
            .movie(tmdb)
            .is_some_and(|m| m.quality_profile.is_some()),
        other => other
            .series_key()
            .and_then(|k| index.series(k))
            .is_some_and(|s| s.quality_profile.is_some()),
    };
    let (target, run, _) = release_run(&st, id, false, want.as_deref().filter(|_| !known)).await?;
    let mut copies = vec![];
    let mut skipped: BTreeMap<(u8, &'static str), usize> = BTreeMap::new();
    for c in &run.candidates {
        let d = &c.decision;
        // A copy that would replace a file already owned is never offered.
        if d.approved && !d.replacement {
            if copies.len() < MAX_COPIES {
                let quality = c.parsed.quality;
                copies.push(json!({
                    "id": c.release_ref,
                    "resolution": resolution_word(quality),
                    "source": source_word(quality),
                    "size_bytes": c.release.size.filter(|s| *s > 0),
                    "speed": speed_word(c.release.seeders),
                    "recommended": copies.is_empty(),
                }));
            }
            continue;
        }
        let reason = d
            .rejections
            .iter()
            .map(|r| skip_reason(&r.code))
            .min()
            .unwrap_or((3, "you already have it or it's on its way"));
        *skipped.entry(reason).or_default() += 1;
    }
    let mut skipped: Vec<((u8, &str), usize)> = skipped.into_iter().collect();
    skipped.sort_by_key(|((rank, _), n)| (std::cmp::Reverse(*n), *rank));
    let skipped: Vec<Value> = skipped
        .into_iter()
        .map(|((_, reason), count)| json!({ "reason": reason, "count": count }))
        .collect();
    let recommended = copies.first().map(|c| c["id"].clone());
    ok(json!({
        "media_id": plan::canonical_id(&target),
        "recommended": recommended,
        "copies": copies,
        "skipped": skipped,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GetBody {
    /// The copy's id from the list.
    id: String,
    /// The level the person picked on the page, for a title Arr does not have yet.
    #[serde(default)]
    level: Option<String>,
}

/// Whether Arr is getting this unit: `None` when the title is not in the catalog, else
/// whether the movie, or every episode of the season or episode that is not yet in Plex, is
/// monitored.
fn monitored_unit(index: &CatalogIndex, id: MediaId) -> Option<bool> {
    match id {
        MediaId::Movie { tmdb } => index.movie(tmdb).map(|m| m.monitored),
        other => {
            let s: &Series = other.series_key().and_then(|k| index.series(k))?;
            if !s.monitored {
                return Some(false);
            }
            let (season, episode) = match other {
                MediaId::Season(_, n) => (n, None),
                MediaId::Episode(_, n, e) => (n, Some(e)),
                _ => return Some(true),
            };
            Some(match &s.episodes {
                Some(eps) => eps
                    .iter()
                    .filter(|e| e.season == season && episode.is_none_or(|n| n == e.number))
                    .filter(|e| !e.has_file)
                    .all(|e| e.monitored),
                None => s.seasons.iter().any(|x| x.number == season && x.monitored),
            })
        }
    }
}

/// `POST /copies/{media_id}/get`: download the copy a person picked. It goes through the same
/// checks as any download Arr starts (Arr must be getting the title, and it is held back for
/// a native Radarr/Sonarr title that was not handed over). A title Arr does not have yet is
/// added first, monitored, without a search of its own.
pub(super) async fn copies_get(
    State(st): St,
    p: Result<Path<String>, PathRejection>,
    ApiJson(body): ApiJson<GetBody>,
) -> ApiResult {
    st.loaded()?;
    let id = unit(media_id(p)?)?;
    let profile = match &body.level {
        Some(l) => Some(level_profile(&st, id, l)?),
        None => None,
    };
    match monitored_unit(&st.index(), id) {
        Some(false) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                ErrorCode::NotManaged,
                "Arr is not getting this right now. Tap Get to start again.",
            ));
        }
        Some(true) => {}
        None => {
            // Only a copy Arr listed for this title is worth adding the title for.
            let r = body.id.clone();
            let known = st
                .store
                .with(move |c| downloads::load_link(c, &r, Some(id)))
                .await
                .map_err(internal("link"))?
                .is_some_and(|l| l.target == id);
            if !known {
                return Err(ApiError::new(
                    StatusCode::NOT_FOUND,
                    ErrorCode::ReleaseNotFound,
                    "that copy is not on the list any more; look again",
                ));
            }
            add_value(&st, id.to_string(), None, false, profile).await?;
        }
    }
    let resp = grab(
        State(st.clone()),
        ApiJson(GrabBody {
            media_id: id.to_string(),
            release_ref: body.id,
        }),
    )
    .await?;
    let _ = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024).await;
    let index = st.index();
    let (title, _) = feed_title(&index, id);
    ok(json!({
        "media_id": id, "title": title, "detail": feed_detail(&index, &[id]),
        "outcome": "on_its_way", "until": null,
        "message": "Arr is getting that copy now.",
    }))
}
