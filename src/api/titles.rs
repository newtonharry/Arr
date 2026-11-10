//! The pages of one show or one movie in the web app: what is in Plex, what is on its way,
//! and the three safe actions (get, stop, follow new episodes). Nothing here deletes a file:
//! a stop unmonitors and cancels downloads that have not finished, never an imported file.

use super::*;
use crate::catalog::{Availability, Episode, SeasonInfo, Series};
use std::collections::HashMap;

fn day(date: &str) -> Option<String> {
    date.get(..10).map(str::to_string)
}

fn aired_at(e: &Episode, now: OffsetDateTime) -> bool {
    e.air_date_utc
        .as_deref()
        .and_then(|d| OffsetDateTime::parse(d, &Rfc3339).ok())
        .is_some_and(|t| t <= now)
}

fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max - 1).collect();
    format!("{}\u{2026}", head.trim_end())
}

/// One episode as the show page sees it.
struct Row {
    season: u16,
    number: u16,
    title: Option<String>,
    air: Option<String>,
    aired: bool,
    in_plex: bool,
    on_way: bool,
    wanted: bool,
    progress: Option<f64>,
}

impl Row {
    fn state(&self) -> &'static str {
        if self.in_plex {
            "in_plex"
        } else if self.on_way {
            "on_its_way"
        } else if !self.aired {
            "upcoming"
        } else if self.wanted {
            "wanted"
        } else {
            "none"
        }
    }
}

struct Show {
    id: MediaId,
    title: String,
    year: Option<u16>,
    network: Option<String>,
    summary: Option<String>,
    status: Option<&'static str>,
    following: bool,
    rows: Vec<Row>,
    /// Season counts from an import, when no episode list is known.
    snapshot: Vec<SeasonInfo>,
    /// Minutes per episode, when known, for the size estimates.
    runtime: Option<u32>,
    /// The quality profile of the show in the catalog, and whether the catalog has it.
    profile: Option<String>,
    tracked: bool,
    /// The ids Plex can look the show up by, for its review scores.
    tvdb: Option<u32>,
    imdb: Option<String>,
    genres: Vec<String>,
    cast: Vec<metadata::CastName>,
}

fn cast_json(cast: &[metadata::CastName]) -> Vec<Value> {
    cast.iter()
        .map(|c| json!({ "name": c.name, "character": c.character }))
        .collect()
}

fn status_word(raw: Option<&str>) -> Option<&'static str> {
    match raw?.to_ascii_lowercase().as_str() {
        "running" | "continuing" => Some("Running"),
        "ended" => Some("Ended"),
        _ => None,
    }
}

/// Progress of the series' active downloads, by each target they cover.
async fn active_progress(
    st: &AppState,
    series: MediaId,
) -> Result<HashMap<String, Option<f64>>, ApiError> {
    let sid = series.to_string();
    let grabs = st
        .store
        .with(move |c| downloads::select(c, "media_id = ?1 AND active = 1", &[&sid]))
        .await
        .map_err(internal("downloads"))?;
    let mut out = HashMap::new();
    for g in &grabs {
        let p = g.progress.map(|p| p.clamp(0.0, 1.0));
        if g.targets.is_empty() {
            out.insert(g.target.to_string(), p);
        }
        for t in &g.targets {
            out.insert(t.target.to_string(), p);
        }
    }
    Ok(out)
}

async fn load_show(st: &Arc<AppState>, key: SeriesKey) -> Result<Show, ApiError> {
    let now = OffsetDateTime::now_utc();
    let index = st.index();
    let catalog: Option<Series> = index.series(key).cloned();
    // The show as TVmaze knows it: the only source for a show outside the catalog, and the
    // summary, network and status of one inside it.
    let (target, show) = match &catalog {
        None => {
            let (t, s) = st.resolver(&index, now).series(key).await?;
            (Some(t), s)
        }
        Some(s) => {
            let looked = match (&st.tvmaze, s.tvmaze_id, key) {
                (Some(tv), Some(id), _) | (Some(tv), None, SeriesKey::Tvmaze(id)) => {
                    tv.by_id(id).await
                }
                (Some(tv), None, SeriesKey::Tvdb(id)) => tv.by_tvdb(id).await,
                (None, _, _) => Ok(None),
            };
            (None, looked.ok().flatten())
        }
    };
    let id = match (&catalog, &target) {
        (Some(s), _) => s.id,
        (None, Some(t)) => t.id,
        (None, None) => MediaId::Series(key),
    };
    let (title, year) = match (&catalog, &target) {
        (Some(s), _) => (s.title.clone(), s.year),
        (None, Some(t)) => (t.title.clone(), t.year),
        (None, None) => (format!("Show {key}"), None),
    };
    // Episodes: the catalog's list when it has one, else TVmaze's (nothing owned or wanted).
    let (eps, own_flags): (Option<Vec<Episode>>, bool) = match &catalog {
        Some(s) if s.episodes.is_some() => (s.episodes.clone(), s.monitored),
        _ => {
            let listed = match (&st.tvmaze, &show) {
                (Some(tv), Some(sh)) => {
                    Some(tv.episodes(sh.id).await.map_err(|e| e.into_api("tvmaze"))?)
                }
                _ => None,
            };
            (listed.map(|l| catalog_episodes(&l)), false)
        }
    };
    let snapshot: Vec<SeasonInfo> = match (&eps, &catalog) {
        (None, Some(s)) => s.seasons.iter().filter(|x| x.number > 0).cloned().collect(),
        _ => vec![],
    };
    if eps.is_none() && snapshot.is_empty() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            ErrorCode::MetadataUnavailable,
            format!("no episode list for {id}"),
        ));
    }
    let active = active_progress(st, id).await?;
    let skey = id.series_key().unwrap_or(key);
    let rows = eps
        .into_iter()
        .flatten()
        .filter(|e| e.season > 0)
        .map(|e| {
            let eid = library::episode_id(id, e.season, e.number).to_string();
            let sid = MediaId::Season(skey, e.season).to_string();
            let progress = active.get(&eid).or_else(|| active.get(&sid)).copied();
            let in_plex = e.has_file;
            Row {
                season: e.season,
                number: e.number,
                title: e.title.clone().filter(|t| !t.trim().is_empty()),
                air: e.air_date_utc.as_deref().and_then(day),
                aired: aired_at(&e, now),
                in_plex,
                on_way: !in_plex && (progress.is_some() || e.queued),
                // `own_flags`: the series is acquired here (its flag already folds in the mode).
                wanted: own_flags && e.monitored && !in_plex,
                progress: progress.flatten(),
            }
        })
        .collect();
    let following = st
        .store
        .with(move |c| library::following(c, id))
        .await
        .map_err(internal("following"))?;
    let runtime = catalog
        .as_ref()
        .and_then(|s| s.runtime_minutes)
        .or_else(|| target.as_ref().and_then(|t| t.runtime_minutes))
        .or_else(|| show.as_ref().and_then(|s| s.runtime.or(s.average_runtime)))
        .filter(|m| *m > 0);
    let tvdb = match key {
        SeriesKey::Tvdb(n) => Some(n),
        SeriesKey::Tvmaze(_) => None,
    }
    .or_else(|| catalog.as_ref().map(|s| s.tvdb_id))
    .or_else(|| target.as_ref().and_then(|t| t.tvdb_id))
    .or_else(|| show.as_ref().and_then(|s| s.externals.thetvdb));
    let imdb = catalog
        .as_ref()
        .and_then(|s| s.imdb_id.clone())
        .or_else(|| show.as_ref().and_then(|s| s.externals.imdb.clone()));
    let mut cast = show.as_ref().map(|s| s.cast()).unwrap_or_default();
    if let (true, Some(tv), Some(sh)) = (
        cast.is_empty(),
        &st.tvmaze,
        show.as_ref().filter(|s| s.embedded.is_none()),
    ) {
        // A show found by its TVDB id comes without its cast: one cached lookup by TVmaze id.
        if let Ok(Some(full)) = tv.by_id(sh.id).await {
            cast = full.cast();
        }
    }
    let genres = metadata::clean_genres(
        show.iter()
            .flat_map(|s| s.genres.iter().map(String::as_str)),
        6,
    );
    Ok(Show {
        tvdb,
        imdb,
        genres,
        cast,
        runtime,
        profile: catalog.as_ref().and_then(|s| s.quality_profile.clone()),
        tracked: catalog.is_some(),
        id,
        title,
        year,
        network: show.as_ref().and_then(|s| s.network_name()),
        summary: show
            .as_ref()
            .and_then(|s| s.summary.as_deref())
            .and_then(|s| metadata::plain_summary(s, 400)),
        status: show
            .as_ref()
            .and_then(|s| status_word(s.status.as_deref()))
            .or_else(|| {
                catalog
                    .as_ref()
                    .and_then(|s| status_word(s.status.as_deref()))
            }),
        following,
        rows,
        snapshot,
    })
}

/// Counts and the one-word state of a season, from its episodes.
fn season_view(n: u16, rows: &[&Row]) -> Value {
    let total = rows.len();
    let aired = rows.iter().filter(|r| r.aired).count();
    let in_plex = rows.iter().filter(|r| r.in_plex).count();
    let on_way = rows.iter().filter(|r| r.on_way).count();
    let wanted = rows
        .iter()
        .filter(|r| r.wanted && !r.on_way && r.aired)
        .count();
    let missing = rows
        .iter()
        .filter(|r| r.aired && !r.in_plex && !r.on_way)
        .count();
    let next = rows
        .iter()
        .filter(|r| !r.aired && !r.in_plex)
        .filter_map(|r| r.air.clone())
        .min();
    let progress: Vec<f64> = rows.iter().filter_map(|r| r.progress).collect();
    let progress =
        (!progress.is_empty()).then(|| progress.iter().sum::<f64>() / progress.len() as f64);
    let state = if on_way > 0 {
        "on_its_way"
    } else if aired > 0 && in_plex >= aired && total == aired {
        "in_plex"
    } else if aired == 0 && in_plex == 0 {
        "upcoming"
    } else if in_plex > 0 {
        "partly"
    } else if wanted > 0 {
        "wanted"
    } else {
        "none"
    };
    json!({
        "number": n, "episode_count": total, "aired_count": aired, "in_plex": in_plex,
        "on_its_way": on_way, "wanted": wanted, "missing": missing,
        "next_air_date": next, "progress": progress, "state": state,
    })
}

fn series_key_of(id: MediaId) -> Result<SeriesKey, ApiError> {
    match id {
        MediaId::Series(k) => Ok(k),
        _ => Err(ApiError::bad_request(
            ErrorCode::InvalidMediaId,
            "expected a series id",
        )),
    }
}

/// `GET /show/{media_id}`: everything the show page needs in one call.
pub(super) async fn show(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    st.loaded()?;
    let key = series_key_of(media_id(p)?)?;
    // Review scores are optional and never slow the page past their budget. When the id in the
    // address is already a TVDB id the lookup starts alongside the page's own work.
    let early = match key {
        SeriesKey::Tvdb(n) => Some((Some(n), None)),
        SeriesKey::Tvmaze(_) => None,
    };
    let lookup = |ids: (Option<u32>, Option<String>)| {
        let st = st.clone();
        async move {
            match &st.movies {
                Some(m) => m.show_scores(&[ids], SCORES_BUDGET).await.pop().flatten(),
                None => None,
            }
        }
    };
    let started = early.is_some();
    let (s, early_scores) = tokio::join!(load_show(&st, key), async {
        match early {
            Some(ids) => lookup(ids).await,
            None => None,
        }
    });
    let s = s?;
    let scores = if started {
        early_scores
    } else {
        lookup((s.tvdb, s.imdb.clone())).await
    };
    let mut numbers: Vec<u16> = s.rows.iter().map(|r| r.season).collect();
    numbers.extend(s.snapshot.iter().map(|x| x.number));
    numbers.sort_unstable();
    numbers.dedup();
    let seasons: Vec<Value> = numbers
        .into_iter()
        .map(|n| {
            let rows: Vec<&Row> = s.rows.iter().filter(|r| r.season == n).collect();
            match s.snapshot.iter().find(|x| x.number == n) {
                Some(x) if rows.is_empty() => {
                    let total = x.total_episode_count.or(x.episode_count).unwrap_or(0);
                    let owned = x.episode_file_count.unwrap_or(0);
                    let state = if total > 0 && owned >= total {
                        "in_plex"
                    } else if owned > 0 {
                        "partly"
                    } else {
                        "none"
                    };
                    json!({
                        "number": n, "episode_count": total, "aired_count": total, "in_plex": owned,
                        "on_its_way": 0, "wanted": 0, "missing": total.saturating_sub(owned),
                        "next_air_date": null, "progress": null, "state": state,
                    })
                }
                _ => season_view(n, &rows),
            }
        })
        .collect();
    let mut v = json!({
        "media_id": s.id, "title": s.title, "year": s.year, "network": s.network,
        "summary": s.summary, "status": s.status, "following": s.following, "seasons": seasons,
        "scores": metadata::scores_json(scores),
        "runtime_minutes": s.runtime, "genres": s.genres, "cast": cast_json(&s.cast),
    });
    let l = st.loaded()?;
    options::page_fields(
        &mut v,
        &l.settings,
        MediaKind::Series,
        s.profile.as_deref(),
        s.tracked,
        s.runtime,
    );
    ok(v)
}

/// `GET /show/{media_id}/season/{n}`: the episodes of one season.
pub(super) async fn season(
    State(st): State<Arc<AppState>>,
    p: Result<Path<(String, u16)>, PathRejection>,
) -> ApiResult {
    st.loaded()?;
    let Path((raw, n)) =
        p.map_err(|e| ApiError::bad_request(ErrorCode::InvalidMediaId, e.body_text()))?;
    let key = series_key_of(parse_media_id(&raw)?)?;
    let s = load_show(&st, key).await?;
    let skey = s.id.series_key().unwrap_or(key);
    let mut rows: Vec<&Row> = s.rows.iter().filter(|r| r.season == n).collect();
    if rows.is_empty() && !s.snapshot.iter().any(|x| x.number == n) {
        return Err(ApiError::not_found(format!("no season {n} for {}", s.id)));
    }
    rows.sort_by_key(|r| r.number);
    let episodes: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "media_id": MediaId::Episode(skey, n, r.number), "number": r.number,
                "title": r.title, "air_date": r.air, "state": r.state(),
                "progress": if r.on_way { r.progress } else { None },
            })
        })
        .collect();
    ok(json!({ "number": n, "episodes": episodes }))
}

/// `GET /movie/{media_id}`: state and details for the movie page.
pub(super) async fn movie(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    st.loaded()?;
    let MediaId::Movie { tmdb } = media_id(p)? else {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidMediaId,
            "expected a movie id",
        ));
    };
    let id = MediaId::Movie { tmdb };
    let now = OffsetDateTime::now_utc();
    let index = st.index();
    let catalog = index.movie(tmdb).cloned();
    let meta = match &st.movies {
        Some(m) => m.movie(tmdb).await.ok().flatten(),
        None => None,
    };
    let (title, year, runtime) = match (&catalog, &meta) {
        (Some(m), _) => (
            m.title.clone(),
            m.year,
            m.runtime_minutes.or(meta.as_ref().and_then(|x| x.runtime)),
        ),
        (None, Some(x)) => (
            x.title.clone(),
            metadata::year_of(x.release_date.as_deref()),
            x.runtime,
        ),
        (None, None) => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                ErrorCode::MetadataUnavailable,
                format!("no details for {id}"),
            ));
        }
    };
    let sid = id.to_string();
    let grabs = st
        .store
        .with(move |c| downloads::select(c, "media_id = ?1 AND active = 1", &[&sid]))
        .await
        .map_err(internal("downloads"))?;
    let (mut state, mut until, mut progress) = ("none", None, None);
    if let Some(m) = &catalog {
        if m.has_file {
            state = "in_plex";
        } else if !grabs.is_empty() || m.queued {
            state = "on_its_way";
            progress = grabs
                .first()
                .and_then(|g| g.progress)
                .map(|p| p.clamp(0.0, 1.0));
        } else if m.monitored {
            match m.availability(now, st.availability_delay_days()) {
                Availability::NotYet(at) => {
                    state = "waiting";
                    until = day(&at);
                }
                _ => state = "wanted",
            }
        }
    }
    let scores = meta.as_ref().and_then(|m| m.scores);
    let genres = meta.as_ref().map(|m| m.genre_names()).unwrap_or_default();
    let cast = meta.as_ref().map(|m| m.cast()).unwrap_or_default();
    let summary = meta
        .and_then(|m| m.overview)
        .map(|t| cut(t.trim(), 400))
        .filter(|t| !t.is_empty());
    let mut v = json!({
        "media_id": id, "title": title, "year": year, "runtime_minutes": runtime,
        "summary": summary, "state": state, "progress": progress, "until": until,
        "scores": metadata::scores_json(scores),
        "genres": genres, "cast": cast_json(&cast),
    });
    let l = st.loaded()?;
    options::page_fields(
        &mut v,
        &l.settings,
        MediaKind::Movie,
        catalog.as_ref().and_then(|m| m.quality_profile.as_deref()),
        catalog.is_some(),
        runtime,
    );
    ok(v)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FollowBody {
    media_id: String,
    on: bool,
    /// smaller, standard or best: the quality the person picked on the page.
    #[serde(default)]
    level: Option<String>,
}

/// `POST /follow`: get a series' new episodes as they air, or stop doing that.
pub(super) async fn follow(State(st): St, ApiJson(body): ApiJson<FollowBody>) -> ApiResult {
    let _guard = st.library_lock.lock().await;
    let l = st.loaded()?;
    let id = parse_media_id(&body.media_id)?;
    let key = series_key_of(id)?;
    let profile = match (&body.level, body.on) {
        (Some(l), true) => Some(options::level_profile(&st, id, l)?),
        _ => None,
    };
    if let Some(p) = &profile {
        // The library lock is already held above.
        options::set_profile(&st, id, p).await?;
    }
    let now = OffsetDateTime::now_utc();
    let index = st.index();
    let known = index.series(key).map(|s| s.id);
    let managed_now = match known {
        Some(sid) => {
            st.store
                .with(move |c| library::is_managed(c, &sid.to_string()))
                .await
                .map_err(internal("follow"))?
                == Some(true)
        }
        None => false,
    };
    let sid = if body.on {
        if managed_now {
            known.unwrap_or(id)
        } else {
            let add = AddBody {
                media_id: id.to_string(),
                monitored: None,
                monitor: None,
                quality_profile: profile,
                root_folder: None,
                search: false,
                metadata: None,
                native_handoff_confirmed: None,
                preserve_monitoring: None,
            };
            ensure_series(&st, &l, key, &add, MonitorMode::None, false, now)
                .await?
                .0
        }
    } else if managed_now {
        known.unwrap_or(id)
    } else {
        return ok(json!({ "media_id": known.unwrap_or(id), "following": false }));
    };
    let on = body.on;
    st.store
        .with(move |c| library::set_following(c, sid, on, now))
        .await
        .map_err(internal("follow"))?;
    st.reload_index().await.map_err(internal("index"))?;
    ok(json!({ "media_id": sid, "following": on }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StopBody {
    media_id: String,
}

/// Whether a download target lies inside the stopped scope.
fn within(scope: MediaId, t: MediaId) -> bool {
    match (scope, t) {
        (MediaId::Season(k, n), MediaId::Season(k2, n2) | MediaId::Episode(k2, n2, _)) => {
            k == k2 && n == n2
        }
        (scope, t) => scope == t,
    }
}

/// `POST /stop`: Arr stops looking for a movie, season or episode, and cancels its
/// downloads that have not finished. Files already in the library are never touched.
pub(super) async fn stop(State(st): St, ApiJson(body): ApiJson<StopBody>) -> ApiResult {
    st.loaded()?;
    let id = parse_media_id(&body.media_id)?;
    if matches!(id, MediaId::Series(_)) {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidMediaId,
            "stop a movie, a season or an episode",
        ));
    }
    let now = OffsetDateTime::now_utc();
    // Stored ids use the series key the catalog knows the show by.
    let index = st.index();
    let canon = |k: SeriesKey| index.series(k).and_then(|s| s.id.series_key()).unwrap_or(k);
    let scope = match id {
        MediaId::Season(k, n) => MediaId::Season(canon(k), n),
        MediaId::Episode(k, n, e) => MediaId::Episode(canon(k), n, e),
        other => other,
    };
    let parent = plan_media(scope);
    {
        let _guard = st.library_lock.lock().await;
        let changed = st
            .store
            .with(move |c| library::set_monitored(c, scope, false, now))
            .await
            .map_err(internal("monitor"))?;
        if !changed {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                ErrorCode::NotManaged,
                format!("{scope} is not something Arr is getting"),
            ));
        }
        // A request that was still retrying is over too.
        st.store
            .with(move |c| {
                for r in worker::list_requests(c, true)? {
                    if let Some(t) = r["target"].as_str().and_then(|t| t.parse::<MediaId>().ok())
                        && within(scope, t)
                    {
                        worker::settle_request(c, t, "cancelled", "stopped by a person", now)?;
                    }
                }
                Ok(())
            })
            .await
            .map_err(internal("requests"))?;
        st.reload_index().await.map_err(internal("index"))?;
    }
    let pid = parent.to_string();
    let grabs = st
        .store
        .with(move |c| downloads::select(c, "media_id = ?1 AND active = 1", &[&pid]))
        .await
        .map_err(internal("downloads"))?;
    let (mut cancelled, mut finishing) = (0, 0);
    for g in grabs {
        let targets: Vec<MediaId> = if g.targets.is_empty() {
            vec![g.target]
        } else {
            g.targets.iter().map(|t| t.target).collect()
        };
        // Only a download that lies entirely inside the scope: a season pack that also holds
        // other episodes is left to finish.
        if !targets.iter().all(|t| within(scope, *t)) {
            continue;
        }
        // One that already has all its data is being added to Plex: it is left to finish.
        // That is decided under the grab lock, inside the cancel.
        let r = downloads::cancel(
            &st,
            &g.id,
            CancelOptions {
                blocklist: false,
                removal: Removal::TorrentAndData,
                ignore_seed_criteria: true,
                research: false,
                only_unfinished: true,
            },
        )
        .await;
        match r {
            Ok(_) => cancelled += 1,
            Err(downloads::ActionError::Conflict("finishing", _)) => finishing += 1,
            Err(e) => {
                return Err(action_error(
                    e,
                    st.cfg.download_backend().unwrap_or("download_client"),
                ));
            }
        }
    }
    ok(
        json!({ "media_id": scope, "stopped": true, "cancelled": cancelled, "finishing": finishing }),
    )
}

/// A season or episode quick add's reply, in the app's words.
pub(super) async fn quick_scope(
    st: &Arc<AppState>,
    id: MediaId,
    profile: Option<String>,
) -> ApiResult {
    let v = add_value(st, id.to_string(), None, true, profile).await?;
    let scope: MediaId = v["scope"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(id);
    let index = st.index();
    let (title, _) = feed_title(&index, scope);
    let (outcome, until, message) = quick_outcome(&v);
    ok(json!({
        "media_id": scope, "title": title, "detail": feed_detail(&index, &[scope]),
        "outcome": outcome, "until": until, "message": message,
    }))
}
