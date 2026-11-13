//! Tests of review round 4: every identity claim a title makes is verified against the stored
//! event for `sure`; one the store cannot check caps the match at `likely` with a named
//! reason; one the store contradicts refuses it. `claims` is the one function that says so for
//! all three shapes, and the walk below goes through every claim of every shape.

use super::catalog;
use super::data::{self, EventRow};
use super::find::{self, Surroundings};
use super::match_tests::{
    all_events, at, candidate, event, judge_with, known_spans, published_for, teams_of,
};
use super::matcher::claims::{self, ALL, Check, Claim, Input, Shape};
use super::matcher::{Confidence, Context, RefusalCode, Verdict, judge};
use super::release;

fn sure(v: &Verdict) -> bool {
    matches!(v, Verdict::Match(m) if m.confidence == Confidence::Sure)
}

// ------------------------------------------------------------------------------------- the walk

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Verified,
    Unverifiable(&'static str),
    Contradicted(RefusalCode),
}

struct Case {
    shape: Shape,
    claim: Claim,
    want: State,
    title: &'static str,
    base: &'static str,
    edit: fn(&mut EventRow),
}

fn keep(_: &mut EventRow) {}

const fn case(
    shape: Shape,
    claim: Claim,
    want: State,
    title: &'static str,
    base: &'static str,
    edit: fn(&mut EventRow),
) -> Case {
    Case {
        shape,
        claim,
        want,
        title,
        base,
        edit,
    }
}

use Claim as K;
use Shape::{Card, Game, Golf};
use State::{Contradicted as C, Unverifiable as U, Verified as V};

fn cases() -> Vec<Case> {
    vec![
        // ----------------------------------------------------------------- a fight card
        case(
            Card,
            K::Date,
            V,
            "UFC 333 2026 10 24 Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            keep,
        ),
        case(
            Card,
            K::Date,
            C(RefusalCode::WrongDate),
            "UFC 333 2026 11 30 Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            keep,
        ),
        case(
            Card,
            K::CardNumber,
            V,
            "UFC 333 Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            keep,
        ),
        case(
            Card,
            K::CardNumber,
            U("number_unverified"),
            "UFC 333 Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            |e| e.name = "UFC: Volkanovski vs. Evloev".into(),
        ),
        case(
            Card,
            K::CardNumber,
            C(RefusalCode::NumberMismatch),
            "UFC 334 Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            keep,
        ),
        case(
            Card,
            K::Serial,
            V,
            "UFC Fight Night 999 Cortes Acosta vs Walker Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:1",
            |e| e.name = "UFC Fight Night 999: Cortes Acosta vs. Walker".into(),
        ),
        case(
            Card,
            K::Serial,
            U("serial_unverified"),
            "UFC Fight Night 999 Cortes Acosta vs Walker Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:1",
            keep,
        ),
        case(
            Card,
            K::Serial,
            C(RefusalCode::NumberMismatch),
            "UFC Fight Night 998 Cortes Acosta vs Walker Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:1",
            |e| e.name = "UFC Fight Night 999: Cortes Acosta vs. Walker".into(),
        ),
        case(
            Card,
            K::Sides,
            V,
            "UFC 333 Volkanovski vs Evloev Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            keep,
        ),
        case(
            Card,
            K::Sides,
            U("names_unverified"),
            "UFC 333 Alice vs Bob Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            |e| e.name = "UFC 333".into(),
        ),
        case(
            Card,
            K::Sides,
            C(RefusalCode::FightersMismatch),
            "UFC 333 Silva vs Walker Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            keep,
        ),
        case(
            Card,
            K::Head,
            V,
            "PFL Tampa: Cyborg vs. Vieira Full Event 720p WEB-DL-FBB",
            "event:pfl:10",
            keep,
        ),
        case(
            Card,
            K::Head,
            U("head_unverified"),
            "PFL Tampa: Cyborg vs. Vieira Full Event 720p WEB-DL-FBB",
            "event:pfl:10",
            |e| e.name = "PFL: Cyborg vs. Vieira".into(),
        ),
        case(
            Card,
            K::Head,
            C(RefusalCode::NotTheEvent),
            "PFL Dallas: Cyborg vs. Vieira Full Event 720p WEB-DL-FBB",
            "event:pfl:10",
            keep,
        ),
        case(
            Card,
            K::Rematch,
            V,
            "PFL Austin Eblen vs Kasanganay 2 Full Event 720p WEB-DL-FBB",
            "event:pfl:13",
            keep,
        ),
        case(
            Card,
            K::Rematch,
            U("rematch_unverified"),
            "PFL Austin Eblen vs Kasanganay Full Event 720p WEB-DL-FBB",
            "event:pfl:13",
            keep,
        ),
        case(
            Card,
            K::Rematch,
            C(RefusalCode::FightersMismatch),
            "PFL Austin Eblen vs Kasanganay 3 Full Event 720p WEB-DL-FBB",
            "event:pfl:13",
            keep,
        ),
        case(
            Card,
            K::Part,
            V,
            "UFC 333 Prelims 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            keep,
        ),
        case(
            Card,
            K::Part,
            U("unlabelled_main"),
            "UFC 333 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            keep,
        ),
        case(
            Card,
            K::Part,
            C(RefusalCode::NotTheEvent),
            "UFC 333 Round 2 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            keep,
        ),
        // ------------------------------------------------------------------------ a game
        case(
            Game,
            K::Date,
            V,
            "NBA 2026 11 12 Washington Wizards vs Indiana Pacers 1080p WEB-DL-DARKSPORT",
            "event:nba:60",
            keep,
        ),
        case(
            Game,
            K::Date,
            C(RefusalCode::WrongDate),
            "NBA 2026 11 20 Washington Wizards vs Indiana Pacers 1080p WEB-DL-DARKSPORT",
            "event:nba:60",
            keep,
        ),
        case(
            Game,
            K::Sides,
            V,
            "NBA 2026 11 12 Washington Wizards vs Indiana Pacers 1080p WEB-DL-DARKSPORT",
            "event:nba:60",
            keep,
        ),
        case(
            Game,
            K::Sides,
            U("sides_unverified"),
            "NBA 2026 11 12 Washington Wizards vs Indiana Pacers 1080p WEB-DL-DARKSPORT",
            "event:nba:60",
            |e| {
                e.home_id = None;
                e.away_id = None;
            },
        ),
        case(
            Game,
            K::Sides,
            C(RefusalCode::TeamMismatch),
            "NBA 2026 11 12 Los Angeles Lakers vs Indiana Pacers 1080p WEB-DL-DARKSPORT",
            "event:nba:60",
            keep,
        ),
        case(
            Game,
            K::Head,
            C(RefusalCode::NotTheEvent),
            "NBA 2026 11 12 Cup: Washington Wizards vs Indiana Pacers 1080p WEB-DL-DARKSPORT",
            "event:nba:60",
            keep,
        ),
        case(
            Game,
            K::Season,
            V,
            "NBA 2026-2027 / 12.11.2026 / Washington Wizards @ Indiana Pacers [Basketball, WEB-DL HD/1080p, EN] [NX]",
            "event:nba:60",
            keep,
        ),
        case(
            Game,
            K::Season,
            U("season_unverified"),
            "NBA 2026-2027 / 12.11.2026 / Washington Wizards @ Indiana Pacers [Basketball, WEB-DL HD/1080p, EN] [NX]",
            "event:nba:60",
            |e| e.season = None,
        ),
        case(
            Game,
            K::Season,
            C(RefusalCode::WrongSeason),
            "NBA 2024-2025 / 12.11.2026 / Washington Wizards @ Indiana Pacers [Basketball, WEB-DL HD/1080p, EN] [NX]",
            "event:nba:60",
            keep,
        ),
        case(
            Game,
            K::Round,
            V,
            "EPL 2026-2027 / Matchweek 9 / 31.10.2026 / Chelsea FC vs Manchester United [Soccer, WEB-DL HD/720p, EN] [NX]",
            "event:premier-league:32",
            keep,
        ),
        case(
            Game,
            K::Round,
            U("round_unverified"),
            "EPL 2026-2027 / Matchweek 9 / 31.10.2026 / Chelsea FC vs Manchester United [Soccer, WEB-DL HD/720p, EN] [NX]",
            "event:premier-league:32",
            |e| e.round = None,
        ),
        case(
            Game,
            K::Round,
            C(RefusalCode::RoundMismatch),
            "EPL 2026-2027 / Matchweek 10 / 31.10.2026 / Chelsea FC vs Manchester United [Soccer, WEB-DL HD/720p, EN] [NX]",
            "event:premier-league:32",
            keep,
        ),
        case(
            Game,
            K::Part,
            C(RefusalCode::NotTheEvent),
            "NBA 2026 11 12 Prelims Washington Wizards vs Indiana Pacers 1080p WEB-DL-DARKSPORT",
            "event:nba:60",
            keep,
        ),
        // ------------------------------------------------------------------ a tournament
        case(
            Golf,
            K::Head,
            V,
            "PGA Tour World Wide Technology Championship Round 3 2026 11 07 1080p WEB-DL-DARKSPORT",
            "event:pga-tour:81",
            keep,
        ),
        case(
            Golf,
            K::Head,
            C(RefusalCode::TournamentMismatch),
            "PGA Tour Mexico Open Round 3 2026 11 07 1080p WEB-DL-DARKSPORT",
            "event:pga-tour:81",
            keep,
        ),
        case(
            Golf,
            K::Date,
            V,
            "PGA Tour World Wide Technology Championship Round 3 2026 11 07 1080p WEB-DL-DARKSPORT",
            "event:pga-tour:81",
            keep,
        ),
        case(
            Golf,
            K::Date,
            C(RefusalCode::WrongDate),
            "PGA Tour World Wide Technology Championship Round 3 2026 11 20 1080p WEB-DL-DARKSPORT",
            "event:pga-tour:81",
            keep,
        ),
        case(
            Golf,
            K::Part,
            V,
            "PGA Tour World Wide Technology Championship Round 3 2026 11 07 1080p WEB-DL-DARKSPORT",
            "event:pga-tour:81",
            keep,
        ),
        case(
            Golf,
            K::Part,
            C(RefusalCode::NotTheEvent),
            "PGA Tour World Wide Technology Championship Round 7 2026 11 07 1080p WEB-DL-DARKSPORT",
            "event:pga-tour:81",
            keep,
        ),
        case(
            Golf,
            K::Sides,
            C(RefusalCode::NotTheEvent),
            "PGA Tour World Wide Technology Championship Alice vs Bob Round 3 2026 11 07 1080p WEB-DL-DARKSPORT",
            "event:pga-tour:81",
            keep,
        ),
        case(
            Card,
            K::Sport,
            V,
            "UFC 333 Main Card [MMA] 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            keep,
        ),
        case(
            Card,
            K::Sport,
            C(RefusalCode::OtherCompetition),
            "UFC 333 Main Card [Soccer] 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            keep,
        ),
        case(
            Card,
            K::Season,
            V,
            "UFC 333 2026 Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            |e| e.season = Some("2026".into()),
        ),
        case(
            Card,
            K::Season,
            U("season_unverified"),
            "UFC 333 2026 Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            keep,
        ),
        case(
            Card,
            K::Season,
            C(RefusalCode::WrongSeason),
            "UFC 333 2024 Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            |e| e.season = Some("2026".into()),
        ),
        case(
            Card,
            K::Round,
            V,
            "UFC 333 Final Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            |e| e.round = Some("Final".into()),
        ),
        case(
            Card,
            K::Round,
            U("round_unverified"),
            "UFC 333 Final Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            keep,
        ),
        case(
            Card,
            K::Round,
            C(RefusalCode::RoundMismatch),
            "UFC 333 Semi Final Main Card 1080p WEB-DL-DARKSPORT",
            "event:ufc:3",
            |e| e.round = Some("Final".into()),
        ),
        case(
            Game,
            K::Sport,
            V,
            "NBA 2026 11 12 Washington Wizards vs Indiana Pacers [Basketball] 1080p WEB-DL-DARKSPORT",
            "event:nba:60",
            keep,
        ),
        case(
            Game,
            K::Sport,
            C(RefusalCode::OtherCompetition),
            "NBA 2026 11 12 Washington Wizards vs Indiana Pacers [Soccer] 1080p WEB-DL-DARKSPORT",
            "event:nba:60",
            keep,
        ),
        case(
            Game,
            K::Serial,
            C(RefusalCode::SeriesMismatch),
            "NBA Fight Night 2026 11 12 Washington Wizards vs Indiana Pacers 1080p WEB-DL-DARKSPORT",
            "event:nba:60",
            keep,
        ),
        case(
            Golf,
            K::Sport,
            V,
            "PGA Tour World Wide Technology Championship [Golf] Round 3 2026 11 07 1080p WEB-DL-DARKSPORT",
            "event:pga-tour:81",
            keep,
        ),
        case(
            Golf,
            K::Sport,
            C(RefusalCode::OtherCompetition),
            "PGA Tour World Wide Technology Championship [Soccer] Round 3 2026 11 07 1080p WEB-DL-DARKSPORT",
            "event:pga-tour:81",
            keep,
        ),
        case(
            Golf,
            K::Serial,
            C(RefusalCode::SeriesMismatch),
            "PGA Tour Fight Night World Wide Technology Championship Round 3 2026 11 07 1080p WEB-DL-DARKSPORT",
            "event:pga-tour:81",
            keep,
        ),
        case(
            Golf,
            K::Season,
            V,
            "PGA Tour 2026 World Wide Technology Championship Round 3 2026 11 07 1080p WEB-DL-DARKSPORT",
            "event:pga-tour:81",
            |e| e.season = Some("2026".into()),
        ),
        case(
            Golf,
            K::Season,
            U("season_unverified"),
            "PGA Tour 2026 World Wide Technology Championship Round 3 2026 11 07 1080p WEB-DL-DARKSPORT",
            "event:pga-tour:81",
            keep,
        ),
        case(
            Golf,
            K::Season,
            C(RefusalCode::WrongSeason),
            "PGA Tour 2024 World Wide Technology Championship Round 3 2026 11 07 1080p WEB-DL-DARKSPORT",
            "event:pga-tour:81",
            |e| e.season = Some("2026".into()),
        ),
        case(
            Golf,
            K::Round,
            V,
            "PGA Tour World Wide Technology Championship Round 3 Playoffs 2026 11 07 1080p WEB-DL-DARKSPORT",
            "event:pga-tour:81",
            |e| e.round = Some("Playoffs".into()),
        ),
        case(
            Golf,
            K::Round,
            U("round_unverified"),
            "PGA Tour World Wide Technology Championship Round 3 Playoffs 2026 11 07 1080p WEB-DL-DARKSPORT",
            "event:pga-tour:81",
            keep,
        ),
        case(
            Golf,
            K::Round,
            C(RefusalCode::RoundMismatch),
            "PGA Tour World Wide Technology Championship Round 3 Semi Final 2026 11 07 1080p WEB-DL-DARKSPORT",
            "event:pga-tour:81",
            |e| e.round = Some("Final".into()),
        ),
        case(
            Game,
            K::Rematch,
            U("rematch_unverified"),
            "NBA 2026 11 12 Washington Wizards vs Indiana Pacers II 1080p WEB-DL-DARKSPORT",
            "event:nba:60",
            keep,
        ),
    ]
}

/// The claims a shape cannot make, or cannot be unable to check, with the reason. Every other
/// claim must have a row in each of the three states.
fn not_applicable(shape: Shape, claim: Claim, state: &str) -> Option<&'static str> {
    use Claim::*;
    match (shape, claim, state) {
        (_, Sport, "unverifiable") => Some("an event always has a sport"),
        (Game | Golf, CardNumber, _) => Some("only a card has a number"),
        (Game | Golf, Serial, "verified" | "unverifiable") => {
            Some("a game or a tournament has no series: any is a contradiction")
        }
        (Golf, Rematch, _) => Some("a tournament has no sides to carry a numeral"),
        (Game, Rematch, "verified" | "contradicted") => {
            Some("a numeral after a team is only ever unverified")
        }
        (_, Date, "unverifiable") => Some("an event always has a start to check a date against"),
        (Game, Head, "verified" | "unverifiable") => {
            Some("a game has no head words: any is a contradiction")
        }
        (Game, Part, "verified" | "unverifiable") => {
            Some("a game has no parts: any is a contradiction")
        }
        (Golf, Head | Part, "unverifiable") => {
            Some("a tournament's name and rounds are always stored or absent from the title")
        }
        (Golf, Sides, "verified" | "unverifiable") => {
            Some("a tournament has no sides: any is a contradiction")
        }
        _ => None,
    }
}

fn judge_case(c: &Case) -> (claims::Check, Verdict) {
    let mut ev = event(c.base);
    (c.edit)(&mut ev);
    let league = catalog::find(&ev.league).unwrap();
    let teams = teams_of(&ev.league);
    let cx = Context::new(
        league,
        &ev,
        &teams,
        &[],
        false,
        true,
        known_spans(true),
        true,
    );
    let r = release::parse(c.title, published_for(None, &ev));
    let start = at(&ev.starts_at);
    let check = claims::check(
        c.shape,
        c.claim,
        &Input {
            r: &r,
            ev: &ev,
            cx: &cx,
            start,
        },
    );
    (check, judge(&r, &ev, &cx))
}

#[test]
fn every_claim_of_every_shape_is_checked_in_all_three_states() {
    let all = cases();
    for c in &all {
        let (check, verdict) = judge_case(c);
        let ctx = format!("{:?}/{:?}/{:?}: {}", c.shape, c.claim, c.want, c.title);
        match (c.want, &check) {
            (State::Verified, Check::Verified) => {
                assert!(sure(&verdict), "{ctx}: {verdict:?}");
            }
            (State::Unverifiable(why), Check::Unverifiable(got)) => {
                assert_eq!(why, *got, "{ctx}");
                // Whatever else is said, it is not `sure`: a claim nothing could check caps it.
                assert!(!sure(&verdict), "{ctx}: {verdict:?}");
                if let Some(m) = verdict.matched() {
                    assert_eq!(m.confidence, Confidence::Likely, "{ctx}");
                    assert!(m.reason.is_some(), "{ctx}");
                }
            }
            (State::Contradicted(code), Check::Contradicted(got, _)) => {
                assert_eq!(code, *got, "{ctx}");
                assert_eq!(verdict.refusal(), Some(code), "{ctx}: {verdict:?}");
            }
            _ => panic!("{ctx}: the claim came to {check:?}"),
        }
    }
    // Nothing is skipped: every claim of every shape has all three states, or says why not.
    for shape in [Card, Game, Golf] {
        for claim in ALL {
            for (state, label) in [(0, "verified"), (1, "unverifiable"), (2, "contradicted")] {
                let has = all.iter().any(|c| {
                    c.shape == shape
                        && c.claim == claim
                        && matches!(
                            (state, c.want),
                            (0, State::Verified)
                                | (1, State::Unverifiable(_))
                                | (2, State::Contradicted(_))
                        )
                });
                if has {
                    continue;
                }
                // (a claim a shape never makes needs no rows; one that is only ever
                // contradicted has the other two states explained)
                assert!(
                    not_applicable(shape, claim, label).is_some(),
                    "{shape:?}/{claim:?} has no {label} row and no reason it cannot"
                );
            }
        }
    }
}

// ----------------------------------------------------------------------- C18, C19 and C20

#[test]
fn names_the_store_cannot_check_cap_the_match() {
    // The stored card has no headliners: the title's names cannot be verified.
    let mut ev = event("event:ufc:3");
    ev.name = "UFC 333".into();
    let v = judge_with(
        "UFC 333 Alice vs Bob Main Card 1080p WEB-DL-DARKSPORT",
        Some("2026-10-25T01:00:00Z"),
        &ev,
        false,
    );
    let m = v.matched().expect("the number still identifies the card");
    assert_eq!(
        (m.confidence, m.reason),
        (Confidence::Likely, Some("names_unverified"))
    );
    // Head words on that path are not waved through either.
    let v = judge_with(
        "UFC 333 Austin Alice vs Bob Main Card 1080p WEB-DL-DARKSPORT",
        Some("2026-10-25T01:00:00Z"),
        &ev,
        false,
    );
    assert!(!sure(&v), "{v:?}");
}

#[test]
fn a_card_with_unknown_headliners_in_the_window_stops_a_names_only_match() {
    let a = event("event:ufc:5");
    let mut a = a;
    a.starts_at = "2026-11-15T22:00:00Z".into();
    let mut b = event("event:ufc:6");
    b.starts_at = "2026-11-22T22:00:00Z".into();
    b.name = "UFC event on 2026-11-22".into();
    assert!(data::is_neutral_name("ufc", &b.name));
    let neighbours = vec![a.clone(), b.clone()];
    let teams = teams_of("ufc");
    let league = catalog::find("ufc").unwrap();
    // Recorded complete over 8 to 24 November.
    let spans = vec![(at("2026-10-25T00:00:00Z"), at("2026-11-24T00:00:00Z"))];
    let cx = Context::new(league, &a, &teams, &neighbours, false, true, spans, true);
    let title = "UFC Fight Night Walker vs Hill Main Card 1080p WEB-DL-DARKSPORT";
    let v = judge(
        &release::parse(title, Some(at("2026-11-23T01:00:00Z"))),
        &a,
        &cx,
    );
    let m = v.matched().expect("fits A by names");
    assert_eq!(
        (m.confidence, m.reason),
        (Confidence::Likely, Some("schedule_unknown")),
        "{v:?}"
    );
    // Published right after A, B is outside the window: A is sure.
    let v = judge(
        &release::parse(title, Some(at("2026-11-16T01:00:00Z"))),
        &a,
        &cx,
    );
    assert!(sure(&v), "{v:?}");
    // A date in the title that rules B out does as well.
    let dated = "UFC Fight Night Walker vs Hill 2026 11 15 Main Card 1080p WEB-DL-DARKSPORT";
    let v = judge(
        &release::parse(dated, Some(at("2026-11-23T01:00:00Z"))),
        &a,
        &cx,
    );
    assert!(!sure(&v) || v.matched().is_some());
    // Golf: a tournament of the days with a name nobody has.
    let g = event("event:pga-tour:81");
    let mut other = event("event:pga-tour:81");
    other.id = "event:pga-tour:99".into();
    other.name = "PGA Tour event on 2026-11-05".into();
    let neighbours = vec![g.clone(), other];
    let teams = teams_of("pga-tour");
    let gl = catalog::find("pga-tour").unwrap();
    let cx = Context::new(
        gl,
        &g,
        &teams,
        &neighbours,
        false,
        true,
        vec![(at("2026-10-20T00:00:00Z"), at("2026-12-01T00:00:00Z"))],
        true,
    );
    let t = "PGA Tour World Wide Technology Championship Round 3 2026 11 07 1080p WEB-DL-DARKSPORT";
    let v = judge(
        &release::parse(t, Some(at("2026-11-08T05:00:00Z"))),
        &g,
        &cx,
    );
    assert_eq!(
        v.matched().map(|m| (m.confidence, m.reason)),
        Some((Confidence::Likely, Some("schedule_unknown"))),
        "{v:?}"
    );
}

#[test]
fn a_serial_the_store_cannot_verify_caps_the_match() {
    let ev = event("event:ufc:1");
    let publ = Some("2026-11-08T01:00:00Z");
    // A serial the schedule source does not carry: named, and never sure.
    let v = judge_with(
        "UFC Fight Night 999 Cortes Acosta vs Walker Main Card 1080p WEB-DL-DARKSPORT",
        publ,
        &ev,
        false,
    );
    let m = v.matched().unwrap();
    assert_eq!(
        (m.confidence, m.reason),
        (Confidence::Likely, Some("serial_unverified"))
    );
    // The same release with no serial is sure: nothing is claimed that cannot be checked.
    let v = judge_with(
        "UFC Fight Night Cortes Acosta vs Walker Main Card 1080p WEB-DL-DARKSPORT",
        publ,
        &ev,
        false,
    );
    assert!(sure(&v), "{v:?}");
    // A serial that contradicts a stored one refuses.
    let mut stored = ev.clone();
    stored.name = "UFC Fight Night 300: Cortes Acosta vs. Walker".into();
    let v = judge_with(
        "UFC Fight Night 299 Cortes Acosta vs Walker Main Card 1080p WEB-DL-DARKSPORT",
        publ,
        &stored,
        false,
    );
    assert_eq!(v.refusal(), Some(RefusalCode::NumberMismatch));
    // A sibling does not lend a serial: the unlabelled file stays at `likely`.
    let league = catalog::find("ufc").unwrap();
    let found = vec![
        candidate(
            "UFC Fight Night 999 Cortes Acosta vs Walker 1080p WEB-DL-DARKSPORT",
            9,
            "2026-11-08T01:00:00Z",
        ),
        candidate(
            "UFC Fight Night 999 Cortes Acosta vs Walker Prelims 1080p WEB-DL-DARKSPORT",
            9,
            "2026-11-08T01:00:00Z",
        ),
    ];
    let j = find::judge_all(found, &ev, league, &Surroundings::all_known(), None);
    assert!(
        j.candidates
            .iter()
            .all(|c| c.m.confidence == Confidence::Likely),
        "{:?}",
        j.candidates.iter().map(|c| &c.m).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------------------- C7

#[test]
fn evidence_lives_as_long_as_the_span_that_relies_on_it() {
    use super::espn::{EventState, Fixture, Side};
    let side = |id: &str, name: &str| Side {
        id: id.into(),
        name: name.into(),
        abbreviation: None,
    };
    let fx = |id: &str, when: &str| Fixture {
        id: id.into(),
        starts_at: at(when),
        ends_at: None,
        season: Some("2026-2027".into()),
        name: Some("Indiana Pacers at Washington Wizards".into()),
        short_name: None,
        home: Some(side("1", "Washington Wizards")),
        away: Some(side("2", "Indiana Pacers")),
        round: None,
        venue: None,
        state: EventState::Scheduled,
    };
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    data::create_tables(&c).unwrap();
    // The first read, on 13 November: the 11th is not kept, only noted.
    let (f11, f12) = (
        fx("301", "2026-11-11T01:00:00Z"),
        fx("302", "2026-11-12T01:00:00Z"),
    );
    data::upsert_events(
        &mut c,
        "nba",
        &[&f11, &f12],
        at("2026-11-12T00:00:00Z"),
        at("2026-11-13T12:00:00Z"),
    )
    .unwrap();
    // 30 November: November is recorded complete.
    data::record_ranges(
        &c,
        "nba",
        &[(at("2026-11-02T00:00:00Z"), at("2026-12-01T00:00:00Z"))],
        at("2026-11-30T12:00:00Z"),
    )
    .unwrap();
    let judged = |c: &rusqlite::Connection, now: &str| {
        let ev = data::event(c, "event:nba:302").unwrap().unwrap();
        let start = at(&ev.starts_at);
        let n = data::pair_events_between(
            c,
            "nba",
            "1",
            "2",
            start - time::Duration::days(3),
            start + time::Duration::days(3),
        )
        .unwrap();
        let spans = data::complete_spans(c, "nba", at(now)).unwrap();
        let teams = teams_of("nba");
        let cx = Context::new(
            catalog::find("nba").unwrap(),
            &ev,
            &teams,
            &n,
            false,
            true,
            spans,
            true,
        );
        judge(
            &release::parse(
                "NBA 2026 11 12 Washington Wizards vs Indiana Pacers 1080p WEB-DL",
                Some(at("2026-11-12T05:00:00Z")),
            ),
            &ev,
            &cx,
        )
    };
    let v = judged(&c, "2026-12-02T00:00:00Z");
    assert_eq!(
        v.matched().and_then(|m| m.reason),
        Some("another_fixture_fits"),
        "{v:?}"
    );
    // 13 March: pruning runs (a refresh of the league records its spans again). The November
    // span is still current, so the evidence under it is still there.
    data::record_ranges(
        &c,
        "nba",
        &[(at("2027-03-01T00:00:00Z"), at("2027-04-01T00:00:00Z"))],
        at("2027-03-13T12:00:00Z"),
    )
    .unwrap();
    let v = judged(&c, "2027-03-13T12:00:00Z");
    assert!(!sure(&v), "{v:?}");
    assert_eq!(
        v.matched().and_then(|m| m.reason),
        Some("another_fixture_fits"),
        "{v:?}"
    );
    // Once the span itself has expired, the evidence may go with it, and nothing certifies the
    // days any more.
    data::record_ranges(
        &c,
        "nba",
        &[(at("2027-04-01T00:00:00Z"), at("2027-05-01T00:00:00Z"))],
        at("2027-04-05T12:00:00Z"),
    )
    .unwrap();
    assert!(
        data::complete_spans(&c, "nba", at("2027-04-05T12:00:00Z"))
            .unwrap()
            .iter()
            .all(|(f, _)| *f > at("2026-12-01T00:00:00Z"))
    );
    let v = judged(&c, "2027-04-05T12:00:00Z");
    assert!(!sure(&v), "{v:?}");
}

// --------------------------------------------------------------------------------------- C15

#[test]
fn a_bout_marker_split_by_brackets_is_still_a_bout_marker() {
    let ev = event("event:ufc:3");
    let publ = Some("2026-10-25T01:00:00Z");
    for title in [
        "UFC 333 Prelims Main [Event] 1080p WEB-DL-DARKSPORT",
        "UFC 333 Prelims [Main] Event 1080p WEB-DL-DARKSPORT",
        "UFC 333 Prelims (Main) {Event} 1080p WEB-DL-DARKSPORT",
        "UFC 333 Prelims [Main | Event] 1080p WEB-DL-DARKSPORT",
        "UFC 333 Main Card [Co] - [Main] 1080p WEB-DL-DARKSPORT",
        "UFC 333 Main Card [Full] [Fight] 1080p WEB-DL-DARKSPORT",
        "UFC 333 Main Card [Wrap] (Up) 1080p WEB-DL-DARKSPORT",
        "UFC 333 Main Card (High) [lights] 1080p WEB-DL-DARKSPORT",
    ] {
        let v = judge_with(title, publ, &ev, false);
        assert!(v.matched().is_none(), "{title}: {v:?}");
    }
    // Two different parts anywhere in a title conflict.
    for title in [
        "UFC 333 Prelims [Main Card] 1080p WEB-DL-DARKSPORT",
        "UFC 333 Prelims (Full) 1080p WEB-DL-DARKSPORT",
    ] {
        let v = judge_with(title, publ, &ev, false);
        assert!(v.matched().is_none(), "{title}: {v:?}");
    }
    // A combined file that says so is a whole event, as before.
    let v = judge_with(
        "UFC 333 Full Event [Main Card and Prelims] 1080p WEB-DL-DARKSPORT",
        publ,
        &ev,
        false,
    );
    assert!(
        v.matched().is_some_and(|m| m.part == super::Part::Whole),
        "{v:?}"
    );
    let _ = all_events();
}
