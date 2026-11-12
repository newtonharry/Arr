//! Sport in the API: search the built-in leagues and the teams Arr has seen, follow or
//! unfollow a league or a team, and list the events of what is followed. Everything answers
//! from the store; the schedule source is only asked when a league is opened or followed and
//! its stored schedule is old, and then for a few seconds at most (the read carries on in the
//! background). A source that is down never turns an answer into an error: the stored
//! schedule is served and `schedule.problem` says what is wrong.

use super::*;
use crate::sport::{self, catalog, data, refresh};

/// How long a request waits for the schedule source before answering from what is stored.
const BUDGET: Duration = Duration::from_secs(8);
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
    let fresh =
        refresh::ensure_fresh_within(st.store.clone(), sports, league, refresh::STALE, BUDGET)
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
                let read =
                    refresh::ensure_teams_within(st.store.clone(), sports.clone(), league, BUDGET)
                        .await;
                found = find(st.clone(), id.to_string())
                    .await
                    .map_err(internal("sport follow"))?;
                if found.is_none() {
                    // Only a list that was really read can say a team is not in the league;
                    // without one (never read, or the source failing and being waited out)
                    // the answer is "try again", not "no such team".
                    let listed = st
                        .store
                        .with(move |c| Ok(data::refresh_state(c, slug)?.teams.is_some()))
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
    let fresh =
        refresh::ensure_fresh_within(st.store.clone(), sports, league, RECENT, BUDGET).await;
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
