//! Sport in the API: search the built-in leagues and the teams Arr has seen, follow or
//! unfollow a league or a team, and list the events of what is followed. Everything answers
//! from the store; the schedule source is only asked when a league is opened or followed and
//! its stored schedule is old, and then for a few seconds at most (the read carries on in the
//! background). A source that is down never turns an answer into an error: the stored
//! schedule is served and `schedule.problem` says what is wrong.

use super::*;
use crate::sport::{self, catalog, data, refresh};

/// The most a request waits for the schedule source before answering from what is stored.
const BUDGET: Duration = Duration::from_secs(8);

/// This request's budget: four fifths of the source's own timeout, at most `BUDGET` (the
/// default timeout of ten seconds gives eight). One deadline serves every stage of a request.
fn budget(st: &AppState) -> Duration {
    st.cfg
        .sports
        .as_ref()
        .map_or(BUDGET, |c| budget_for(c.timeout))
}

/// Four fifths of the source's timeout, at most `BUDGET`.
fn budget_for(timeout: Duration) -> Duration {
    (timeout * 4 / 5).min(BUDGET)
}

/// What is left of a request's deadline.
fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}
/// A league followed or opened within this long of its last read is not read again.
const RECENT: Duration = Duration::from_secs(20 * 60);
const SEARCH_LEAGUES: usize = 12;
const SEARCH_TEAMS: usize = 20;
const LEAGUE_UPCOMING: usize = 30;
const LEAGUE_RECENT: usize = 10;
const EVENTS_LIMIT: usize = 500;
const FEED_LIMIT: usize = 20;
const FEED_DAYS: u64 = 14;

fn disabled() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::ProviderNotConfigured,
        "sport is turned off ([sports] enabled = false in config.toml)",
    )
}

fn sports(st: &AppState) -> Result<&Arc<sport::Sports>, ApiError> {
    st.sports.as_ref().ok_or_else(disabled)
}

/// `nba` or `league:nba`.
fn league_of(raw: &str) -> Result<&'static catalog::League, ApiError> {
    let slug = raw.trim().strip_prefix("league:").unwrap_or(raw.trim());
    catalog::find(slug).ok_or_else(|| {
        ApiError::not_found(format!(
            "no such league {slug:?}; GET /api/v1/sport/leagues lists them"
        ))
    })
}

fn team_id_of(raw: &str) -> Result<&str, ApiError> {
    let ok = !raw.is_empty() && raw.len() <= 18 && raw.bytes().all(|b| b.is_ascii_digit());
    ok.then_some(raw).ok_or_else(|| {
        ApiError::bad_request(
            ErrorCode::InvalidRequest,
            "team_id must be the number of a team",
        )
    })
}

fn date_or_time(raw: &str, what: &str) -> Result<OffsetDateTime, ApiError> {
    OffsetDateTime::parse(raw, &Rfc3339)
        .ok()
        // An offset can push a time past the last year that exists: that is a bad time, not
        // a panic.
        .and_then(|t| t.checked_to_offset(time::UtcOffset::UTC))
        .or_else(|| {
            let f = time::macros::format_description!("[year]-[month]-[day]");
            time::Date::parse(raw, &f)
                .ok()
                .map(|d| d.midnight().assume_utc())
        })
        .ok_or_else(|| {
            ApiError::bad_request(
                ErrorCode::InvalidQuery,
                format!("{what} must be an RFC 3339 timestamp or a YYYY-MM-DD date"),
            )
        })
}

fn followed_by(follows: &[data::FollowRow], league: &str) -> (bool, Vec<data::FollowRow>) {
    let mine: Vec<data::FollowRow> = follows
        .iter()
        .filter(|f| f.league == league)
        .cloned()
        .collect();
    (mine.iter().any(|f| f.team_id.is_none()), mine)
}

fn league_with_follow(l: &catalog::League, follows: &[data::FollowRow]) -> Value {
    let (whole, mine) = followed_by(follows, l.slug);
    let mut v = sport::league_json(l);
    v["followed"] = json!(whole);
    v["followed_teams"] = json!(mine.iter().filter(|f| f.team_id.is_some()).count());
    v
}

fn team_json(t: &data::TeamRow, followed: bool) -> Value {
    json!({
        "id": t.id, "name": t.name, "short_name": t.short_name,
        "abbreviation": t.abbreviation, "followed": followed,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SearchQuery {
    q: Option<String>,
}

/// Leagues from the built-in list and teams from the leagues Arr has read so far.
pub(super) async fn search(State(st): St, ApiQuery(q): ApiQuery<SearchQuery>) -> ApiResult {
    sports(&st)?;
    let text = text_query(q.q.as_deref(), true)?.expect("required");
    let words = crate::title::tokens(&text);
    let key = crate::title::key(&text);
    let (follows, teams) = st
        .store
        .with(|c| Ok((data::follows(c)?, data::all_teams(c)?)))
        .await
        .map_err(internal("sport search"))?;
    let leagues: Vec<Value> = catalog::search(&text)
        .into_iter()
        .take(SEARCH_LEAGUES)
        .map(|l| league_with_follow(l, &follows))
        .collect();
    let mut hits: Vec<(u8, &data::TeamRow)> = vec![];
    for t in &teams {
        if catalog::find(&t.league).is_none() {
            continue;
        }
        let names: Vec<&str> = [
            Some(t.name.as_str()),
            t.short_name.as_deref(),
            t.abbreviation.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        if names.iter().any(|n| crate::title::key(n) == key) {
            hits.push((0, t));
        } else {
            let hay: Vec<String> = names.iter().flat_map(|n| crate::title::tokens(n)).collect();
            if words
                .iter()
                .all(|w| hay.iter().any(|h| h.starts_with(w.as_str())))
            {
                hits.push((1, t));
            }
        }
    }
    hits.sort_by(|a, b| (a.0, &a.1.name, &a.1.league).cmp(&(b.0, &b.1.name, &b.1.league)));
    let teams: Vec<Value> = hits
        .into_iter()
        .take(SEARCH_TEAMS)
        .filter_map(|(_, t)| {
            let l = catalog::find(&t.league)?;
            let followed = follows
                .iter()
                .any(|f| f.league == t.league && f.team_id.as_deref() == Some(t.id.as_str()));
            Some(json!({ "league": sport::league_json(l), "team": team_json(t, followed) }))
        })
        .collect();
    ok(json!({ "q": text, "leagues": leagues, "teams": teams }))
}

/// Every league in the built-in list.
pub(super) async fn leagues(State(st): St) -> ApiResult {
    sports(&st)?;
    let follows = st
        .store
        .with(|c| data::follows(c))
        .await
        .map_err(internal("sport leagues"))?;
    let list: Vec<Value> = catalog::LEAGUES
        .iter()
        .map(|l| league_with_follow(l, &follows))
        .collect();
    ok(json!({ "leagues": list }))
}

/// One league: its teams, what is coming up and what was played, and whether it is followed.
pub(super) async fn league(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    let sports = sports(&st)?.clone();
    let Path(raw) =
        p.map_err(|e| ApiError::bad_request(ErrorCode::InvalidRequest, e.body_text()))?;
    let league = league_of(&raw)?;
    // Brings the schedule up to date if it is old, for a few seconds at most.
    let fresh = refresh::ensure_fresh_within(
        st.store.clone(),
        sports,
        league,
        refresh::STALE,
        budget(&st),
    )
    .await;
    let now = OffsetDateTime::now_utc();
    let slug = league.slug;
    let (follows, teams, upcoming, recent, state) = st
        .store
        .with(move |c| {
            Ok((
                data::follows(c)?,
                data::teams_of(c, slug)?,
                data::league_events(c, slug, true, now, LEAGUE_UPCOMING)?,
                data::league_events(c, slug, false, now, LEAGUE_RECENT)?,
                data::refresh_state(c, slug)?,
            ))
        })
        .await
        .map_err(internal("sport league"))?;
    let (_, mine) = followed_by(&follows, slug);
    let following_teams: Vec<Value> = mine
        .iter()
        .filter_map(|f| Some(json!({ "id": f.team_id.as_ref()?, "name": f.team_name })))
        .collect();
    // What went wrong on the last try, as the refresh recorded it.
    let problem = state
        .last_error
        .clone()
        .filter(|_| state.failures > 0)
        .or(match &fresh {
            refresh::Freshness::Failed(why) | refresh::Freshness::Backoff(why) => Some(why.clone()),
            _ => None,
        });
    ok(json!({
        "league": league_with_follow(league, &follows),
        "following_teams": following_teams,
        "teams": teams.iter().map(|t| team_json(t, mine.iter().any(|f| f.team_id.as_deref() == Some(t.id.as_str())))).collect::<Vec<_>>(),
        "upcoming": upcoming.iter().map(sport::event_json).collect::<Vec<_>>(),
        "recent": recent.iter().map(sport::event_json).collect::<Vec<_>>(),
        "schedule": {
            "updated_at": state.refreshed.map(rfc3339),
            "updating": matches!(fresh, refresh::Freshness::Pending),
            "problem": problem,
        },
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FollowBody {
    /// `nba` or `league:nba`.
    league: String,
    /// A team's number in that league (from the league page or a search). Leave out to follow
    /// the whole league.
    team_id: Option<String>,
}

/// Follows a league, or one team in it. Following starts now: nothing earlier is wanted.
pub(super) async fn follow(State(st): St, ApiJson(b): ApiJson<FollowBody>) -> ApiResult {
    let sports = sports(&st)?.clone();
    // One deadline for the whole request: the team list and the schedule share it.
    let deadline = Instant::now() + budget(&st);
    let league = league_of(&b.league)?;
    let team_id = b.team_id.as_deref().map(team_id_of).transpose()?;
    if team_id.is_some() && !league.teams {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidRequest,
            format!(
                "{} has no teams; follow the whole league (leave team_id out)",
                league.name
            ),
        ));
    }
    let slug = league.slug;
    let team = match team_id {
        None => None,
        Some(id) => {
            let find = |st: Arc<AppState>, id: String| async move {
                st.store.with(move |c| data::team(c, slug, &id)).await
            };
            let mut found = find(st.clone(), id.to_string())
                .await
                .map_err(internal("sport follow"))?;
            if found.is_none() {
                // The league's team list has not been read yet.
                let read = refresh::ensure_teams_within(
                    st.store.clone(),
                    sports.clone(),
                    league,
                    remaining(deadline),
                )
                .await;
                found = find(st.clone(), id.to_string())
                    .await
                    .map_err(internal("sport follow"))?;
                if found.is_none() {
                    // Only a list that was read successfully and is still current can say a
                    // team is not in the league; without one (never read, stale and the
                    // refresh failing, slow or being waited out) the answer is "try again",
                    // not "no such team".
                    let listed = st
                        .store
                        .with(move |c| {
                            Ok(refresh::teams_current(
                                &data::refresh_state(c, slug)?,
                                OffsetDateTime::now_utc(),
                            ))
                        })
                        .await
                        .map_err(internal("sport follow"))?;
                    return Err(if listed {
                        ApiError::not_found(format!("{} has no team numbered {id}", league.name))
                    } else {
                        let why = read
                            .err()
                            .unwrap_or_else(|| "the team list has not been read yet".to_string());
                        ApiError::new(
                            StatusCode::SERVICE_UNAVAILABLE,
                            ErrorCode::UpstreamUnavailable,
                            format!(
                                "Arr could not check the team list right now ({why}); try again in a moment"
                            ),
                        )
                    });
                }
            }
            found
        }
    };
    let now = OffsetDateTime::now_utc();
    let (row, was_new) = {
        let team = team.clone();
        st.store
            .with(move |c| {
                data::follow(
                    c,
                    slug,
                    team.as_ref().map(|t| (t.id.as_str(), t.name.as_str())),
                    now,
                )
            })
            .await
            .map_err(internal("sport follow"))?
    };
    // The first thing a person wants after following is to see what is coming up: read the
    // schedule now (a few seconds at most) unless it was read a moment ago.
    let fresh = refresh::ensure_fresh_within(
        st.store.clone(),
        sports,
        league,
        RECENT,
        remaining(deadline),
    )
    .await;
    // The refresh may have corrected the team's label: answer with the label every other route
    // shows now (the stored `since` is unchanged).
    let row = {
        let (l, t) = (row.league.clone(), row.team_id.clone());
        st.store
            .with(move |c| {
                Ok(data::follows(c)?
                    .into_iter()
                    .find(|f| f.league == l && f.team_id == t))
            })
            .await
            .map_err(internal("sport follow"))?
            .unwrap_or(row)
    };
    let now = OffsetDateTime::now_utc();
    let mine = row.clone();
    let next = st
        .store
        .with(move |c| data::next_event_of(c, &mine, now))
        .await
        .map_err(internal("sport follow"))?;
    let counted = {
        let mine = row.clone();
        st.store
            .with(move |c| {
                data::followed_events(
                    c,
                    now,
                    now + time::Duration::days(21),
                    Some(&mine.league),
                    Some(&["scheduled", "postponed"]),
                    EVENTS_LIMIT,
                )
            })
            .await
            .map_err(internal("sport follow"))?
            .into_iter()
            .filter(|e| {
                row.team_id.as_deref().is_none_or(|t| {
                    e.home_id.as_deref() == Some(t) || e.away_id.as_deref() == Some(t)
                })
            })
            .count()
    };
    let body = json!({
        "created": was_new,
        "follow": sport::follow_json(&row),
        "schedule_read": matches!(fresh, refresh::Freshness::Fresh | refresh::Freshness::Refreshed(_)),
        "upcoming": counted,
        "next_event": next.as_ref().map(sport::event_json),
    });
    if was_new { created(body) } else { ok(body) }
}

/// Stops following a league or a team. What was already stored stays; following again later
/// starts from then.
pub(super) async fn unfollow(State(st): St, ApiJson(b): ApiJson<FollowBody>) -> ApiResult {
    sports(&st)?;
    let league = league_of(&b.league)?;
    let team_id = b.team_id.as_deref().map(team_id_of).transpose()?;
    let slug = league.slug;
    let team_id = team_id.map(str::to_string);
    let removed = st
        .store
        .with(move |c| data::unfollow(c, slug, team_id.as_deref()))
        .await
        .map_err(internal("sport unfollow"))?;
    ok(json!({ "removed": removed }))
}

/// Everything followed, each with its next event.
pub(super) async fn following(State(st): St) -> ApiResult {
    sports(&st)?;
    let now = OffsetDateTime::now_utc();
    let rows = st
        .store
        .with(move |c| {
            let mut out = vec![];
            for f in data::follows(c)? {
                let next = data::next_event_of(c, &f, now)?;
                out.push((f, next));
            }
            Ok(out)
        })
        .await
        .map_err(internal("sport following"))?;
    let list: Vec<Value> = rows
        .iter()
        .map(|(f, next)| {
            let mut v = sport::follow_json(f);
            v["next_event"] = next.as_ref().map_or(Value::Null, sport::event_json);
            v
        })
        .collect();
    ok(json!({ "following": list }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EventsQuery {
    from: Option<String>,
    to: Option<String>,
    league: Option<String>,
}

/// Events of what is followed that are not over by `from` and start before `to`.
pub(super) async fn events(State(st): St, ApiQuery(q): ApiQuery<EventsQuery>) -> ApiResult {
    sports(&st)?;
    let now = OffsetDateTime::now_utc();
    let from = q
        .from
        .as_deref()
        .map(|s| date_or_time(s, "from"))
        .transpose()?
        .unwrap_or(now);
    let to = match q.to.as_deref() {
        Some(s) => date_or_time(s, "to")?,
        None => from.checked_add(time::Duration::days(14)).ok_or_else(|| {
            ApiError::bad_request(
                ErrorCode::InvalidQuery,
                "from is too late to have a default end; give to as well",
            )
        })?,
    };
    if to <= from || to - from > time::Duration::days(92) {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidQuery,
            "to must be after from and at most 92 days later",
        ));
    }
    let slug = q
        .league
        .as_deref()
        .map(league_of)
        .transpose()?
        .map(|l| l.slug);
    let list = st
        .store
        .with(move |c| data::followed_events(c, from, to, slug, None, EVENTS_LIMIT))
        .await
        .map_err(internal("sport events"))?;
    ok(json!({
        "from": rfc3339(from), "to": rfc3339(to),
        "events": list.iter().map(sport::event_json).collect::<Vec<_>>(),
    }))
}

/// The most a search for the copies of an event may take, whatever the indexers do.
/// The most stored events of a league that are looked at around a card or a tournament. More
/// than this and the list is cut, and a cut list is not a complete one.
const NEIGHBOURS_LIMIT: usize = 500;
const COPIES_CEILING: Duration = Duration::from_secs(20);

/// How long a search for the copies of an event may take: twice the schedule source's own
/// timeout (the indexers answer concurrently, each within its own), at most [`COPIES_CEILING`]
/// and at least two seconds. A slow indexer gives a partial answer rather than a wait.
fn copies_budget(st: &AppState) -> Duration {
    copies_budget_for(st.cfg.sports.as_ref().map_or(COPIES_CEILING, |c| c.timeout))
}

fn copies_budget_for(timeout: Duration) -> Duration {
    (timeout * 2).clamp(Duration::from_secs(2), COPIES_CEILING)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CopiesQuery {
    /// `main`, `prelims`, `early-prelims` or `r1`..`r4`.
    part: Option<String>,
    /// Ask the indexers again instead of using a recent answer.
    fresh: Option<bool>,
}

/// `event:ufc:600061400`, `ufc:600061400` or either with `:main` after it: the event's id and
/// the part the id names, if any.
fn event_id_of(raw: &str) -> Result<(String, Option<sport::Part>), ApiError> {
    let bad = || {
        ApiError::bad_request(
            ErrorCode::InvalidRequest,
            "an event id looks like event:<league>:<number>, optionally followed by :<part>",
        )
    };
    let rest = raw.trim().strip_prefix("event:").unwrap_or(raw.trim());
    let mut it = rest.split(':');
    let (slug, n, part) = (it.next(), it.next(), it.next());
    if it.next().is_some() {
        return Err(bad());
    }
    let (Some(slug), Some(n)) = (slug, n) else {
        return Err(bad());
    };
    let league = catalog::find(slug).ok_or_else(|| ApiError::not_found("no such league"))?;
    if n.is_empty() || n.starts_with('0') || !n.bytes().all(|b| b.is_ascii_digit()) || n.len() > 18
    {
        return Err(bad());
    }
    let part = part
        .map(|p| sport::Part::from_slug(p).ok_or_else(bad))
        .transpose()?;
    Ok((data::event_id(league.slug, n), part))
}

fn parts_of(league: &catalog::League) -> Vec<sport::Part> {
    match league.sport {
        catalog::Sport::Mma => vec![
            sport::Part::Main,
            sport::Part::Prelims,
            sport::Part::EarlyPrelims,
        ],
        catalog::Sport::Golf => (1..=4).map(sport::Part::Round).collect(),
        _ => vec![],
    }
}

fn part_slug(p: sport::Part) -> String {
    p.slug().unwrap_or_else(|| "whole".into())
}

/// One copy as the list shows it. Every field is a number, a time or a word from a closed list
/// (the part, the resolution and source, the language, the reason): **nothing is the release's
/// own text**, not its title, its uploader's tag, its channel or its words, so nothing a release
/// says (a result, a link, a hash) can reach a client. `label` is built from the closed words.
fn copy_json(c: &sport::find::Candidate, indexer: Option<String>) -> Value {
    let q = c.parsed.quality;
    let (resolution, source) = (options::resolution_word(q), options::source_word(q));
    let label = [
        Some(c.m.part.label()),
        resolution.map(str::to_string),
        source.map(str::to_string),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" \u{b7} ");
    // The publish time is the indexers' own field: it is returned only as a time, re-written.
    let published = c
        .release
        .publish_date
        .as_deref()
        .and_then(|d| OffsetDateTime::parse(d, &time::format_description::well_known::Rfc3339).ok())
        .map(rfc3339);
    json!({
        "id": c.release_ref,
        "part": part_slug(c.m.part),
        "part_label": c.m.part.label(),
        // A caution, not a verdict: absent does not mean Arr verified the copy.
        "caution": match c.m.confidence {
            sport::matcher::Confidence::Sure => Value::Null,
            sport::matcher::Confidence::Likely => {
                let reason = c.m.reason.unwrap_or("unverified_text");
                json!({ "reason": reason, "note": sport::caution_note(reason) })
            }
        },
        "resolution": resolution,
        "source": source,
        "fps": c.parsed.fps,
        "language": c.parsed.language,
        "size_bytes": c.release.size.filter(|s| *s > 0),
        "speed": options::speed_word(c.release.seeders),
        "seeders": c.release.seeders,
        "published_at": published,
        // (the configured name by the source's identity; never the release's own text)
        "indexer": indexer,
        "label": label,
    })
}

/// `GET /sport/event/{id}/copies`: the releases that are this event, found over the indexers
/// the shows use and judged by the sport matcher. Read only: it records no link, no grab and no
/// file, starts nothing and sends the person nothing to a download client. An indexer that
/// fails gives a partial answer (`searched.complete` false with `problem`), and the whole
/// request stops at a deadline with what it has.
pub(super) async fn event_copies(
    State(st): St,
    p: Result<Path<String>, PathRejection>,
    ApiQuery(q): ApiQuery<CopiesQuery>,
) -> ApiResult {
    let sports = sports(&st)?.clone();
    let Path(raw) =
        p.map_err(|e| ApiError::bad_request(ErrorCode::InvalidRequest, e.body_text()))?;
    let (id, id_part) = event_id_of(&raw)?;
    let query_part = q
        .part
        .as_deref()
        .map(|s| {
            sport::Part::from_slug(s).ok_or_else(|| {
                ApiError::bad_request(ErrorCode::InvalidRequest, "part is not one this event has")
            })
        })
        .transpose()?;
    if let (Some(a), Some(b)) = (id_part, query_part)
        && a != b
    {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidRequest,
            "the id and part name different parts",
        ));
    }
    let want = id_part.or(query_part);
    let id2 = id.clone();
    let ev = st
        .store
        .with(move |c| data::event(c, &id2))
        .await
        .map_err(internal("sport event"))?
        .ok_or_else(|| ApiError::not_found("no such event; GET /api/v1/sport/events lists them"))?;
    let league = catalog::find(&ev.league)
        .ok_or_else(|| ApiError::not_found("the event's league is not one Arr knows"))?;
    let parts = parts_of(league);
    if let Some(w) = want
        && !parts.contains(&w)
    {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidRequest,
            "this event has no such part",
        ));
    }
    let now = OffsetDateTime::now_utc();
    let head = |available: bool, note: Option<&str>| {
        json!({
            "event": sport::event_json(&ev),
            "parts": parts.iter().map(|p| json!({ "part": part_slug(*p), "label": p.label() })).collect::<Vec<_>>(),
            "available": available,
            "note": note,
        })
    };
    // Nothing to look for yet, or ever: say so without asking an indexer anything.
    if let Err(why) = sport::find::availability(&ev, league.sport, now) {
        let mut v = head(false, Some(why));
        v["copies"] = json!([]);
        v["skipped"] = json!([]);
        v["searched"] = json!({ "queries": 0, "indexers": 0, "complete": true, "problem": null });
        return ok(v);
    }
    let loaded = st.loaded()?;
    let slug = league.slug;
    let ev2 = ev.clone();
    let sport_kind = league.sport;
    let around = st
        .store
        .with(move |c| {
            let at = OffsetDateTime::parse(
                &ev2.starts_at,
                &time::format_description::well_known::Rfc3339,
            )
            .unwrap_or(now);
            let teams = data::teams_of(c, slug)?;
            // Complete is a recorded fact: the last read of the list was whole, and recent.
            let teams_complete = !teams.is_empty()
                && data::teams_read(c, slug)?.is_some_and(|(t, whole)| {
                    whole && now - t <= time::Duration::days(sport::find::TEAMS_FRESH_DAYS)
                });
            // The neighbours: for a game, the fixtures of the same two teams within three days
            // (a pair has a handful, so nothing is cut); for a card or a tournament, every
            // event of the league within three weeks, cut at a limit that is noticed.
            let (neighbours, neighbours_whole) = match (sport_kind, &ev2.home_id, &ev2.away_id) {
                (
                    catalog::Sport::Soccer
                    | catalog::Sport::Basketball
                    | catalog::Sport::RugbyUnion
                    | catalog::Sport::RugbyLeague,
                    Some(h),
                    Some(a),
                ) => (
                    data::pair_events_between(
                        c,
                        slug,
                        h,
                        a,
                        at - time::Duration::days(3),
                        at + time::Duration::days(3),
                    )?,
                    true,
                ),
                _ => {
                    let mut list = data::league_events_between(
                        c,
                        slug,
                        at - time::Duration::days(21),
                        at + time::Duration::days(21),
                        NEIGHBOURS_LIMIT + 1,
                    )?;
                    let whole = list.len() <= NEIGHBOURS_LIMIT;
                    list.truncate(NEIGHBOURS_LIMIT);
                    (list, whole)
                }
            };
            Ok(sport::find::Surroundings {
                teams_complete,
                spans: data::complete_spans(c, slug, now)?,
                neighbours_whole,
                teams,
                neighbours,
                earlier: data::league_events(c, slug, false, now, 200)?,
            })
        })
        .await
        .map_err(internal("sport surroundings"))?;
    // One search per event at a time, not again within a cooldown (whatever `fresh` says), and
    // a bounded number across all events (see `sport::gate`). The indexers are asked together,
    // and whatever has answered when the budget runs out is the answer. What is kept is the
    // search; it is judged again, below, against the event as it is now.
    let fresh = q.fresh.unwrap_or(false);
    let budget = copies_budget(&st);
    let served = sports
        .copies
        .serve(&ev.id, fresh, || {
            sport::find::search_event(
                st.search.as_ref(),
                &loaded.settings,
                &ev,
                league,
                fresh,
                budget,
            )
        })
        .await?;
    let (found, cached_age) = match served {
        sport::gate::Served::Searched(f) => (f, None),
        sport::gate::Served::Cached { found, age } => (found, Some(age)),
        sport::gate::Served::Busy { retry_after } => {
            return ok(busy_answer(
                head(true, Some("busy")),
                retry_after.as_secs() + 1,
                "Arr is looking up other events right now",
            ));
        }
    };
    // Judging is CPU work: off the async workers, a few requests at a time, and inside one
    // budget that counts the wait for a slot.
    let Some(found) = sport::find::judge_search_bounded(
        sports.judging.clone(),
        found,
        ev.clone(),
        league,
        around,
    )
    .await
    else {
        return ok(busy_answer(
            head(true, Some("busy")),
            sport::find::JUDGE_BUDGET.as_secs(),
            "Arr is judging other requests right now",
        ));
    };
    let found = found.for_part(want);
    let mut skipped: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (code, n) in &found.skipped {
        *skipped.entry(code.plain()).or_default() += n;
    }
    let mut skipped: Vec<(&str, usize)> = skipped.into_iter().collect();
    skipped.sort_by_key(|(reason, n)| (std::cmp::Reverse(*n), *reason));
    let mut v = head(true, None);
    v["copies"] = json!(
        found
            .candidates
            .iter()
            .map(|c| {
                copy_json(
                    c,
                    st.search
                        .as_ref()
                        .and_then(|b| b.configured_name(c.release.indexer_id)),
                )
            })
            .collect::<Vec<_>>()
    );
    v["skipped"] = json!(
        skipped
            .into_iter()
            .map(|(reason, count)| json!({ "reason": reason, "count": count }))
            .collect::<Vec<_>>()
    );
    v["searched"] = json!({
        "queries": found.queries,
        "indexers": found.indexers,
        "complete": found.complete,
        "problem": found.problem,
        "cached": cached_age.is_some(),
        "age_secs": cached_age.map(|a| a.as_secs()),
    });
    ok(v)
}

/// The answer when Arr cannot take the request now: no copies, and when to ask again.
fn busy_answer(mut v: Value, retry_after_secs: u64, problem: &str) -> Value {
    v["copies"] = json!([]);
    v["skipped"] = json!([]);
    v["searched"] = json!({
        "queries": 0, "indexers": 0, "complete": false, "cached": false, "age_secs": null,
        "retry_after_secs": retry_after_secs,
        "problem": problem,
    });
    v
}

/// What `/feed` shows under "sport": the next fortnight of followed events that are still to
/// be played (postponed ones too, so the feed says so). Empty when sport is off.
pub(super) async fn coming_up(st: &AppState, now: OffsetDateTime) -> Result<Vec<Value>, ApiError> {
    if st.sports.is_none() {
        return Ok(vec![]);
    }
    let list = st
        .store
        .with(move |c| {
            data::followed_events(
                c,
                now,
                now + Duration::from_secs(FEED_DAYS * 86_400),
                None,
                Some(&["scheduled", "postponed"]),
                FEED_LIMIT,
            )
        })
        .await
        .map_err(internal("feed"))?;
    Ok(list.iter().map(sport::event_json).collect())
}

/// What `/calendar` shows under "sport": followed events in the window, whatever came of them
/// except those that were cancelled. Empty when sport is off.
pub(super) async fn in_window(
    st: &AppState,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<Vec<Value>, ApiError> {
    if st.sports.is_none() {
        return Ok(vec![]);
    }
    let list = st
        .store
        .with(move |c| {
            data::followed_events(
                c,
                start,
                end,
                None,
                Some(&["scheduled", "postponed", "played"]),
                EVENTS_LIMIT,
            )
        })
        .await
        .map_err(internal("calendar"))?;
    Ok(list.iter().map(sport::event_json).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_copies_search_gets_twice_the_timeout_between_two_and_twenty_seconds() {
        assert_eq!(
            copies_budget_for(Duration::from_millis(100)),
            Duration::from_secs(2)
        );
        assert_eq!(
            copies_budget_for(Duration::from_secs(4)),
            Duration::from_secs(8)
        );
        assert_eq!(
            copies_budget_for(Duration::from_secs(60)),
            Duration::from_secs(20)
        );
    }

    #[test]
    fn the_budget_is_four_fifths_of_the_timeout_at_most_eight_seconds() {
        let ms = |t: u64| budget_for(Duration::from_millis(t)).as_millis();
        assert_eq!(
            ms(1000),
            800,
            "a one second timeout gives 800 ms, not a second"
        );
        assert_eq!(ms(500), 400);
        assert_eq!(ms(2000), 1600);
        assert_eq!(ms(10_000), 8000);
        assert_eq!(ms(60_000), 8000);
    }
}
