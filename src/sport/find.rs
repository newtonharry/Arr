//! Looking for the releases of one event: the search texts, the search itself over the same
//! indexers the shows use, and the judging of every answer. Read-only: nothing here records a
//! download, a link or a grab.
//!
//! A failing indexer gives a partial answer (`Found::complete` is false and `problem` says
//! so); only a missing search backend or no usable indexer is an error, as everywhere else.

use super::Part;
use super::catalog::{League, Sport};
use super::data::{EventRow, TeamRow};
use super::matcher::{self, Confidence, Context, Match, RefusalCode, Verdict};
use super::names;
use super::release::{self, Series, SportRelease};
use crate::decision::{self, Health};
use crate::error::{ApiError, ErrorCode};
use crate::indexer::Backend;
use crate::prowlarr::ProwlarrRelease;
use crate::search;
use crate::settings::{MediaKind, Settings};
use crate::title;
use std::collections::BTreeMap;
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

/// The most search texts sent for one event.
pub const MAX_QUERIES: usize = 3;

fn time_of(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).ok()
}

/// When Arr reckons the event is over: the source's end, if it gave one (a day later for a
/// tournament, whose end date is at midnight before the last evening), else the start plus a
/// length typical for the sport. MMA's start is the start of the whole event (the first
/// early prelim), so eight hours covers the main card.
pub fn expected_end(ev: &EventRow, sport: Sport) -> Option<OffsetDateTime> {
    let start = time_of(&ev.starts_at)?;
    if let Some(end) = ev.ends_at.as_deref().and_then(time_of) {
        return Some(if sport == Sport::Golf {
            end + Duration::days(1)
        } else {
            end
        });
    }
    Some(
        start
            + match sport {
                Sport::Soccer | Sport::RugbyUnion | Sport::RugbyLeague => Duration::hours(3),
                Sport::Basketball => Duration::minutes(210),
                Sport::Mma => Duration::hours(8),
                Sport::Golf => Duration::days(4),
            },
    )
}

/// Whether copies of the event can exist yet by Arr's reckoning, and if not, why.
pub fn availability(ev: &EventRow, sport: Sport, now: OffsetDateTime) -> Result<(), &'static str> {
    match ev.state.as_str() {
        "cancelled" => return Err("cancelled"),
        "postponed" => return Err("postponed"),
        "unlisted" => return Err("unlisted"),
        _ => {}
    }
    match expected_end(ev, sport) {
        Some(end) if end <= now => Ok(()),
        _ => Err("not_over_yet"),
    }
}

fn ymd(d: time::Date) -> String {
    format!("{:04} {:02} {:02}", d.year(), u8::from(d.month()), d.day())
}

fn dmy(d: time::Date) -> String {
    format!("{:02}.{:02}.{:04}", d.day(), u8::from(d.month()), d.year())
}

/// What to ask the indexers, at most [`MAX_QUERIES`] texts. Plain words: the indexers match
/// words, and the matcher judges what comes back.
pub fn queries_for(ev: &EventRow, league: &League) -> Vec<String> {
    let alias = league.aliases.first().copied().unwrap_or(league.name);
    // As the catalogue writes it ("A-League"): a title has it that way, and an indexer reads
    // the hyphen as a break.
    let alias = alias.to_string();
    let Some(start) = time_of(&ev.starts_at) else {
        return vec![];
    };
    let day = start.date();
    let join = |parts: &[&str]| {
        parts
            .iter()
            .filter(|p| !p.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut out: Vec<String> = vec![];
    match league.sport {
        Sport::Soccer | Sport::Basketball | Sport::RugbyUnion | Sport::RugbyLeague => {
            let core = |n: &Option<String>| {
                n.as_deref()
                    .map(|n| names::strip_generic(&title::tokens(n)).join(" "))
                    .unwrap_or_default()
            };
            let (home, away) = (core(&ev.home_name), core(&ev.away_name));
            out.push(join(&[&alias, &ymd(day)]));
            if !home.is_empty() && !away.is_empty() {
                out.push(join(&[&alias, &home, &away]));
                out.push(join(&[&home, &away, &dmy(day)]));
            }
        }
        Sport::Mma => {
            let e = release::parse(&ev.name, None);
            if let (Series::Numbered, Some(n)) = (e.series, e.number) {
                out.push(join(&[&alias, &n.to_string()]));
            }
            if let Some(s) = &e.sides {
                let side = |v: &[String]| names::fighter_tokens(v).join(" ");
                out.push(join(&[&alias, &side(&s.left), &side(&s.right)]));
                let last =
                    |v: &[String]| names::fighter_tokens(v).last().cloned().unwrap_or_default();
                out.push(join(&[&alias, &last(&s.left)]));
            }
            // (A card the source gave no title is named "<League> event on <date>": no place.)
            let neutral = ev.name.contains(" event on ");
            if !e.head.is_empty() && !neutral {
                out.push(join(&[&alias, &e.head.join(" ")]));
            }
            if out.is_empty() {
                out.push(join(&[&alias, &ymd(day)]));
            }
        }
        Sport::Golf => {
            let t = title::tokens(&ev.name);
            let cut = t
                .iter()
                .position(|w| matches!(w.as_str(), "pres" | "presented" | "by"))
                .unwrap_or(t.len());
            let name = t[..cut].join(" ");
            out.push(join(&[&alias, &name]));
            out.push(name);
        }
    }
    let mut seen: Vec<String> = vec![];
    out.retain(|q| {
        !q.is_empty() && !seen.contains(q) && {
            seen.push(q.clone());
            true
        }
    });
    out.truncate(MAX_QUERIES);
    out
}

/// What the store knows around the event.
#[derive(Debug, Default, Clone)]
pub struct Surroundings {
    pub teams: Vec<TeamRow>,
    /// Events of the same league within a few days of this one.
    pub neighbours: Vec<EventRow>,
    /// Earlier events of the same league.
    pub earlier: Vec<EventRow>,
    /// `teams` is the league's whole list: its last read is recorded as whole and is under
    /// [`TEAMS_FRESH_DAYS`] days old. Otherwise a team is named only by an exact name (see
    /// `names::resolve_side`).
    pub teams_complete: bool,
    /// The stretches the league's schedule is recorded as read in full and current
    /// (`data::complete_spans`). "No other fixture fits" is only known inside one.
    pub spans: Vec<(OffsetDateTime, OffsetDateTime)>,
    /// `neighbours` is every stored event of its window, not cut at the limit it was loaded
    /// with.
    pub neighbours_whole: bool,
}

impl Surroundings {
    /// For tests: everything known, the schedule in full for all time.
    #[cfg(test)]
    pub fn all_known() -> Self {
        Surroundings {
            teams_complete: true,
            spans: vec![(
                OffsetDateTime::UNIX_EPOCH,
                OffsetDateTime::UNIX_EPOCH + Duration::days(365 * 200),
            )],
            neighbours_whole: true,
            ..Default::default()
        }
    }
}

/// A team list older than this is not taken to be the league's list even if its read was whole:
/// a promoted club may be missing from it. (The worker reads each list weekly, so only a
/// failing source gets here.)
pub const TEAMS_FRESH_DAYS: i64 = 21;

/// Whether an earlier event had the same two fighters as `ev`.
pub fn earlier_same_pair(ev: &EventRow, earlier: &[EventRow]) -> bool {
    let here = release::parse(&ev.name, None);
    let Some(h) = &here.sides else { return false };
    let start = time_of(&ev.starts_at);
    earlier.iter().filter(|o| o.id != ev.id).any(|o| {
        let before = matches!((time_of(&o.starts_at), start), (Some(a), Some(b)) if a < b);
        let theirs = release::parse(&o.name, None);
        before
            && theirs.sides.is_some_and(|s| {
                (names::same_fighter(&s.left, &h.left) && names::same_fighter(&s.right, &h.right))
                    || (names::same_fighter(&s.left, &h.right)
                        && names::same_fighter(&s.right, &h.left))
            })
    })
}

/// One release that is the event (or a part of it).
#[derive(Debug, Clone)]
pub struct Candidate {
    pub release: ProwlarrRelease,
    pub release_ref: String,
    pub parsed: SportRelease,
    pub m: Match,
    pub health: Health,
}

#[derive(Debug, Default, Clone)]
pub struct Found {
    /// Best first (by part, then how much the title proved, then health). Nothing here is a
    /// recommendation: a person picks.
    pub candidates: Vec<Candidate>,
    /// Releases the search returned that are not this event, by why.
    pub skipped: BTreeMap<RefusalCode, usize>,
    pub queries: usize,
    pub indexers: usize,
    /// Every query to every indexer answered in full, and every result was judged.
    pub complete: bool,
    pub problem: Option<String>,
    /// Results not judged because there were more than [`MAX_JUDGED`], or the deadline came.
    pub unjudged: usize,
}

impl Found {
    /// The copies that answer a request for one part: those of the part and the whole-event
    /// files that say so (a file whose part the title does not establish could be anything, so
    /// it answers no part). The rest are counted as `another_part`.
    pub fn for_part(&self, want: Option<Part>) -> Found {
        let Some(w) = want else {
            return self.clone();
        };
        let mut out = self.clone();
        out.candidates.clear();
        for c in &self.candidates {
            // Only a part the title establishes answers a request for a part: "UFC 333 1080p"
            // names none, whatever the matcher guessed it to be.
            let answers = c.m.part_established && (c.m.part == w || c.m.part == Part::Whole);
            if answers {
                out.candidates.push(c.clone());
            } else {
                *out.skipped.entry(RefusalCode::AnotherPart).or_default() += 1;
            }
        }
        out
    }
}

fn resolution_rank(q: crate::quality::Quality) -> u8 {
    match q.resolution() {
        Some(1080) => 0,
        Some(720) => 1,
        Some(2160) => 2,
        Some(_) => 3,
        None => 4,
    }
}

fn language_rank(r: &SportRelease) -> u8 {
    match r.language.as_deref() {
        None | Some("English") => 0,
        Some(_) => 1,
    }
}

/// The most results of one search that are read. A search is a few queries of a few pages; a
/// feed that sends more than this is not an answer about one event.
pub const MAX_JUDGED: usize = 300;

pub struct Judged {
    pub candidates: Vec<Candidate>,
    pub skipped: BTreeMap<RefusalCode, usize>,
    /// Results left unread: past [`MAX_JUDGED`], or past the deadline.
    pub unjudged: usize,
}

/// Judges the releases a search returned against the event. Pure over its inputs, so tests run
/// it on lists of titles. The work is bounded: at most [`MAX_JUDGED`] results, each parsed
/// within the bounds of `release::parse`, with the event's side of every comparison worked out
/// once; and it stops at the `deadline`, so it cannot run on after the request's budget.
pub fn judge_all(
    found: Vec<ProwlarrRelease>,
    ev: &EventRow,
    league: &'static League,
    around: &Surroundings,
    deadline: Option<std::time::Instant>,
) -> Judged {
    let cx = Context::new(
        league,
        ev,
        &around.teams,
        &around.neighbours,
        earlier_same_pair(ev, &around.earlier),
        around.teams_complete,
        around.spans.clone(),
        around.neighbours_whole,
    );
    let total = found.len();
    let mut unjudged = 0;
    let mut skipped: BTreeMap<RefusalCode, usize> = BTreeMap::new();
    let mut matched: Vec<(ProwlarrRelease, SportRelease, Match)> = vec![];
    for (i, r) in found.into_iter().enumerate() {
        if i >= MAX_JUDGED || deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            unjudged = total - i;
            break;
        }
        let published = r
            .publish_date
            .as_deref()
            .and_then(|d| OffsetDateTime::parse(d, &Rfc3339).ok());
        let parsed = release::parse(r.title.as_deref().unwrap_or(""), published);
        match matcher::judge(&parsed, ev, &cx) {
            Verdict::Match(m) => matched.push((r, parsed, m)),
            Verdict::Refused(x) => *skipped.entry(x.code).or_default() += 1,
        }
    }
    // Unlabelled files are told from their siblings' labels (the part only: see `settle_unlabelled`).
    let mut items: Vec<(Option<String>, Match)> = matched
        .iter()
        .map(|(_, p, m)| (p.group.clone(), m.clone()))
        .collect();
    matcher::settle_unlabelled(&mut items);
    for ((_, _, m), (_, settled)) in matched.iter_mut().zip(items) {
        *m = settled;
    }
    let mut out: Vec<Candidate> = matched
        .into_iter()
        .map(|(r, parsed, m)| Candidate {
            release_ref: search::release_ref(&r),
            health: decision::health(r.seeders, r.leechers),
            release: r,
            parsed,
            m,
        })
        .collect();
    out.sort_by(|a, b| {
        let key = |c: &Candidate| {
            (
                c.m.part,
                c.m.confidence == Confidence::Likely,
                std::cmp::Reverse(c.health),
                language_rank(&c.parsed),
                resolution_rank(c.parsed.quality),
                std::cmp::Reverse(c.release.seeders.unwrap_or(0)),
                std::cmp::Reverse(c.release.size.unwrap_or(0)),
                c.release_ref.clone(),
            )
        };
        key(a).cmp(&key(b))
    });
    Judged {
        candidates: out,
        skipped,
        unjudged,
    }
}

/// What a search for an event brought back, before any of it is judged: the releases (as many
/// as are read), and how the search went. This is what is kept between requests; every request
/// judges it again against the event, its neighbours and the team data as they are then.
#[derive(Debug, Default, Clone)]
pub struct Searched {
    pub releases: Vec<ProwlarrRelease>,
    pub queries: usize,
    pub indexers: usize,
    /// Every query to every indexer answered in full.
    pub complete: bool,
    pub problem: Option<String>,
}

/// Searches the shows' indexers for the event, giving the search up at `budget`. Nothing is
/// judged here.
pub async fn search_event(
    backend: Option<&Backend>,
    settings: &Settings,
    ev: &EventRow,
    league: &'static League,
    refresh: bool,
    budget: std::time::Duration,
) -> Result<Searched, ApiError> {
    let texts = queries_for(ev, league);
    if texts.is_empty() {
        return Ok(Searched {
            complete: true,
            ..Searched::default()
        });
    }
    let fetched = match search::fetch(
        backend,
        settings,
        MediaKind::Series,
        false,
        refresh,
        &texts,
        Some(budget),
    )
    .await
    {
        Ok(f) => f,
        Err(e)
            if matches!(
                e.code,
                ErrorCode::ProviderNotConfigured | ErrorCode::NoUsableIndexers
            ) =>
        {
            return Err(e);
        }
        // Every query failed: say so rather than fail the request.
        Err(e) => {
            return Ok(Searched {
                queries: texts.len(),
                complete: false,
                problem: Some(e.message),
                ..Searched::default()
            });
        }
    };
    let complete = fetched.queries.iter().all(|q| q.status == "complete");
    let problem = fetched
        .queries
        .iter()
        .find(|q| q.status != "complete")
        .and_then(|q| {
            q.error
                .as_ref()
                .and_then(|e| e["message"].as_str().map(str::to_string))
                .or_else(|| Some(format!("an indexer answered only in part ({})", q.status)))
        });
    let mut indexers: Vec<u32> = fetched
        .queries
        .iter()
        .flat_map(|q| q.indexer_ids.iter().copied())
        .collect();
    indexers.sort_unstable();
    indexers.dedup();
    let mut releases: Vec<ProwlarrRelease> = fetched.found.into_iter().map(|(r, _)| r).collect();
    // Only what can be read is kept: the rest of a very long list is not an answer about one
    // event, and is not stored.
    let over = releases.len() > MAX_JUDGED;
    releases.truncate(MAX_JUDGED);
    Ok(Searched {
        releases,
        queries: texts.len(),
        indexers: indexers.len(),
        complete: complete && !over,
        problem: problem
            .or_else(|| over.then(|| "there were more results than Arr could look at".into())),
    })
}

/// How long judging a search's results may take, whatever there is to judge.
pub const JUDGE_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Judges a search against the event as it is now: its neighbours, the league's teams and what
/// is recorded as known. The answer is never kept: a change to any of those changes it.
pub fn judge_search(
    searched: &Searched,
    ev: &EventRow,
    league: &'static League,
    around: &Surroundings,
) -> Found {
    judge_search_until(
        searched,
        ev,
        league,
        around,
        std::time::Instant::now() + JUDGE_BUDGET,
    )
}

/// How many requests may be judging at once: judging is CPU work, and a request that has to
/// wait for a slot waits inside its own budget.
pub const JUDGE_SLOTS: usize = 4;

/// [`judge_search`] off the async workers, behind `slots`: a few requests judge at a time, the
/// others wait for a slot, and the whole of it (the wait, the setup, the judging and the
/// settling) is inside one [`JUDGE_BUDGET`]. `None` when no slot came free in time, which the
/// caller answers as busy.
pub async fn judge_search_bounded(
    slots: std::sync::Arc<tokio::sync::Semaphore>,
    searched: std::sync::Arc<Searched>,
    ev: EventRow,
    league: &'static League,
    around: Surroundings,
) -> Option<Found> {
    let deadline = std::time::Instant::now() + JUDGE_BUDGET;
    let permit = tokio::time::timeout(JUDGE_BUDGET, slots.acquire_owned())
        .await
        .ok()?
        .ok()?;
    tokio::task::spawn_blocking(move || {
        let _slot = permit;
        judge_search_until(&searched, &ev, league, &around, deadline)
    })
    .await
    .ok()
}

fn judge_search_until(
    searched: &Searched,
    ev: &EventRow,
    league: &'static League,
    around: &Surroundings,
    deadline: std::time::Instant,
) -> Found {
    let judged = judge_all(
        searched.releases.clone(),
        ev,
        league,
        around,
        Some(deadline),
    );
    let mut complete = searched.complete;
    let mut problem = searched.problem.clone();
    if judged.unjudged > 0 {
        complete = false;
        problem.get_or_insert_with(|| "there were more results than Arr could look at".into());
    }
    Found {
        candidates: judged.candidates,
        skipped: judged.skipped,
        queries: searched.queries,
        indexers: searched.indexers,
        complete,
        problem,
        unjudged: judged.unjudged,
    }
}
