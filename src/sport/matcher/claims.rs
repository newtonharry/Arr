//! The identity claims a title makes, and what the stored event says to each. One function,
//! `check`, answers for every claim of every shape (a game, a fight card, a golf tournament), so
//! the three judges do not each decide for themselves what counts as verified.
//!
//! Every claim is in one of four states:
//! - `Unclaimed`: the title makes no such claim (a game has no card number).
//! - `Verified`: the stored event says the same.
//! - `Unverifiable(reason)`: the title claims it and the store cannot check it (the stored card
//!   has no number, no headliners, no season). The match is at most `Likely`, with that reason.
//! - `Contradicted(code, detail)`: the store says otherwise. The release is refused.
//!
//! `Sure` needs every claim `Verified` or `Unclaimed`.

use super::{Context, RefusalCode, near, same_pair_runs, time_of, too_far};
use crate::sport::data::EventRow;
use crate::sport::names::{self, Resolved};
use crate::sport::release::{self, Assertion, PartHint, Round, Series, SportRelease, Standing};
use time::{Date, Duration, OffsetDateTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Game,
    Card,
    Golf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// "UFC 334": the number of a numbered card.
    CardNumber,
    /// "Fight Night 999", "PFL 10": the serial of a series.
    Serial,
    /// Each side's names: the two teams, the two fighters.
    Sides,
    /// The words before the names: a card's place, a tournament's name.
    Head,
    /// Every date the title carries.
    Date,
    /// The season or year of a game.
    Season,
    /// A rematch numeral ("Walker 2").
    Rematch,
    /// A round or matchday of a game.
    Round,
    /// The sport the title labels itself with ("Soccer", "Rugby"), against the event's own.
    Sport,
    /// Which part of the event the file holds: a card's part, a tournament's round or day.
    Part,
}

pub const ALL: [Claim; 10] = [
    Claim::Sport,
    Claim::Date,
    Claim::CardNumber,
    Claim::Serial,
    Claim::Sides,
    Claim::Head,
    Claim::Season,
    Claim::Rematch,
    Claim::Round,
    Claim::Part,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Unclaimed,
    Verified,
    Unverifiable(&'static str),
    Contradicted(RefusalCode, String),
}

impl Check {
    fn refused(code: RefusalCode, detail: impl Into<String>) -> Check {
        Check::Contradicted(code, detail.into())
    }
}

/// What a claim is checked against.
pub struct Input<'a> {
    pub r: &'a SportRelease,
    pub ev: &'a EventRow,
    pub cx: &'a Context<'a>,
    pub start: OffsetDateTime,
}

fn different_dates(r: &SportRelease) -> bool {
    r.dates.iter().any(|d| *d != r.dates[0])
}

/// Every season the title asserts.
fn seasons_of(r: &SportRelease) -> Vec<(u16, u16)> {
    r.assertions
        .iter()
        .filter_map(|a| match a {
            Assertion::Season(x, y) => Some((*x, *y)),
            Assertion::Round(_) => None,
        })
        .collect()
}

/// Every stage and round the title asserts.
fn rounds_of(r: &SportRelease) -> Vec<Round> {
    r.assertions
        .iter()
        .filter_map(|a| match a {
            Assertion::Round(x) => Some(*x),
            Assertion::Season(..) => None,
        })
        .collect()
}

/// Which claim an assertion is. Exhaustive: an assertion the parser can emit and no claim reads
/// does not compile.
pub fn claim_of(a: &Assertion) -> Claim {
    match a {
        Assertion::Round(_) => Claim::Round,
        Assertion::Season(..) => Claim::Season,
    }
}

/// The identity of a major championship from the words that name it, so that "The Open" (the
/// Open Championship) and "U.S. Open" are two tournaments and not one containing the other.
/// `None` for any other tournament, which is compared by its words.
fn major_of(words: &[String]) -> Option<&'static str> {
    let mut w: Vec<&str> = vec![];
    let mut i = 0;
    while i < words.len() {
        let x = words[i].as_str();
        if x == "the" {
            i += 1;
        } else if x == "u" && words.get(i + 1).is_some_and(|n| n == "s") {
            w.push("us");
            i += 2;
        } else {
            w.push(x);
            i += 1;
        }
    }
    match w.join(" ").as_str() {
        "masters" | "masters tournament" => Some("masters"),
        "pga championship" => Some("pga championship"),
        "us open" | "us open championship" => Some("us open"),
        "open" | "open championship" | "british open" | "british open championship" => {
            Some("open championship")
        }
        _ => None,
    }
}

/// The sport label claim: a title that labels itself "Soccer" or "Rugby" says what sport the
/// event is. "Football" is the soccer label of the catalogue (no gridiron football is in it) and
/// "rugby" is broad (union or league); "sport" and "sports" say nothing.
fn sport_check(r: &SportRelease, sport: crate::sport::catalog::Sport) -> Check {
    use crate::sport::catalog::Sport;
    let mut claimed = false;
    for l in &r.labels {
        let ok = match *l {
            "soccer" | "football" => sport == Sport::Soccer,
            "basketball" => sport == Sport::Basketball,
            "rugby" => matches!(sport, Sport::RugbyUnion | Sport::RugbyLeague),
            "golf" => sport == Sport::Golf,
            "mma" => sport == Sport::Mma,
            _ => continue,
        };
        claimed = true;
        if !ok {
            return Check::refused(
                RefusalCode::OtherCompetition,
                format!("{l}: labelled as another sport"),
            );
        }
    }
    if claimed {
        Check::Verified
    } else {
        Check::Unclaimed
    }
}

/// The season claim, for every shape: equality with the stored season verifies it, a season
/// that cannot be the stored one contradicts it, anything else (nothing stored, or a year inside
/// a stored span) leaves it unverified. Two different seasons in one title contradict.
fn season_check(r: &SportRelease, ev: &EventRow) -> Check {
    let mut seasons = seasons_of(r);
    if seasons.is_empty() {
        return Check::Unclaimed;
    }
    seasons.sort_unstable();
    seasons.dedup();
    if seasons.len() > 1 {
        return Check::refused(RefusalCode::WrongSeason, "two different seasons");
    }
    let (a, b) = seasons[0];
    match ev.season.as_deref().and_then(season_years) {
        Some((e0, e1)) if (a, b) == (e0, e1) => Check::Verified,
        Some((e0, e1)) if a > e1 || b < e0 => {
            Check::refused(RefusalCode::WrongSeason, "another season")
        }
        _ => Check::Unverifiable("season_unverified"),
    }
}

/// The round claim, for every shape: only an equal stored round verifies it. Two different
/// rounds of one kind in a title contradict each other; a round of another kind than the stored
/// one cannot be compared and stays unverified.
fn round_check(r: &SportRelease, ev: &EventRow) -> Check {
    let rounds = rounds_of(r);
    if rounds.is_empty() {
        return Check::Unclaimed;
    }
    for (i, a) in rounds.iter().enumerate() {
        if rounds[i + 1..]
            .iter()
            .any(|b| a.standing(b) == Standing::Different)
        {
            return Check::refused(RefusalCode::RoundMismatch, "two different rounds");
        }
    }
    let stored = ev
        .round
        .as_deref()
        .and_then(|t| release::round_of(&crate::title::tokens(t)));
    let mut worst = Check::Verified;
    for a in &rounds {
        match stored.map(|e| a.standing(&e)) {
            Some(Standing::Equal) => {}
            Some(Standing::Different) => {
                return Check::refused(RefusalCode::RoundMismatch, "another round");
            }
            Some(Standing::Incomparable) | None => worst = Check::Unverifiable("round_unverified"),
        }
    }
    worst
}

/// Both teams of a game title as the league's teams, or why they are not.
pub fn game_pair(r: &SportRelease, cx: &Context) -> Result<(String, String), Check> {
    let Some(sides) = &r.sides else {
        return Err(Check::Unclaimed);
    };
    let (a, b) = (
        names::resolve_side(&sides.left, &cx.keys, cx.teams_complete),
        names::resolve_side(&sides.right, &cx.keys, cx.teams_complete),
    );
    if a == Resolved::Ambiguous || b == Resolved::Ambiguous {
        return Err(Check::refused(
            RefusalCode::AmbiguousTeam,
            "a side fits several teams",
        ));
    }
    let (Resolved::Team(a), Resolved::Team(b)) = (a, b) else {
        return Err(Check::refused(
            RefusalCode::TeamMismatch,
            "a side is no team of the league",
        ));
    };
    if a == b {
        return Err(Check::refused(
            RefusalCode::TeamMismatch,
            "both sides are one team",
        ));
    }
    Ok((a, b))
}

fn season_years(label: &str) -> Option<(u16, u16)> {
    super::season_years(label)
}

/// The event's days for a golf tournament: the first, the last (play goes on into the evening
/// of the end date), and whether a day is inside.
pub fn golf_days(ev: &EventRow, start: OffsetDateTime) -> (Date, Date) {
    let first = start.date();
    let last = ev
        .ends_at
        .as_deref()
        .and_then(time_of)
        .map_or(first + Duration::days(4), |e| e.date() + Duration::days(1));
    (first, last)
}

/// The tournament words of a golf title that the event must have, and the event's own.
pub fn golf_words(r: &SportRelease, ev: &EventRow) -> (Vec<String>, Vec<String>, Vec<String>) {
    const GENERIC: &[&str] = &["the", "golf", "tour", "pres", "presented", "by"];
    let tournament_alias = matches!(
        r.alias.join(" ").as_str(),
        "the masters"
            | "masters tournament"
            | "pga championship"
            | "us open"
            | "u s open"
            | "the open"
            | "the open championship"
    );
    let mut mine: Vec<String> = if tournament_alias {
        r.alias.clone()
    } else {
        vec![]
    };
    for w in &r.body {
        if matches!(w.as_str(), "day" | "round" | "final" | "rd") {
            break;
        }
        mine.push(w.clone());
    }
    // (initials apart in the title, "U S Open", are one word, as in the stored name)
    let mine: Vec<String> = crate::sport::names::fold_initials(mine)
        .into_iter()
        .filter(|w| !GENERIC.contains(&w.as_str()) && w.parse::<u32>().is_err())
        .collect();
    let all_theirs = crate::sport::names::tokens(&ev.name);
    let theirs: Vec<String> = {
        let cut = all_theirs
            .iter()
            .position(|w| matches!(w.as_str(), "pres" | "presented" | "by"))
            .unwrap_or(all_theirs.len());
        all_theirs[..cut]
            .iter()
            .filter(|w| !GENERIC.contains(&w.as_str()))
            .cloned()
            .collect()
    };
    (mine, theirs, all_theirs)
}

/// The answer for one claim of one shape.
pub fn check(shape: Shape, claim: Claim, i: &Input) -> Check {
    let (r, ev, cx, start) = (i.r, i.ev, i.cx, i.start);
    let e = &cx.event_name;
    match (shape, claim) {
        // ---------------------------------------------------------------- a fight card
        (Shape::Card, Claim::CardNumber | Claim::Serial) if r.number_conflict => {
            Check::refused(RefusalCode::NumberMismatch, "two different numbers")
        }
        (Shape::Card, Claim::CardNumber) => {
            if r.series != Series::Numbered {
                return Check::Unclaimed;
            }
            if e.series == Series::FightNight {
                return Check::refused(
                    RefusalCode::SeriesMismatch,
                    "a numbered card and a Fight Night are different events",
                );
            }
            match (r.number, e.number) {
                (Some(a), Some(b)) if e.series == r.series => {
                    if a == b {
                        Check::Verified
                    } else {
                        Check::refused(RefusalCode::NumberMismatch, format!("{a} is not {b}"))
                    }
                }
                _ => Check::Unverifiable("number_unverified"),
            }
        }
        (Shape::Card, Claim::Serial) => {
            if !matches!(r.series, Series::FightNight | Series::Named) {
                return Check::Unclaimed;
            }
            if r.series == Series::FightNight && e.series == Series::Numbered {
                return Check::refused(
                    RefusalCode::SeriesMismatch,
                    "a numbered card and a Fight Night are different events",
                );
            }
            let Some(a) = r.number else {
                // "Fight Night" with no serial: the series is the claim.
                return match e.series {
                    Series::FightNight => Check::Verified,
                    _ => Check::Unverifiable("series_unverified"),
                };
            };
            match e.number {
                Some(b) if e.series == r.series => {
                    if a == b {
                        Check::Verified
                    } else {
                        Check::refused(RefusalCode::NumberMismatch, format!("{a} is not {b}"))
                    }
                }
                _ => Check::Unverifiable("serial_unverified"),
            }
        }
        (Shape::Card, Claim::Sides) => {
            let Some(rs) = &r.sides else {
                return Check::Unclaimed;
            };
            let Some(es) = &e.sides else {
                return Check::Unverifiable("names_unverified");
            };
            match same_pair_runs(rs, es, &e.head) {
                Err(_) => Check::refused(RefusalCode::FightersMismatch, "other fighters"),
                // (a given name before a surname is a word nothing checks)
                Ok(false) => Check::Unverifiable("unverified_text"),
                Ok(true) => Check::Verified,
            }
        }
        (Shape::Card, Claim::Head) => {
            if r.head.is_empty() {
                return Check::Unclaimed;
            }
            // With no names in the title, the event's own names are words it may use.
            let mut own: Vec<&String> = e.head.iter().collect();
            if r.sides.is_none() {
                own.extend(
                    e.sides
                        .iter()
                        .flat_map(|s| s.left.iter().chain(s.right.iter())),
                );
            }
            if own.is_empty() {
                return Check::Unverifiable("head_unverified");
            }
            match r.head.iter().find(|w| !own.contains(w)) {
                None => Check::Verified,
                Some(w) => {
                    Check::refused(RefusalCode::NotTheEvent, format!("unexplained word {w}"))
                }
            }
        }
        (Shape::Card, Claim::Date) => {
            if r.dates.is_empty() {
                return Check::Unclaimed;
            }
            if different_dates(r) {
                return Check::refused(RefusalCode::WrongDate, "two different dates");
            }
            match too_far(&r.dates, start.date(), 1) {
                Some(off) => Check::refused(RefusalCode::WrongDate, format!("{off} days away")),
                None => Check::Verified,
            }
        }
        // A numeral after a name on a shape that has no ordinals (a team "II", a "9"): the
        // claim is there, and nothing stored can check it.
        (Shape::Game, Claim::Rematch) => {
            if r.rematches.is_empty() {
                Check::Unclaimed
            } else {
                Check::Unverifiable("rematch_unverified")
            }
        }
        (Shape::Card, Claim::Rematch) => {
            let mut mine = r.rematches.clone();
            mine.sort_unstable();
            mine.dedup();
            if mine.len() > 1 {
                return Check::refused(RefusalCode::FightersMismatch, "two rematch numerals");
            }
            match (mine.first(), e.rematches.first()) {
                (Some(a), Some(b)) if a != b => {
                    Check::refused(RefusalCode::FightersMismatch, "another rematch")
                }
                (Some(_), Some(_)) => Check::Verified,
                // The stored title has no ordinal, or the title has none: whether an earlier
                // meeting exists says nothing about which meeting this is.
                (Some(_), None) | (None, Some(_)) => Check::Unverifiable("rematch_unverified"),
                (None, None) => Check::Unclaimed,
            }
        }
        (Shape::Card | Shape::Golf, Claim::Season) | (Shape::Game, Claim::Season) => {
            season_check(r, ev)
        }
        (_, Claim::Round) => round_check(r, ev),
        (Shape::Card, Claim::Part) => match r.part {
            PartHint::EarlyPrelims | PartHint::Prelims | PartHint::Main | PartHint::Full => {
                Check::Verified
            }
            PartHint::None => Check::Unverifiable(if cx.league.slug == "ufc" {
                "unlabelled_main"
            } else {
                "unlabelled_whole"
            }),
            PartHint::Day(_) | PartHint::Round(_) | PartHint::Bout => {
                Check::refused(RefusalCode::NotTheEvent, "a round of something else")
            }
        },

        // -------------------------------------------------------------------- a game
        (Shape::Game, Claim::Sides) => {
            let (Some(home), Some(away)) = (&ev.home_id, &ev.away_id) else {
                return Check::Unverifiable("sides_unverified");
            };
            if home == away {
                return Check::refused(
                    RefusalCode::NoEvidence,
                    "the stored event has the same team on both sides",
                );
            }
            match game_pair(r, cx) {
                Err(c) => c,
                Ok((a, b)) => {
                    if (&a == home && &b == away) || (&a == away && &b == home) {
                        Check::Verified
                    } else {
                        Check::refused(RefusalCode::TeamMismatch, "other teams")
                    }
                }
            }
        }
        (Shape::Game, Claim::Head) => {
            if r.head.is_empty() {
                Check::Unclaimed
            } else {
                Check::refused(RefusalCode::NotTheEvent, r.head.join(" "))
            }
        }
        (Shape::Game, Claim::Date) => {
            if r.dates.is_empty() {
                return Check::refused(RefusalCode::NoDate, "no date in the title");
            }
            if different_dates(r) {
                return Check::refused(RefusalCode::WrongDate, "two different dates");
            }
            match too_far(&r.dates, start.date(), 1) {
                Some(off) => Check::refused(RefusalCode::WrongDate, format!("{off} days away")),
                None => Check::Verified,
            }
        }
        (Shape::Game, Claim::Part) => match r.part {
            PartHint::None => Check::Unclaimed,
            // (a game is whole)
            PartHint::Full => Check::Verified,
            _ => Check::refused(RefusalCode::NotTheEvent, "a game has no such part"),
        },

        // ------------------------------------------------------------------- a tournament
        (Shape::Golf, Claim::Sides) => {
            if r.sides.is_some() {
                Check::refused(RefusalCode::NotTheEvent, "not a tournament round")
            } else {
                Check::Unclaimed
            }
        }
        (Shape::Golf, Claim::Head) => {
            let (mine, theirs, all_theirs) = golf_words(r, ev);
            // A major is its own identity: equal to the stored one, never contained in it. Any
            // other tournament is its words, all of which the stored name has.
            let (title_major, stored_major) = (major_of(&mine), major_of(&theirs));
            let same_major = title_major.is_some() && title_major == stored_major;
            if title_major != stored_major
                || (!same_major && (mine.is_empty() || !mine.iter().all(|w| theirs.contains(w))))
            {
                return Check::refused(RefusalCode::TournamentMismatch, "another tournament");
            }
            const GENERIC: &[&str] = &["the", "golf", "tour", "pres", "presented", "by"];
            // Nothing is accepted because it was ignored: what else the title says about the
            // tournament is the event's own word or a closed one.
            for w in &crate::sport::names::fold_initials(r.head.clone()) {
                if !all_theirs.contains(w)
                    && !GENERIC.contains(&w.as_str())
                    && !r.alias.contains(w)
                    && !mine.contains(w)
                {
                    return Check::refused(
                        RefusalCode::NotTheEvent,
                        format!("{w} is not a word of the tournament"),
                    );
                }
            }
            Check::Verified
        }
        (Shape::Golf, Claim::Date) => {
            if r.dates.is_empty() {
                return Check::Unclaimed;
            }
            let (first, last) = golf_days(ev, start);
            let in_play = |d: Date| d >= first - Duration::days(1) && d <= last + Duration::days(1);
            if !r.dates.iter().all(|d| in_play(*d)) {
                return Check::refused(RefusalCode::WrongDate, "outside the tournament days");
            }
            Check::Verified
        }
        (Shape::Golf, Claim::Part) => {
            if matches!(
                r.part,
                PartHint::Prelims | PartHint::EarlyPrelims | PartHint::Main | PartHint::Full
            ) {
                return Check::refused(RefusalCode::NotTheEvent, "not a tournament round");
            }
            let n = match r.part {
                PartHint::Day(n) | PartHint::Round(n) => n,
                _ => return Check::Unclaimed,
            };
            if !(1..=4).contains(&n) {
                return Check::refused(RefusalCode::NotTheEvent, "not one of the four rounds");
            }
            if let PartHint::Day(_) = r.part
                && !r.dates.is_empty()
            {
                let (first, _) = golf_days(ev, start);
                let want = first + Duration::days(i64::from(n) - 1);
                if near(&r.dates, want, 1).is_none() {
                    return Check::refused(RefusalCode::WrongDate, "the day does not fit the date");
                }
            }
            Check::Verified
        }

        (_, Claim::Sport) => sport_check(r, cx.league.sport),
        // A series ("Fight Night", a card number) on a game or a tournament is a claim nothing
        // stored can make true.
        (Shape::Game | Shape::Golf, Claim::CardNumber | Claim::Serial) => {
            if r.series == Series::None {
                Check::Unclaimed
            } else {
                Check::refused(
                    RefusalCode::SeriesMismatch,
                    "a series or card number on a game or a tournament",
                )
            }
        }

        // Every other pairing is a claim the shape cannot make.
        _ => Check::Unclaimed,
    }
}

/// Every claim of the shape, in the order they are applied.
pub fn all(shape: Shape, i: &Input) -> Vec<(Claim, Check)> {
    ALL.iter().map(|c| (*c, check(shape, *c, i))).collect()
}

/// What the claims came to.
pub struct Summary {
    /// The first thing nothing could check, as the reason the match is only `Likely`. The part
    /// is left out (`part_reason`): it is the weakest reason, and a sibling may settle it.
    pub reason: Option<&'static str>,
    pub part_reason: Option<&'static str>,
    pub verified: Vec<Claim>,
    /// A fighter's name was matched with a given name standing before the surname.
    pub lead_name: bool,
}

impl Summary {
    pub fn has(&self, c: Claim) -> bool {
        self.verified.contains(&c)
    }
}

/// Applies the claims: the first one the store contradicts refuses the release; the rest are
/// verified, unclaimed, or leave the match at most `Likely`.
pub fn settle(shape: Shape, i: &Input) -> Result<Summary, (RefusalCode, String)> {
    let mut s = Summary {
        reason: None,
        part_reason: None,
        verified: vec![],
        lead_name: false,
    };
    for (claim, c) in all(shape, i) {
        match c {
            Check::Contradicted(code, detail) => return Err((code, detail)),
            Check::Verified => s.verified.push(claim),
            Check::Unclaimed => {}
            Check::Unverifiable(why) => {
                if claim == Claim::Part {
                    s.part_reason = Some(why);
                } else if s.reason.is_none() {
                    s.reason = Some(why);
                }
                if claim == Claim::Sides && why == "unverified_text" {
                    s.lead_name = true;
                }
            }
        }
    }
    Ok(s)
}
