//! Whether a release is a given event. Pure: a parsed release (`release::parse`), a stored
//! event and what is known around it go in; a match with its evidence, or the reason for
//! refusing, comes out.
//!
//! Strictness is the point. A wrong file under an event is the failure to avoid, so a release
//! matches only on evidence the title itself carries: the competition, and for a game the date
//! (within one day) and both teams; for a fight card the card's number or both headline
//! surnames, with a date or publish time near the event; for a golf tournament its name and a
//! day inside it. Words no rule accounts for refuse a release as much as a listed one does.

pub mod claims;

use super::Part;
use super::catalog::{League, Sport};
use super::data::{EventRow, TeamRow};
use super::names::{self, TeamKey};
use super::release::{self, PartHint, SportRelease};
use crate::import::rfc3339;
use claims::{Claim, Input, Shape};
use time::{Date, Duration, OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// Shown as a match.
    Sure,
    /// Shown with "check this is the right one"; never picked by Arr on its own, except the
    /// one case the caller names (`Match::unlabelled`).
    Likely,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub part: Part,
    pub confidence: Confidence,
    /// Why it is only `Likely`: `date_only`, `place_only`, `rematch_unverified`,
    /// `unlabelled_main` or `unlabelled_full`.
    pub reason: Option<&'static str>,
    /// What the title proved: `league`, `date`, `teams`, `number`, `fighters`, `place`,
    /// `tournament`, `round`.
    pub evidence: Vec<&'static str>,
    /// A UFC card file with no part word. `settle_unlabelled` decides what it holds.
    pub unlabelled: bool,
    /// Whether the title says which part this is (a part word, or a game that has no parts).
    /// A whole-event match without it answers no request for a part.
    pub part_established: bool,
    /// The title has words that no closed list or event checks (see `release::Role::Free`), or
    /// a given name before a fighter's. Such a match is at most `Likely`, and a sibling cannot
    /// settle it.
    pub unverified: bool,
}

/// Builds the verdict of a match. `Sure` is earned: it needs no reason to doubt and no
/// unverified word. Whatever is unverified caps it at `Likely`, with the first reason there is
/// (`unverified_text` if that is all).
#[allow(clippy::too_many_arguments)]
fn matched(
    r: &SportRelease,
    part: Part,
    reason: Option<&'static str>,
    evidence: Vec<&'static str>,
    unlabelled: bool,
    part_established: bool,
    unverified: bool,
) -> Verdict {
    let unverified = unverified || !r.unverified().is_empty();
    // Whatever else is unverified, a title with no uploader tag has no one to answer for it: a
    // forum-style title has none by nature, and is capped for it.
    let reason = reason
        .or(unverified.then_some("unverified_text"))
        .or(r.group.is_none().then_some("no_uploader_tag"));
    Verdict::Match(Match {
        part,
        confidence: if reason.is_some() {
            Confidence::Likely
        } else {
            Confidence::Sure
        },
        reason,
        evidence,
        unlabelled,
        part_established,
        unverified,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RefusalCode {
    NotAnEvent,
    NoLeague,
    OtherLeague,
    AmbiguousLeague,
    QualifiedLeague,
    OtherCompetition,
    NotTheEvent,
    DateUnreadable,
    PublishedBeforeEvent,
    NoTeams,
    NoDate,
    WrongDate,
    TeamMismatch,
    AmbiguousTeam,
    RoundMismatch,
    WrongSeason,
    SeriesMismatch,
    NumberMismatch,
    FightersMismatch,
    NoEvidence,
    OutsideWindow,
    TournamentMismatch,
    AnotherPart,
    TitleTooLong,
    SingleBout,
}

impl RefusalCode {
    pub fn as_str(self) -> &'static str {
        match self {
            RefusalCode::NotAnEvent => "not_an_event",
            RefusalCode::NoLeague => "no_league",
            RefusalCode::OtherLeague => "other_league",
            RefusalCode::AmbiguousLeague => "ambiguous_league",
            RefusalCode::QualifiedLeague => "qualified_league",
            RefusalCode::OtherCompetition => "other_competition",
            RefusalCode::NotTheEvent => "not_the_event",
            RefusalCode::DateUnreadable => "date_unreadable",
            RefusalCode::PublishedBeforeEvent => "published_before_event",
            RefusalCode::NoTeams => "no_teams",
            RefusalCode::NoDate => "no_date",
            RefusalCode::WrongDate => "wrong_date",
            RefusalCode::TeamMismatch => "team_mismatch",
            RefusalCode::AmbiguousTeam => "ambiguous_team",
            RefusalCode::RoundMismatch => "round_mismatch",
            RefusalCode::WrongSeason => "wrong_season",
            RefusalCode::SeriesMismatch => "series_mismatch",
            RefusalCode::NumberMismatch => "number_mismatch",
            RefusalCode::FightersMismatch => "fighters_mismatch",
            RefusalCode::NoEvidence => "no_evidence",
            RefusalCode::OutsideWindow => "outside_window",
            RefusalCode::TournamentMismatch => "tournament_mismatch",
            RefusalCode::AnotherPart => "another_part",
            RefusalCode::TitleTooLong => "title_too_long",
            RefusalCode::SingleBout => "single_bout",
        }
    }

    /// Why, in words for a person. Several codes share one sentence so the list stays short.
    pub fn plain(self) -> &'static str {
        match self {
            RefusalCode::NotAnEvent | RefusalCode::TitleTooLong => {
                "not sport at all (a video game, an episode)"
            }
            RefusalCode::SingleBout => "one fight from the card, not the card",
            RefusalCode::NoLeague
            | RefusalCode::OtherLeague
            | RefusalCode::AmbiguousLeague
            | RefusalCode::QualifiedLeague
            | RefusalCode::OtherCompetition => "from another competition",
            RefusalCode::NotTheEvent => "not the event itself (a show about it)",
            RefusalCode::DateUnreadable
            | RefusalCode::PublishedBeforeEvent
            | RefusalCode::NoDate
            | RefusalCode::WrongDate
            | RefusalCode::OutsideWindow => "from a different day",
            RefusalCode::NoTeams
            | RefusalCode::TeamMismatch
            | RefusalCode::AmbiguousTeam
            | RefusalCode::RoundMismatch
            | RefusalCode::WrongSeason
            | RefusalCode::SeriesMismatch
            | RefusalCode::NumberMismatch
            | RefusalCode::FightersMismatch
            | RefusalCode::NoEvidence
            | RefusalCode::TournamentMismatch => "not this one",
            RefusalCode::AnotherPart => "another part of the event",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: RefusalCode,
    /// For logs and tests; never shown, since it can quote the title.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Match(Match),
    Refused(Refusal),
}

impl Verdict {
    pub fn matched(&self) -> Option<&Match> {
        match self {
            Verdict::Match(m) => Some(m),
            Verdict::Refused(_) => None,
        }
    }

    pub fn refusal(&self) -> Option<RefusalCode> {
        match self {
            Verdict::Refused(r) => Some(r.code),
            Verdict::Match(_) => None,
        }
    }
}

/// What the judge may look at besides the release and the event. Built once for an event
/// (`Context::new`) and used for every release of a search, so what is worked out from the store
/// is not worked out again per release.
pub struct Context<'a> {
    pub league: &'static League,
    /// The league's known teams (empty when the list was never read).
    pub teams: &'a [TeamRow],
    /// The teams a release's sides are resolved against (`names::team_keys`).
    pub keys: Vec<TeamKey>,
    /// Whether `teams` is the league's whole list: the last read of it was recorded complete
    /// (nothing dropped, renamed or cut) and is recent. If not, a side is resolved only by an
    /// exact name of a known team: a partial name ("Manchester") cannot be told from another
    /// club's when the other clubs are not known.
    pub teams_complete: bool,
    /// Other events of the league near this one (a window that depends on the sport).
    pub neighbours: &'a [EventRow],
    /// `neighbours`' names read as titles (parallel to `neighbours`).
    pub neighbour_names: Vec<SportRelease>,
    /// The stretches of time the league's schedule is recorded as read in full, and current.
    /// "No other fixture fits" is only known inside one.
    pub spans: Vec<(OffsetDateTime, OffsetDateTime)>,
    /// `neighbours` is every stored event of the window asked for, not cut at a limit.
    pub neighbours_whole: bool,
    /// Whether an earlier stored event had the same two fighters (so a "2" is a rematch the
    /// schedule already knows).
    pub earlier_same_pair: bool,
    /// The event's own name read as a title.
    pub event_name: SportRelease,
}

impl<'a> Context<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        league: &'static League,
        ev: &EventRow,
        teams: &'a [TeamRow],
        neighbours: &'a [EventRow],
        earlier_same_pair: bool,
        teams_complete: bool,
        spans: Vec<(OffsetDateTime, OffsetDateTime)>,
        neighbours_whole: bool,
    ) -> Self {
        Context {
            league,
            teams,
            keys: names::team_keys(teams, ev),
            teams_complete,
            neighbours,
            neighbour_names: neighbours
                .iter()
                .map(|n| release::parse(&n.name, None))
                .collect(),
            spans,
            neighbours_whole,
            earlier_same_pair,
            event_name: release::parse(&ev.name, None),
        }
    }

    /// Whether the schedule is known in full for `[from, to]`: the neighbours were not cut,
    /// and one recorded stretch covers it.
    pub fn covers(&self, from: OffsetDateTime, to: OffsetDateTime) -> bool {
        self.neighbours_whole && self.spans.iter().any(|(a, b)| *a <= from && to <= *b)
    }
}

/// How long after the event's start a release may appear and still be taken for it.
pub const LATE_DAYS: i64 = 14;

fn refuse(code: RefusalCode, detail: impl Into<String>) -> Verdict {
    Verdict::Refused(Refusal {
        code,
        detail: detail.into(),
    })
}

fn time_of(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).ok()
}

fn days_apart(a: Date, b: Date) -> i64 {
    (a - b).whole_days().abs()
}

/// What the publish time says, whatever the title says. A release is put up from a day before
/// the event to `latest`; a later one is a re-upload (`late_upload`), and no publish time at
/// all is `undated`: either way nothing outside the title vouches for it, so it is at most
/// `Likely`. `settled` is whether the title itself settles which event it is (a date, or a
/// card's number): without that, a late or missing publish time refuses the title.
fn publication(
    r: &SportRelease,
    latest: OffsetDateTime,
    settled: bool,
) -> Result<Option<&'static str>, Verdict> {
    match r.published {
        Some(p) if p <= latest => Ok(None),
        Some(_) if settled => Ok(Some("late_upload")),
        Some(_) => Err(refuse(RefusalCode::OutsideWindow, "published long after")),
        None if settled => Ok(Some("undated")),
        None => Err(refuse(RefusalCode::NoEvidence, "no date at all")),
    }
}

/// How far the farthest of the title's dates is from `day`, if it is more than `slack` days:
/// every date a title carries must be near the event (a second date that is not is a repost,
/// a different game, or noise, and nothing says which).
fn too_far(dates: &[Date], day: Date, slack: i64) -> Option<i64> {
    dates
        .iter()
        .map(|d| days_apart(*d, day))
        .max()
        .filter(|n| *n > slack)
}

/// A title date within `slack` days of `day`.
fn near(dates: &[Date], day: Date, slack: i64) -> Option<i64> {
    dates
        .iter()
        .map(|d| days_apart(*d, day))
        .min()
        .filter(|n| *n <= slack)
}

/// Whether two fighter pairs are the same two people. `Ok(strict)`: `strict` is false when a
/// given name stands before a surname in the release (one word, not the card's place), which
/// nothing checks.
fn same_pair_runs(
    r: &release::Sides,
    e: &release::Sides,
    head: &[String],
) -> Result<bool, &'static str> {
    // One side of the card: the release's run may carry a place before the name, or a given
    // name; the event's run is the surname(s). `Some(strict)` if it fits.
    let fits = |rr: &[String], ee: &[String], lead_ok: bool| -> Option<bool> {
        // Explicit suffixes are compared before they are dropped: "Jr" is not "Sr", and a
        // suffix only one side has is something nothing checks.
        let (sa, sb) = (names::suffixes_of(rr), names::suffixes_of(ee));
        let suffix = match (sa.is_empty(), sb.is_empty()) {
            (false, false) if sa != sb => return None,
            (false, true) | (true, false) => false,
            _ => true,
        };
        if !names::same_fighter(rr, ee) {
            return None;
        }
        let (fr, fe) = (names::fighter_tokens(rr), names::fighter_tokens(ee));
        if fr.len() <= fe.len() {
            return Some(suffix);
        }
        let lead = &fr[..fr.len() - fe.len()];
        if lead_ok && lead.iter().all(|w| head.contains(w)) {
            return Some(suffix);
        }
        (lead.len() <= 1).then_some(false)
    };
    let both = |a: Option<bool>, b: Option<bool>| a.zip(b).map(|(x, y)| x && y);
    let aligned = both(
        fits(&r.left, &e.left, true),
        fits(&r.right, &e.right, false),
    );
    let swapped = both(
        fits(&r.left, &e.right, true),
        fits(&r.right, &e.left, false),
    );
    match (aligned, swapped) {
        (Some(x), Some(y)) => Ok(x || y),
        (Some(x), None) | (None, Some(x)) => Ok(x),
        (None, None) => Err("fighters"),
    }
}

/// Judges one release against one event.
///
/// Where the title's names are split is not decided by position (a `v` can be a letter of
/// "P.S.V."): every place a side separator could stand (`SportRelease::splits`, at most
/// `release::MAX_SPLITS`) is tried, the default reading and each forced one, each judged in full
/// against the stored event with the initials of both sides folded the same way. A reading that
/// matches is kept over those that do not. If several readings match and differ, the best
/// confidence wins, and a tie is unverified (`Likely`, `unverified_text`), never a guess. If none
/// matches, the default reading's refusal stands, as before.
pub fn judge(r: &SportRelease, ev: &EventRow, cx: &Context) -> Verdict {
    let first = judge_one(r, ev, cx);
    if r.splits < 2 || r.split.is_some() || r.raw.is_empty() {
        return first;
    }
    let mut matches: Vec<Match> = first.matched().cloned().into_iter().collect();
    for i in 0..r.splits {
        let alt = release::parse_split(&r.raw, r.published, i);
        if let Verdict::Match(m) = judge_one(&alt, ev, cx)
            && !matches.contains(&m)
        {
            matches.push(m);
        }
    }
    match matches.len() {
        0 => first,
        1 => Verdict::Match(matches.remove(0)),
        _ => {
            let best = matches
                .iter()
                .map(|m| m.confidence)
                .max_by_key(|c| matches!(c, Confidence::Sure))
                .unwrap_or(Confidence::Likely);
            let top: Vec<&Match> = matches.iter().filter(|m| m.confidence == best).collect();
            let mut m = top[0].clone();
            if top.iter().any(|x| **x != m) {
                m.confidence = Confidence::Likely;
                m.reason = Some("unverified_text");
                m.unverified = true;
            }
            Verdict::Match(m)
        }
    }
}

fn judge_one(r: &SportRelease, ev: &EventRow, cx: &Context) -> Verdict {
    if r.oversized {
        return refuse(RefusalCode::TitleTooLong, "longer than any real title");
    }
    if !r.non_event.is_empty() {
        return refuse(RefusalCode::NotAnEvent, r.non_event.join(", "));
    }
    if r.leagues.is_empty() {
        return refuse(RefusalCode::NoLeague, "no competition named");
    }
    // A competition's name found inside the event's own name ("PGA Championship" in "KPMG
    // Women's PGA Championship") is the tournament, not a second competition, when the title
    // also names the event's own tour.
    let event_words = names::tokens(&ev.name);
    let leagues: Vec<&'static League> = if r.leagues.len() > 1 {
        // (only an occurrence inside the event's own name is exempt: another explicit claim of
        // the same league still counts)
        let occurrences = |slug: &str| r.league_hits.iter().filter(|h| h.slug == slug).count();
        // An occurrence is inside the tournament's name when the event's name continues around
        // it in the title (more of the name stands beside it); a lone occurrence that is in the
        // name at all is the tournament too ("LPGA Tour PGA Championship ...").
        let inside = |h: &release::LeagueHit, alone: bool| {
            let n = h.words.len();
            if n == 0 {
                return false;
            }
            event_words
                .windows(n)
                .enumerate()
                .filter(|(_, w)| *w == h.words.as_slice())
                .any(|(j, _)| {
                    let left = h
                        .before
                        .iter()
                        .rev()
                        .zip(event_words[..j].iter().rev())
                        .take_while(|(a, b)| a == b)
                        .count();
                    let right = h
                        .after
                        .iter()
                        .zip(event_words[j + n..].iter())
                        .take_while(|(a, b)| a == b)
                        .count();
                    alone || left + right > 0
                })
        };
        r.leagues
            .iter()
            .copied()
            .filter(|l| {
                l.slug == cx.league.slug
                    || r.league_hits
                        .iter()
                        .any(|h| h.slug == l.slug && !inside(h, occurrences(l.slug) == 1))
            })
            .collect()
    } else {
        r.leagues.clone()
    };
    if leagues.len() > 1 {
        return refuse(RefusalCode::AmbiguousLeague, "several competitions named");
    }
    if leagues[0].slug != cx.league.slug {
        return refuse(
            RefusalCode::OtherLeague,
            format!("names {}", leagues[0].slug),
        );
    }
    if r.men && cx.league.women() {
        return refuse(RefusalCode::OtherCompetition, "men's competition");
    }
    if r.women && !cx.league.women() {
        return refuse(RefusalCode::OtherCompetition, "women's competition");
    }
    // A qualifier word that is part of a participant's own stored name ("Junior dos Santos")
    // is the name; any other makes it a youth, reserve or academy competition.
    if !r.youth_words.is_empty() {
        let mut stored: Vec<String> = names::tokens(&ev.name);
        for n in [&ev.home_name, &ev.away_name].into_iter().flatten() {
            stored.extend(names::tokens(n));
        }
        // ...and so is one inside a fighter's run in the title whose rest is a stored fighter
        // ("Junior dos Santos" for the stored "dos Santos").
        let in_a_fighters_name = |w: &String| {
            let stored_sides = cx.event_name.sides.as_ref();
            r.sides.as_ref().is_some_and(|rs| {
                [&rs.left, &rs.right].into_iter().any(|run| {
                    run.contains(w)
                        && stored_sides.is_some_and(|es| {
                            [&es.left, &es.right]
                                .into_iter()
                                .any(|s| names::same_fighter(run, s))
                        })
                })
            })
        };
        if let Some(w) = r
            .youth_words
            .iter()
            .find(|w| !stored.contains(w) && !in_a_fighters_name(w))
        {
            return refuse(
                RefusalCode::OtherCompetition,
                format!("{w}: a youth or reserve side"),
            );
        }
    }
    // A division number after the competition's name is another tier, unless it is the
    // leading number of a participant's stored name ("1. FC Union Berlin").
    if let Some(d) = r.tier_digit {
        let leads = [&ev.home_name, &ev.away_name]
            .into_iter()
            .flatten()
            .any(|n| {
                names::tokens(n)
                    .first()
                    .is_some_and(|t| *t == d.to_string())
            });
        if !leads {
            return refuse(
                RefusalCode::QualifiedLeague,
                "a division number after the name",
            );
        }
    }
    if r.qualified {
        return refuse(RefusalCode::QualifiedLeague, "words before the competition");
    }
    if r.part == PartHint::Bout {
        return refuse(
            RefusalCode::SingleBout,
            "a main event, a co-main or one fight, not the card",
        );
    }
    if !r.shoulder.is_empty() {
        return refuse(RefusalCode::NotTheEvent, r.shoulder.join(", "));
    }
    if r.part_conflict {
        return refuse(
            RefusalCode::NotTheEvent,
            "the title and its tag name different parts",
        );
    }
    // The rule is total: every word is accounted for, or the title is refused.
    let unexplained = r.unexplained();
    if !unexplained.is_empty() {
        return refuse(
            RefusalCode::NotTheEvent,
            format!("words no rule accounts for: {}", unexplained.join(" ")),
        );
    }
    if r.date_unreadable {
        return refuse(RefusalCode::DateUnreadable, "a date that is not a date");
    }
    let Some(start) = time_of(&ev.starts_at) else {
        return refuse(RefusalCode::NoEvidence, "the event has no usable start");
    };
    if let Some(p) = r.published
        && p < start - Duration::hours(24)
    {
        return refuse(
            RefusalCode::PublishedBeforeEvent,
            format!("published {}", rfc3339(p)),
        );
    }
    let verdict = match cx.league.sport {
        Sport::Mma => judge_card(r, ev, cx, start),
        Sport::Golf => judge_golf(r, ev, cx, start),
        Sport::Soccer | Sport::Basketball | Sport::RugbyUnion | Sport::RugbyLeague => {
            judge_game(r, ev, cx, start)
        }
    };
    cap_feeds(verdict, r, ev, cx)
}

/// A team's feed code after a channel name ("DAZN-SAS") is verified only by being the code of
/// one of the event's own two teams; any other code leaves the match at most `Likely`.
fn cap_feeds(v: Verdict, r: &SportRelease, ev: &EventRow, cx: &Context) -> Verdict {
    let codes = r.feed_codes();
    let Verdict::Match(mut m) = v else { return v };
    if codes.is_empty() {
        return Verdict::Match(m);
    }
    let own: Vec<String> = [&ev.home_id, &ev.away_id]
        .into_iter()
        .flatten()
        .filter_map(|id| cx.teams.iter().find(|t| &t.id == id))
        .filter_map(|t| t.abbreviation.as_deref())
        .map(crate::title::fold_chars)
        .collect();
    if codes.iter().any(|c| !own.iter().any(|o| o == c)) {
        m.unverified = true;
        m.reason = m.reason.or(Some("unverified_text"));
        m.confidence = Confidence::Likely;
    }
    Verdict::Match(m)
}

fn season_years(label: &str) -> Option<(u16, u16)> {
    let first: u16 = label.get(..4)?.parse().ok()?;
    let last = match label.len() {
        4 => first,
        7 => {
            let yy: u16 = label.get(5..)?.parse().ok()?;
            first - first % 100 + yy + if yy < first % 100 { 100 } else { 0 }
        }
        9 => label.get(5..)?.parse().ok()?,
        _ => return None,
    };
    Some((first, last))
}

/// The window of time in which another fixture could fit a title: a day or two either side of
/// each date it carries, or, with no date, the days a release published then could belong to
/// (an event started within `LATE_DAYS` before it, at most a day after).
fn rival_window(r: &SportRelease, slack_before: i64) -> Option<(OffsetDateTime, OffsetDateTime)> {
    let day = |d: Date| d.midnight().assume_utc();
    if let (Some(lo), Some(hi)) = (r.dates.iter().min(), r.dates.iter().max()) {
        return Some((
            day(*lo) - Duration::days(2 + slack_before),
            day(*hi) + Duration::days(3),
        ));
    }
    r.published.map(|p| {
        (
            p - Duration::days(LATE_DAYS + 1 + slack_before),
            p + Duration::days(1),
        )
    })
}

fn judge_game(r: &SportRelease, ev: &EventRow, cx: &Context, start: OffsetDateTime) -> Verdict {
    if r.sides.is_none() {
        return refuse(RefusalCode::NoTeams, "no two sides");
    }
    let summary = match claims::settle(Shape::Game, &Input { r, ev, cx, start }) {
        Ok(s) => s,
        Err((code, detail)) => return refuse(code, detail),
    };
    // Another fixture of the same two teams that the title would fit as well (the next night
    // of a series, the return leg a day later) makes it a guess, however well it fits this one.
    let rivals = match claims::game_pair(r, cx) {
        Ok((a, b)) => cx
            .neighbours
            .iter()
            .filter(|n| n.id != ev.id)
            .filter(|n| match (&n.home_id, &n.away_id) {
                (Some(h), Some(w)) => (h == &a && w == &b) || (h == &b && w == &a),
                _ => false,
            })
            .filter(|n| {
                time_of(&n.starts_at).is_some_and(|t| near(&r.dates, t.date(), 1).is_some())
            })
            .count(),
        Err(_) => 0,
    };
    // An event of those days whose sides are not known could be one of them: nothing rules it
    // out, so "no other fixture fits" is not known.
    let unknown_near = cx
        .neighbours
        .iter()
        .filter(|n| n.id != ev.id && (n.home_id.is_none() || n.away_id.is_none()))
        .any(|n| time_of(&n.starts_at).is_some_and(|t| near(&r.dates, t.date(), 1).is_some()));
    // ...and that nothing else fits is only known where the schedule is recorded in full.
    let known = rival_window(r, 0).is_some_and(|(from, to)| cx.covers(from, to));
    let late = match publication(r, start + Duration::days(LATE_DAYS), true) {
        Ok(l) => l,
        Err(v) => return v,
    };
    let reason = if rivals > 0 {
        Some("another_fixture_fits")
    } else if summary.reason.is_some() {
        summary.reason
    } else if !known || unknown_near {
        Some("schedule_unknown")
    } else {
        late
    };
    matched(
        r,
        Part::Whole,
        reason,
        vec!["league", "date", "teams"],
        false,
        true,
        false,
    )
}

fn judge_card(r: &SportRelease, ev: &EventRow, cx: &Context, start: OffsetDateTime) -> Verdict {
    let e = &cx.event_name;
    let summary = match claims::settle(Shape::Card, &Input { r, ev, cx, start }) {
        Ok(s) => s,
        Err((code, detail)) => return refuse(code, detail),
    };
    // What identifies the card: its number or serial, or both fighters' names.
    let numbers_equal =
        summary.has(Claim::CardNumber) || (summary.has(Claim::Serial) && r.number.is_some());
    let names_ok = summary.has(Claim::Sides) || summary.lead_name;
    let unverified = summary.lead_name;
    let mut reason: Option<&'static str> = summary.reason;

    // When: the publish time is checked whatever the title says (a date or a card's number
    // says which event, not when it was put up: a re-upload carries the old date too).
    let day = start.date();
    let late = match publication(
        r,
        start + Duration::days(LATE_DAYS),
        numbers_equal || !r.dates.is_empty(),
    ) {
        Ok(l) => l,
        Err(v) => return v,
    };

    // Evidence that it is this card.
    let place = !r.head.is_empty() && r.head.iter().all(|w| e.head.contains(w));
    let mut evidence = vec!["league"];
    if numbers_equal {
        evidence.push("number");
    }
    if names_ok {
        evidence.push("fighters");
    }
    // Whether the schedule is known in full for the days a rival could be in.
    let known = rival_window(r, 0).is_some_and(|(from, to)| cx.covers(from, to));
    if !numbers_equal && !names_ok {
        let titled = near(&r.dates, day, 1).is_some();
        // "No other card within a day" is only known where the schedule is recorded in full.
        let unique = known
            && cx
                .neighbours
                .iter()
                .filter(|n| n.id != ev.id)
                .filter_map(|n| time_of(&n.starts_at))
                .all(|t| days_apart(t.date(), day) > 1);
        if place {
            evidence.push("place");
            reason = reason.or(Some("place_only"));
        } else if titled && unique {
            evidence.push("date");
            reason = reason.or(Some("date_only"));
        } else {
            return refuse(
                RefusalCode::NoEvidence,
                "neither the number nor the fighters",
            );
        }
    }
    if reason.is_none() {
        reason = late;
    }
    // A match on names (not on the number, which is the card's alone) is a guess if another
    // card of the same two fighters fits as well, if a card of those days has headliners nobody
    // knows (it could be this pair's), or if the schedule is not known to be whole.
    if names_ok && !numbers_equal && reason.is_none() {
        let fits = |n: &EventRow| {
            time_of(&n.starts_at).is_some_and(|t| {
                if r.dates.is_empty() {
                    r.published.is_some_and(|p| {
                        p >= t - Duration::hours(24) && p <= t + Duration::days(LATE_DAYS)
                    })
                } else {
                    near(&r.dates, t.date(), 1).is_some()
                }
            })
        };
        let others = cx
            .neighbours
            .iter()
            .zip(&cx.neighbour_names)
            .filter(|(n, _)| n.id != ev.id && fits(n));
        let mut rivals = 0;
        let mut unknown = 0;
        for (_, nn) in others {
            match (&nn.sides, &e.sides) {
                (Some(a), Some(b)) if same_pair_runs(a, b, &e.head).is_ok() => rivals += 1,
                // A card whose headliners nobody knows could be this pair's: it is ruled out
                // only by what the release itself claims (a series, a number that differs).
                (None, _)
                    if (r.series == release::Series::None
                        || nn.series == release::Series::None
                        || nn.series == r.series)
                        && r.number.zip(nn.number).is_none_or(|(a, b)| a == b) =>
                {
                    unknown += 1;
                }
                _ => {}
            }
        }
        if rivals > 0 {
            reason = Some("another_fixture_fits");
        } else if unknown > 0 || !known {
            reason = Some("schedule_unknown");
        }
    }
    // Which part. A file with no part word is the main card only where that is what such files
    // are (UFC, `Likely`); for any other card nothing says whether it is the whole event, the
    // main card or one fight, so it is a guess that answers no part and is never recommended.
    let (part, unlabelled, established) = match r.part {
        PartHint::EarlyPrelims => (Part::EarlyPrelims, false, true),
        PartHint::Prelims => (Part::Prelims, false, true),
        PartHint::Main => (Part::Main, false, true),
        PartHint::Full => (Part::Whole, false, true),
        PartHint::None if cx.league.slug == "ufc" => (Part::Main, true, false),
        PartHint::None => (Part::Whole, true, false),
        PartHint::Day(_) | PartHint::Round(_) | PartHint::Bout => {
            return refuse(RefusalCode::NotTheEvent, "a round of something else");
        }
    };
    if unlabelled && reason.is_none() {
        reason = summary.part_reason;
    }
    matched(
        r,
        part,
        reason,
        evidence,
        unlabelled,
        established,
        unverified,
    )
}

fn judge_golf(r: &SportRelease, ev: &EventRow, cx: &Context, start: OffsetDateTime) -> Verdict {
    let summary = match claims::settle(Shape::Golf, &Input { r, ev, cx, start }) {
        Ok(s) => s,
        Err((code, detail)) => return refuse(code, detail),
    };
    const GENERIC: &[&str] = &["the", "golf", "tour", "pres", "presented", "by"];
    let (mine, _, _) = claims::golf_words(r, ev);
    let (_, last) = claims::golf_days(ev, start);
    let round_n = match r.part {
        PartHint::Day(n) | PartHint::Round(n) => Some(n),
        _ => None,
    };
    let late = match publication(
        r,
        (last + Duration::days(LATE_DAYS + 1))
            .midnight()
            .assume_utc()
            - Duration::seconds(1),
        !r.dates.is_empty(),
    ) {
        Ok(l) => l,
        Err(v) => return v,
    };
    // Another tournament of the league that the title fits as well (two events in one week),
    // by the dates it carries or, with none, by when it was published.
    let theirs_of = |n: &EventRow| -> Vec<String> {
        let t = names::tokens(&n.name);
        let cut = t
            .iter()
            .position(|w| matches!(w.as_str(), "pres" | "presented" | "by"))
            .unwrap_or(t.len());
        t[..cut]
            .iter()
            .filter(|w| !GENERIC.contains(&w.as_str()))
            .cloned()
            .collect()
    };
    let in_days = |n: &EventRow| {
        time_of(&n.starts_at).is_some_and(|s| {
            let e = n
                .ends_at
                .as_deref()
                .and_then(time_of)
                .map_or(s.date() + Duration::days(4), |e| {
                    e.date() + Duration::days(1)
                });
            let (lo, hi) = (s.date() - Duration::days(1), e + Duration::days(1));
            if r.dates.is_empty() {
                r.published
                    .is_some_and(|p| p.date() >= lo && p.date() <= hi + Duration::days(LATE_DAYS))
            } else {
                r.dates.iter().any(|d| *d >= lo && *d <= hi)
            }
        })
    };
    let rivals = cx
        .neighbours
        .iter()
        .filter(|n| n.id != ev.id)
        .filter(|n| {
            let t = theirs_of(n);
            mine.iter().all(|w| t.contains(w))
        })
        .filter(|n| in_days(n))
        .count();
    // A tournament of those days whose name nobody has (a neutral name) could be this one's
    // namesake: nothing rules it out.
    let unknown_near = cx
        .neighbours
        .iter()
        .filter(|n| n.id != ev.id && super::data::is_neutral_name(&n.league, &n.name))
        .any(in_days);
    // A tournament runs for days, so a rival can have started days before the date.
    let known = rival_window(r, 6).is_some_and(|(from, to)| cx.covers(from, to));
    let reason = if rivals > 0 {
        Some("another_fixture_fits")
    } else if summary.reason.is_some() {
        summary.reason
    } else if !known || unknown_near {
        Some("schedule_unknown")
    } else {
        late
    };
    matched(
        r,
        round_n.map_or(Part::Whole, Part::Round),
        reason,
        vec!["league", "tournament", "date"],
        false,
        true,
        false,
    )
}

/// Settles the card part of the unlabelled UFC files among the matches of one event, using
/// their siblings: files from the same uploader for the same card. Only a file whose single
/// uncertainty is its part (`unlabelled_main`, and no unverified word) is touched, and only by a
/// sibling that is itself `Sure`: an unlabelled file next to a labelled prelims file is the main
/// card; next to an explicit "Main Card" file it is the whole event. A sibling lends nothing
/// else. Which event it is, a rematch numeral, a date-only match stay as `judge` left them,
/// because a sibling's certainty is about the sibling.
pub fn settle_unlabelled(items: &mut [(Option<String>, Match)]) {
    let snapshot: Vec<(Option<String>, Part, bool)> = items
        .iter()
        .map(|(g, m)| {
            (
                g.clone(),
                m.part,
                m.confidence == Confidence::Sure && !m.unlabelled && !m.unverified,
            )
        })
        .collect();
    for (group, m) in items.iter_mut() {
        if !m.unlabelled || m.unverified || m.reason != Some("unlabelled_main") {
            continue;
        }
        let Some(group) = group else { continue };
        let sibling = |want: &dyn Fn(Part) -> bool| {
            snapshot
                .iter()
                .any(|(g, p, sure)| *sure && g.as_ref() == Some(group) && want(*p))
        };
        if sibling(&|p| p == Part::Main) {
            m.part = Part::Whole;
            m.reason = Some("unlabelled_full");
            m.confidence = Confidence::Likely;
            m.part_established = false;
        } else if sibling(&|p| matches!(p, Part::Prelims | Part::EarlyPrelims)) {
            m.reason = None;
            m.confidence = Confidence::Sure;
            m.part_established = true;
        }
    }
}
