//! Tests of review round 6: a series on a game, a conflicting sport label, name suffixes, one
//! tournament contained in another, a missing uploader tag, and the real events that were wrongly
//! refused (a "Junior" in a fighter's name, a "1." in a club's name, a rematch numeral read as a
//! card number). The last test is the table of real titles that must match.

use super::catalog;
use super::data::{EventRow, TeamRow};
use super::match_tests::{at, event, judge_with, known_spans};
use super::matcher::{Confidence, Context, RefusalCode, Verdict, judge};
use super::release;

fn sure(v: &Verdict) -> bool {
    matches!(v, Verdict::Match(m) if m.confidence == Confidence::Sure)
}

fn reason(v: &Verdict) -> Option<&'static str> {
    v.matched().and_then(|m| m.reason)
}

fn stored(id: &str, name: &str, start: &str) -> EventRow {
    let mut e = event("event:ufc:3");
    e.id = id.into();
    e.league = id.split(':').nth(1).unwrap().into();
    e.name = name.into();
    e.starts_at = start.into();
    e.ends_at = None;
    e
}

fn team(league: &str, id: &str, name: &str) -> TeamRow {
    TeamRow {
        league: league.into(),
        id: id.into(),
        name: name.into(),
        short_name: None,
        abbreviation: None,
    }
}

// ------------------------------------------------------------------------------------ C25, C28

#[test]
fn c25_a_series_on_a_game_and_c28_a_sport_label_that_is_not_the_events() {
    let nba = event("event:nba:60");
    let v = judge_with(
        "NBA Fight Night 2026 11 12 Washington Wizards vs Indiana Pacers 1080p WEB-DL-DARKSPORT",
        None,
        &nba,
        false,
    );
    assert_eq!(v.refusal(), Some(RefusalCode::SeriesMismatch), "{v:?}");
    let ufc = event("event:ufc:3");
    let publ = Some("2026-10-25T01:00:00Z");
    let v = judge_with(
        "UFC 333 Main Card 1080p WEB-DL-DARKSPORT [Soccer]",
        publ,
        &ufc,
        false,
    );
    assert_eq!(v.refusal(), Some(RefusalCode::OtherCompetition), "{v:?}");
    // The label that is right, and the documented equivalents.
    let v = judge_with(
        "UFC 333 Main Card 1080p WEB-DL-DARKSPORT [MMA]",
        publ,
        &ufc,
        false,
    );
    assert!(sure(&v), "{v:?}");
    let epl = event("event:premier-league:32");
    for label in ["Soccer", "Football"] {
        let v = judge_with(
            &format!(
                "EPL 2026-2027 / Matchweek 9 / 31.10.2026 / Chelsea FC vs Manchester United [{label}, WEB-DL HD/720p, EN] [NX]"
            ),
            None,
            &epl,
            false,
        );
        assert!(sure(&v), "{label}: {v:?}");
    }
    let v = judge_with(
        "EPL 2026-2027 / Matchweek 9 / 31.10.2026 / Chelsea FC vs Manchester United [Basketball, WEB-DL HD/720p, EN] [NX]",
        None,
        &epl,
        false,
    );
    assert_eq!(v.refusal(), Some(RefusalCode::OtherCompetition), "{v:?}");
    // "Rugby" is broad: union or league.
    let six = event("event:six-nations:70");
    let v = judge_with(
        "Guinness Six Nations 2022 / Round1 / Scotland. vs. England [Rugby, 05.02.2022, HDTV/1080p, MKV/H.264, EN] [NX]",
        None,
        &six,
        false,
    );
    assert!(sure(&v), "{v:?}");
}

// ---------------------------------------------------------------------------------------- C29

#[test]
fn c29_explicit_suffixes_are_compared_before_they_are_dropped() {
    let ev = stored(
        "event:ufc:5001",
        "UFC 333: Volkanovski Jr vs. Evloev",
        "2026-10-24T14:00:00Z",
    );
    let publ = Some("2026-10-25T01:00:00Z");
    let v = judge_with(
        "UFC 333 Volkanovski Sr vs Evloev Main Card 1080p WEB-DL-DARKSPORT",
        publ,
        &ev,
        false,
    );
    assert_eq!(v.refusal(), Some(RefusalCode::FightersMismatch), "{v:?}");
    let v = judge_with(
        "UFC 333 Volkanovski Jr vs Evloev Main Card 1080p WEB-DL-DARKSPORT",
        publ,
        &ev,
        false,
    );
    assert!(sure(&v), "{v:?}");
    // A suffix the store lacks (or the title lacks) is unverified.
    let v = judge_with(
        "UFC 333 Volkanovski vs Evloev Main Card 1080p WEB-DL-DARKSPORT",
        publ,
        &ev,
        false,
    );
    assert_eq!(reason(&v), Some("unverified_text"), "{v:?}");
}

// ---------------------------------------------------------------------------------------- C30

#[test]
fn c30_one_major_is_not_contained_in_another() {
    let mut us = event("event:pga-tour:81");
    us.name = "U.S. Open".into();
    us.starts_at = "2026-06-18T12:00:00Z".into();
    us.ends_at = Some("2026-06-21T21:00:00Z".into());
    let publ = Some("2026-06-22T05:00:00Z");
    let at_us = |t: &str| judge_with(t, publ, &us, false);
    let v = at_us("PGA Tour The Open Round 3 2026 06 20 1080p WEB-DL-NX");
    assert_eq!(v.refusal(), Some(RefusalCode::TournamentMismatch), "{v:?}");
    let v = at_us("The Open Round 3 2026 06 20 1080p WEB-DL-NX");
    assert!(v.matched().is_none(), "{v:?}");
    let v = at_us("PGA Tour Open Round 3 2026 06 20 1080p WEB-DL-NX");
    assert!(v.matched().is_none(), "{v:?}");
    // The right one still matches, in each spelling.
    for t in [
        "PGA Tour US Open Round 3 2026 06 20 1080p WEB-DL-NX",
        "PGA Tour U.S. Open Round 3 2026 06 20 1080p WEB-DL-NX",
    ] {
        let v = at_us(t);
        assert!(v.matched().is_some(), "{t}: {v:?}");
    }
}

// ---------------------------------------------------------------------------------------- C31

#[test]
fn c31_a_missing_uploader_tag_is_a_named_caution() {
    let ufc = event("event:ufc:3");
    let publ = Some("2026-10-25T01:00:00Z");
    let v = judge_with("UFC 333 Main Card 1080p WEB-DL", publ, &ufc, false);
    assert_eq!(reason(&v), Some("no_uploader_tag"), "{v:?}");
    let v = judge_with(
        "UFC 333 Main Card 1080p WEB-DL-DARKSPORT",
        publ,
        &ufc,
        false,
    );
    assert!(sure(&v), "{v:?}");
    // A forum-style title has no tag by nature: capped as well.
    let epl = event("event:premier-league:32");
    let forum = "EPL 2026-2027 / Matchweek 9 / 31.10.2026 / Chelsea FC vs Manchester United [Soccer, WEB-DL HD/720p, EN]";
    let v = judge_with(forum, None, &epl, false);
    assert_eq!(reason(&v), Some("no_uploader_tag"), "{v:?}");
    // The cap survives a labelled sibling: it does not make an untagged file the main card.
    let league = catalog::find("ufc").unwrap();
    let found = vec![
        super::match_tests::candidate("UFC 333 1080p WEB-DL", 9, "2026-10-25T01:00:00Z"),
        super::match_tests::candidate(
            "UFC 333 Prelims 1080p WEB-DL-DARKSPORT",
            9,
            "2026-10-25T01:00:00Z",
        ),
    ];
    let j = super::find::judge_all(
        found,
        &ufc,
        league,
        &super::find::Surroundings::all_known(),
        None,
    );
    let bare = j
        .candidates
        .iter()
        .find(|c| !c.m.part_established)
        .expect("the untagged one");
    assert_ne!(bare.m.confidence, Confidence::Sure);
}

// ------------------------------------------------------------------------ C32, C33, C34, C35

#[test]
fn c32_c33_c35_real_events_that_were_refused() {
    // C32: "Junior" is part of a fighter's name.
    let ev = stored(
        "event:ufc:5002",
        "UFC 166: Cain Velasquez vs. Junior dos Santos 3",
        "2013-10-20T02:00:00Z",
    );
    let v = judge_with(
        "UFC 166 Cain Velasquez vs Junior dos Santos 3 Main Card 1080p WEB-DL-DARKSPORT",
        Some("2013-10-20T12:00:00Z"),
        &ev,
        false,
    );
    assert!(v.matched().is_some(), "{v:?}");
    // ...while an actual junior competition stays refused.
    let reserves = judge_with(
        "EPL 2019 01 19 Liverpool vs Crystal Palace Reserves 720p-NX",
        None,
        &event("event:premier-league:30"),
        false,
    );
    assert_eq!(
        reserves.refusal(),
        Some(RefusalCode::OtherCompetition),
        "{reserves:?}"
    );
    // C35: the numeral after the left name is the rematch numeral, not a second card number.
    let ev = stored(
        "event:ufc:5003",
        "UFC 227: Dillashaw vs. Garbrandt 2",
        "2018-08-05T01:00:00Z",
    );
    let v = judge_with(
        "UFC 227 Dillashaw 2 vs Garbrandt Main Card 1080p WEB-DL-DARKSPORT",
        Some("2018-08-05T12:00:00Z"),
        &ev,
        false,
    );
    assert!(v.matched().is_some(), "{v:?}");
    assert_eq!(
        v.matched().map(|m| m.confidence),
        Some(Confidence::Sure),
        "{v:?}"
    );
    // C33: a leading "1." belongs to the club's stored name.
    let league = catalog::find("bundesliga").unwrap();
    let mut ev = event("event:bundesliga:90");
    ev.id = "event:bundesliga:9101".into();
    ev.name = "1. FC Union Berlin v Borussia Dortmund".into();
    ev.home_id = Some("11".into());
    ev.home_name = Some("1. FC Union Berlin".into());
    ev.away_id = Some("12".into());
    ev.away_name = Some("Borussia Dortmund".into());
    ev.starts_at = "2026-11-07T15:30:00Z".into();
    ev.season = Some("2026-2027".into());
    let teams = vec![
        team("bundesliga", "11", "1. FC Union Berlin"),
        team("bundesliga", "12", "Borussia Dortmund"),
    ];
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
    let title =
        "Bundesliga 1. FC Union Berlin vs Borussia Dortmund 2026 11 07 1080p WEB-DL-DARKSPORT";
    let v = judge(
        &release::parse(title, Some(at("2026-11-07T20:00:00Z"))),
        &ev,
        &cx,
    );
    assert!(v.matched().is_some(), "{v:?}");
    // A real division number is still another tier.
    let mut other = ev.clone();
    other.home_name = Some("Union Berlin".into());
    let teams2 = vec![
        team("bundesliga", "11", "Union Berlin"),
        team("bundesliga", "12", "Borussia Dortmund"),
    ];
    let cx2 = Context::new(
        league,
        &other,
        &teams2,
        &[],
        false,
        true,
        known_spans(true),
        true,
    );
    let v = judge(
        &release::parse(
            "Bundesliga 2 Union Berlin vs Borussia Dortmund 2026 11 07 1080p WEB-DL-DARKSPORT",
            Some(at("2026-11-07T20:00:00Z")),
        ),
        &other,
        &cx2,
    );
    assert_eq!(v.refusal(), Some(RefusalCode::QualifiedLeague), "{v:?}");
}

#[test]
fn c34_each_league_has_its_gender() {
    let lpga = event("event:lpga-tour:80");
    let publ = Some("2023-07-23T10:00:00Z");
    let v = judge_with(
        "LPGA Tour DOW Invitational Round 3 Men 2023 07 22 1080p WEB-DL-NX",
        publ,
        &lpga,
        false,
    );
    assert_eq!(v.refusal(), Some(RefusalCode::OtherCompetition), "{v:?}");
    let v = judge_with(
        "LPGA Tour DOW Invitational Round 3 Women 2023 07 22 1080p WEB-DL-NX",
        publ,
        &lpga,
        false,
    );
    assert!(v.matched().is_some(), "{v:?}");
    // A men's league still refuses "Women".
    let nba = event("event:nba:60");
    let v = judge_with(
        "NBA Women 2026 11 12 Washington Wizards vs Indiana Pacers 1080p WEB-DL-NX",
        None,
        &nba,
        false,
    );
    assert!(v.matched().is_none(), "{v:?}");
    assert!(catalog::find("lpga-tour").unwrap().women());
    assert!(catalog::find("wnba").unwrap().women());
    assert!(!catalog::find("pga-tour").unwrap().women());
}

// ------------------------------------------------------------------- real titles must match

/// (league, home, away, start, season, round) of real games with awkward names.
const GAMES: &[(&str, &str, &str, &str)] = &[
    (
        "bundesliga",
        "1. FC Koln",
        "RB Leipzig",
        "2026-11-09T17:30:00Z",
    ),
    (
        "bundesliga",
        "1. FC Heidenheim",
        "Hannover 96",
        "2026-11-09T17:30:00Z",
    ),
    (
        "bundesliga",
        "FC Schalke 04",
        "TSV 1860 Munchen",
        "2026-11-09T17:30:00Z",
    ),
    (
        "bundesliga",
        "1. FSV Mainz 05",
        "Racing 92",
        "2026-11-09T17:30:00Z",
    ),
    (
        "bundesliga",
        "Boca Juniors",
        "Argentinos Juniors",
        "2026-11-09T17:30:00Z",
    ),
    (
        "bundesliga",
        "Newell's Old Boys",
        "Young Boys",
        "2026-11-10T17:30:00Z",
    ),
    (
        "mls",
        "Atlanta United",
        "D.C. United",
        "2026-11-11T00:30:00Z",
    ),
    (
        "mls",
        "LA Galaxy",
        "St. Louis City SC",
        "2026-11-12T00:30:00Z",
    ),
    (
        "a-league-men",
        "Wellington Phoenix",
        "A.F.C. Sydney",
        "2026-11-13T08:35:00Z",
    ),
    (
        "premier-league",
        "Brighton & Hove Albion",
        "Wolverhampton Wanderers",
        "2026-11-07T15:00:00Z",
    ),
    (
        "premier-league",
        "Nottingham Forest",
        "Manchester United",
        "2026-11-08T14:00:00Z",
    ),
    (
        "premier-league",
        "Newcastle United",
        "Manchester City",
        "2026-11-09T14:00:00Z",
    ),
    (
        "la-liga",
        "Real Betis",
        "Athletic Club",
        "2026-11-07T20:00:00Z",
    ),
    (
        "la-liga",
        "Atletico Madrid",
        "Real Sociedad",
        "2026-11-08T20:00:00Z",
    ),
    (
        "la-liga",
        "Real Madrid",
        "Sporting Gijon",
        "2026-11-09T20:00:00Z",
    ),
    (
        "bundesliga",
        "1. FC Union Berlin",
        "Borussia Dortmund",
        "2026-11-07T15:30:00Z",
    ),
    (
        "bundesliga",
        "1. FC Heidenheim",
        "1. FSV Mainz 05",
        "2026-11-07T15:30:00Z",
    ),
    (
        "bundesliga",
        "Bayer 04 Leverkusen",
        "Borussia Monchengladbach",
        "2026-11-08T15:30:00Z",
    ),
    (
        "bundesliga",
        "FC St. Pauli",
        "VfL Wolfsburg",
        "2026-11-08T17:30:00Z",
    ),
    (
        "bundesliga",
        "TSV 1860 Munchen",
        "SV Werder Bremen",
        "2026-11-09T17:30:00Z",
    ),
    ("serie-a", "Inter Milan", "AS Roma", "2026-11-07T19:45:00Z"),
    (
        "serie-a",
        "AC Milan",
        "Hellas Verona",
        "2026-11-08T19:45:00Z",
    ),
    (
        "ligue-1",
        "Paris Saint-Germain",
        "Olympique Lyonnais",
        "2026-11-07T20:00:00Z",
    ),
    (
        "ligue-1",
        "Stade Rennais",
        "AS Monaco",
        "2026-11-08T20:00:00Z",
    ),
    (
        "mls",
        "Inter Miami CF",
        "D.C. United",
        "2026-11-07T00:30:00Z",
    ),
    (
        "mls",
        "Sporting Kansas City",
        "St. Louis City SC",
        "2026-11-08T00:30:00Z",
    ),
    (
        "mls",
        "Real Salt Lake",
        "Atlanta United",
        "2026-11-09T00:30:00Z",
    ),
    (
        "mls",
        "LA Galaxy",
        "New York City FC",
        "2026-11-10T00:30:00Z",
    ),
    (
        "a-league-men",
        "Western United",
        "Central Coast Mariners",
        "2026-11-07T08:35:00Z",
    ),
    (
        "a-league-men",
        "Melbourne City",
        "Sydney FC",
        "2026-11-08T08:35:00Z",
    ),
    (
        "nba",
        "LA Clippers",
        "Portland Trail Blazers",
        "2026-11-12T03:00:00Z",
    ),
    (
        "nba",
        "Golden State Warriors",
        "Philadelphia 76ers",
        "2026-11-12T01:00:00Z",
    ),
    (
        "nrl",
        "St George Illawarra Dragons",
        "Canterbury-Bankstown Bulldogs",
        "2026-09-26T09:00:00Z",
    ),
    (
        "nrl",
        "Wests Tigers",
        "North Queensland Cowboys",
        "2026-09-27T09:00:00Z",
    ),
    (
        "gallagher-premiership",
        "Bath Rugby",
        "Saracens",
        "2026-11-07T15:00:00Z",
    ),
    (
        "top-14",
        "Stade Toulousain",
        "Union Bordeaux-Begles",
        "2026-11-07T19:00:00Z",
    ),
    (
        "super-rugby-pacific",
        "Moana Pasifika",
        "Hurricanes",
        "2026-03-07T06:00:00Z",
    ),
];

fn scene(s: &str) -> String {
    s.replace(" & ", ".and.").replace(' ', ".")
}

/// Plain title styles, the ones the indexers carry: scene (dots), spaced, and forum.
fn titles(league: &catalog::League, home: &str, away: &str, start: &str) -> Vec<String> {
    let alias = league.aliases[0];
    let (y, m, d) = (&start[..4], &start[5..7], &start[8..10]);
    vec![
        format!(
            "{}.{y}.{m}.{d}.{}.vs.{}.1080p.WEB-DL.H264-NX",
            scene(alias),
            scene(home),
            scene(away)
        ),
        format!("{alias} {y} {m} {d} {home} vs {away} 1080p WEB-DL-DARKSPORT"),
        format!("{alias} {y} {m} {d} {home} Vs {away} 720p HDTV H264-NX"),
        format!("{alias} {home} vs {away} {y} {m} {d} 1080p WEB-DL-DARKSPORT"),
    ]
}

#[test]
fn real_titles_with_awkward_names_match_their_event() {
    let mut refused: Vec<String> = vec![];
    let mut tried = 0;
    for (slug, home, away, start) in GAMES {
        let league = catalog::find(slug).expect("league");
        let mut ev = event("event:nba:60");
        ev.id = format!("event:{slug}:7001");
        ev.league = (*slug).into();
        ev.name = format!("{home} v {away}");
        ev.starts_at = (*start).into();
        ev.ends_at = None;
        ev.home_id = Some("1".into());
        ev.home_name = Some((*home).into());
        ev.away_id = Some("2".into());
        ev.away_name = Some((*away).into());
        ev.season = Some("2026-2027".into());
        ev.round = None;
        let teams = vec![
            team(slug, "1", home),
            team(slug, "2", away),
            team(slug, "3", "Zeta United"),
        ];
        let published = at(start) + time::Duration::hours(10);
        // With the league's list whole and with it not (then only exact names resolve).
        for complete in [true, false] {
            let cx = Context::new(
                league,
                &ev,
                &teams,
                &[],
                false,
                complete,
                known_spans(true),
                true,
            );
            for title in titles(league, home, away, start) {
                tried += 1;
                match judge(&release::parse(&title, Some(published)), &ev, &cx) {
                    Verdict::Match(_) => {}
                    Verdict::Refused(r) => refused.push(format!(
                        "{title} (list whole: {complete}): {:?} {}",
                        r.code, r.detail
                    )),
                }
            }
        }
    }
    assert!(tried >= 160, "{tried}");
    assert!(
        refused.is_empty(),
        "{} refused:\n{}",
        refused.len(),
        refused.join("\n")
    );
}

/// (stored card name, start, title) of real fight cards: Junior, Jr, II, a three-word surname, an
/// apostrophe, a hyphen, an accent, a numeral on either side.
const CARDS: &[(&str, &str, &str)] = &[
    (
        "UFC 166: Cain Velasquez vs. Junior dos Santos 3",
        "2013-10-20T02:00:00Z",
        "UFC 166 Cain Velasquez vs Junior dos Santos 3 Main Card",
    ),
    (
        "UFC 227: Dillashaw vs. Garbrandt 2",
        "2018-08-05T01:00:00Z",
        "UFC 227 Dillashaw 2 vs Garbrandt Main Card",
    ),
    (
        "UFC 227: Dillashaw vs. Garbrandt 2",
        "2018-08-05T01:00:00Z",
        "UFC 227 Dillashaw vs Garbrandt 2 Main Card",
    ),
    (
        "UFC 306: Sean O'Malley vs. Merab Dvalishvili",
        "2024-09-15T03:00:00Z",
        "UFC 306 Sean O'Malley vs Merab Dvalishvili Main Card",
    ),
    (
        "UFC 306: Sean O'Malley vs. Merab Dvalishvili",
        "2024-09-15T03:00:00Z",
        "UFC.306.OMalley.vs.Dvalishvili.Main.Card",
    ),
    (
        "UFC 295: Prochazka vs. Pereira",
        "2023-11-12T03:00:00Z",
        "UFC 295 Prochazka vs Pereira Main Card",
    ),
    (
        "UFC 292: Aljamain Sterling vs. Sean O'Malley",
        "2023-08-20T03:00:00Z",
        "UFC 292 Sterling vs O'Malley Main Card",
    ),
    (
        "UFC 252: Miocic vs. Cormier 3",
        "2020-08-16T03:00:00Z",
        "UFC 252 Miocic vs Cormier 3 Main Card",
    ),
    (
        "UFC 252: Miocic vs. Cormier 3",
        "2020-08-16T03:00:00Z",
        "UFC 252 Miocic vs Cormier III Main Card",
    ),
    (
        "UFC 310: Alexandre Pantoja vs. Kai Asakura",
        "2024-12-08T03:00:00Z",
        "UFC 310 Pantoja vs Asakura Main Card",
    ),
    (
        "UFC 285: Jones vs. Gane",
        "2023-03-05T03:00:00Z",
        "UFC 285 Jon Jones vs Ciryl Gane Main Card",
    ),
    (
        "UFC Fight Night: Cortes Acosta vs. Walker",
        "2026-11-07T22:00:00Z",
        "UFC Fight Night Cortes-Acosta vs Walker Main Card",
    ),
    (
        "UFC Fight Night: Smith Jr. vs. Jones",
        "2026-11-07T22:00:00Z",
        "UFC Fight Night Smith Jr vs Jones Main Card",
    ),
    (
        "UFC 300: Hill vs. Pereira",
        "2024-04-14T03:00:00Z",
        "UFC 300 Hill vs Pereira Main Card",
    ),
    (
        "UFC 187: Johnson vs. Cormier",
        "2015-05-24T03:00:00Z",
        "UFC 187 Johnson vs Cormier Main Card",
    ),
    (
        "PFL 7: Rafael dos Anjos vs. Rafael Fiziev",
        "2026-07-18T21:00:00Z",
        "PFL 7 Rafael dos Anjos vs Rafael Fiziev Full Event",
    ),
];

#[test]
fn real_fight_cards_with_awkward_names_match_their_event() {
    let mut refused: Vec<String> = vec![];
    for (name, start, base) in CARDS {
        let slug = if name.starts_with("PFL") {
            "pfl"
        } else {
            "ufc"
        };
        let league = catalog::find(slug).unwrap();
        let mut ev = event("event:ufc:3");
        ev.id = format!("event:{slug}:7002");
        ev.league = slug.into();
        ev.name = (*name).into();
        ev.starts_at = (*start).into();
        ev.ends_at = None;
        let cx = Context::new(league, &ev, &[], &[], false, true, known_spans(true), true);
        let published = at(start) + time::Duration::hours(12);
        for tag in ["-DARKSPORT", ""] {
            let title = format!("{base} 1080p WEB-DL{tag}");
            match judge(&release::parse(&title, Some(published)), &ev, &cx) {
                Verdict::Match(_) => {}
                Verdict::Refused(r) => refused.push(format!("{title}: {:?} {}", r.code, r.detail)),
            }
        }
    }
    assert!(
        refused.is_empty(),
        "{} refused:\n{}",
        refused.len(),
        refused.join("\n")
    );
}

/// (stored tournament, start, end, title words) of real golf events with awkward names.
const TOURNAMENTS: &[(&str, &str, &str, &str)] = &[
    (
        "The Masters",
        "2026-04-09T12:00:00Z",
        "2026-04-12T21:00:00Z",
        "PGA Tour The Masters",
    ),
    (
        "U.S. Open",
        "2026-06-18T12:00:00Z",
        "2026-06-21T21:00:00Z",
        "PGA Tour US Open",
    ),
    (
        "The Open Championship",
        "2026-07-16T06:00:00Z",
        "2026-07-19T21:00:00Z",
        "PGA Tour The Open",
    ),
    (
        "PGA Championship",
        "2026-05-14T12:00:00Z",
        "2026-05-17T21:00:00Z",
        "PGA Tour PGA Championship",
    ),
    (
        "AT&T Pebble Beach Pro-Am",
        "2026-02-05T12:00:00Z",
        "2026-02-08T21:00:00Z",
        "PGA Tour AT&T Pebble Beach Pro-Am",
    ),
    (
        "Arnold Palmer Invitational presented by Mastercard",
        "2026-03-05T12:00:00Z",
        "2026-03-08T21:00:00Z",
        "PGA Tour Arnold Palmer Invitational",
    ),
    (
        "THE PLAYERS Championship",
        "2026-03-12T12:00:00Z",
        "2026-03-15T21:00:00Z",
        "PGA Tour The Players Championship",
    ),
    (
        "Valero Texas Open",
        "2026-04-02T12:00:00Z",
        "2026-04-05T21:00:00Z",
        "PGA Tour Valero Texas Open",
    ),
];

#[test]
fn real_tournaments_match_their_event() {
    let mut refused: Vec<String> = vec![];
    for (name, start, end, base) in TOURNAMENTS {
        let league = catalog::find("pga-tour").unwrap();
        let mut ev = event("event:pga-tour:81");
        ev.id = "event:pga-tour:7003".into();
        ev.name = (*name).into();
        ev.starts_at = (*start).into();
        ev.ends_at = Some((*end).into());
        let cx = Context::new(league, &ev, &[], &[], false, true, known_spans(true), true);
        let day = at(start).date() + time::Duration::days(2);
        let published = at(end) + time::Duration::hours(6);
        let title = format!(
            "{base} Round 3 {:04} {:02} {:02} 1080p WEB-DL-DARKSPORT",
            day.year(),
            u8::from(day.month()),
            day.day()
        );
        match judge(&release::parse(&title, Some(published)), &ev, &cx) {
            Verdict::Match(_) => {}
            Verdict::Refused(r) => refused.push(format!("{title}: {:?} {}", r.code, r.detail)),
        }
    }
    assert!(
        refused.is_empty(),
        "{} refused:\n{}",
        refused.len(),
        refused.join("\n")
    );
}

/// LPGA events whose names hold another tour's tournament name or abbreviations.
const LPGA: &[(&str, &str, &str, &str)] = &[
    (
        "KPMG Women's PGA Championship",
        "2026-06-25T12:00:00Z",
        "2026-06-28T21:00:00Z",
        "LPGA Tour KPMG Women's PGA Championship",
    ),
    (
        "U.S. Women's Open",
        "2026-06-04T12:00:00Z",
        "2026-06-07T21:00:00Z",
        "LPGA Tour US Women's Open",
    ),
    (
        "AIG Women's Open",
        "2026-07-30T12:00:00Z",
        "2026-08-02T21:00:00Z",
        "LPGA Tour AIG Women's Open",
    ),
    (
        "The Chevron Championship",
        "2026-04-23T12:00:00Z",
        "2026-04-26T21:00:00Z",
        "LPGA Tour The Chevron Championship",
    ),
    (
        "The Amundi Evian Championship",
        "2026-07-09T12:00:00Z",
        "2026-07-12T21:00:00Z",
        "LPGA Tour The Amundi Evian Championship",
    ),
];

#[test]
fn real_lpga_tournaments_match_their_event() {
    let mut refused: Vec<String> = vec![];
    for (name, start, end, base) in LPGA {
        let league = catalog::find("lpga-tour").unwrap();
        let mut ev = event("event:lpga-tour:80");
        ev.id = "event:lpga-tour:7004".into();
        ev.name = (*name).into();
        ev.starts_at = (*start).into();
        ev.ends_at = Some((*end).into());
        let cx = Context::new(league, &ev, &[], &[], false, true, known_spans(true), true);
        let day = at(start).date() + time::Duration::days(2);
        let published = at(end) + time::Duration::hours(6);
        let title = format!(
            "{base} Round 3 {:04} {:02} {:02} 1080p WEB-DL-NX",
            day.year(),
            u8::from(day.month()),
            day.day()
        );
        match judge(&release::parse(&title, Some(published)), &ev, &cx) {
            Verdict::Match(_) => {}
            Verdict::Refused(r) => refused.push(format!("{title}: {:?} {}", r.code, r.detail)),
        }
    }
    assert!(
        refused.is_empty(),
        "{} refused:\n{}",
        refused.len(),
        refused.join("\n")
    );
    // The same names for soccer are another competition: the Women's Champions League is not the
    // Champions League.
    let ev = event("event:champions-league:40");
    let v = judge_with(
        "UEFA Women's Champions League / Final / 30.05.2026 / Paris Saint-Germain @ Arsenal London [Soccer, WEB-DL HD/1080p, EN/CBS] [NX]",
        None,
        &ev,
        false,
    );
    assert!(v.matched().is_none(), "{v:?}");
}

#[test]
fn c32_c40_qualifiers_inside_a_fighters_name_and_several_suffixes() {
    let ev = stored(
        "event:ufc:5010",
        "UFC Fight Night: dos Santos vs. Ivanov",
        "2026-11-07T22:00:00Z",
    );
    let publ = Some("2026-11-08T10:00:00Z");
    let v = judge_with(
        "UFC Fight Night Junior dos Santos vs Blagoy Ivanov Main Card 1080p WEB-DL-NX",
        publ,
        &ev,
        false,
    );
    assert!(v.matched().is_some(), "{v:?}");
    // A qualifier outside any fighter's run still refuses.
    for t in [
        "UFC Junior Fight Night dos Santos vs Ivanov Main Card 1080p WEB-DL-NX",
        "UFC Fight Night dos Santos vs Ivanov Reserves Main Card 1080p WEB-DL-NX",
        "UFC U21 Fight Night dos Santos vs Ivanov Main Card 1080p WEB-DL-NX",
        "UFC Fight Night dos Santos vs Ivanov Academy Main Card 1080p WEB-DL-NX",
    ] {
        let v = judge_with(t, publ, &ev, false);
        assert!(v.matched().is_none(), "{t}: {v:?}");
    }
    // C40: several suffixes on one name are compared as written.
    let jr = stored(
        "event:ufc:5011",
        "UFC 333: Volkanovski Jr vs. Evloev",
        "2026-10-24T14:00:00Z",
    );
    let v = judge_with(
        "UFC 333 Volkanovski Sr Jr vs Evloev Main Card 1080p WEB-DL-NX",
        Some("2026-10-25T01:00:00Z"),
        &jr,
        false,
    );
    assert_eq!(v.refusal(), Some(RefusalCode::FightersMismatch), "{v:?}");
}

#[test]
fn abbreviations_compare_equal_in_names() {
    use super::names::tokens;
    for (a, b) in [
        ("U.S. Open", "US Open"),
        ("D.C. United", "DC United"),
        ("F.C. Koln", "FC Koln"),
        ("A.F.C. Bournemouth", "AFC Bournemouth"),
        ("L.A. Galaxy", "LA Galaxy"),
    ] {
        assert_eq!(tokens(a), tokens(b), "{a} / {b}");
    }
    assert_ne!(tokens("St. Louis City SC"), tokens("Saint Louis City SC"));
    assert_eq!(tokens("St Louis"), tokens("St. Louis"));
}

// ---------------------------------------------------------------------------------- round 8

/// Judges `title` as a copy of the game `home` v `away` of `slug`, with the league's list whole
/// and not.
fn game_verdicts(slug: &str, home: &str, away: &str, start: &str, title: &str) -> Vec<Verdict> {
    let league = catalog::find(slug).expect("league");
    let mut ev = event("event:nba:60");
    ev.id = format!("event:{slug}:7002");
    ev.league = slug.into();
    ev.name = format!("{home} v {away}");
    ev.starts_at = start.into();
    ev.ends_at = None;
    ev.home_id = Some("1".into());
    ev.home_name = Some(home.into());
    ev.away_id = Some("2".into());
    ev.away_name = Some(away.into());
    ev.season = Some("2026-2027".into());
    ev.round = None;
    let teams = vec![team(slug, "1", home), team(slug, "2", away)];
    let published = at(start) + time::Duration::hours(10);
    [true, false]
        .into_iter()
        .map(|complete| {
            let cx = Context::new(
                league,
                &ev,
                &teams,
                &[],
                false,
                complete,
                known_spans(true),
                true,
            );
            judge(&release::parse(title, Some(published)), &ev, &cx)
        })
        .collect()
}

#[test]
fn c42_the_top_division_is_accepted_and_the_second_tier_refused() {
    let day = "2026 11 07";
    let start = "2026-11-07T15:30:00Z";
    let accept: &[(&str, &str, &str, &str)] = &[
        (
            "bundesliga",
            "1. Bundesliga",
            "Bayern Munich",
            "Borussia Dortmund",
        ),
        (
            "bundesliga",
            "1 Bundesliga",
            "Bayern Munich",
            "Borussia Dortmund",
        ),
        (
            "bundesliga",
            "Bundesliga",
            "Bayern Munich",
            "Borussia Dortmund",
        ),
        (
            "ligue-1",
            "Ligue 1",
            "Paris Saint-Germain",
            "Olympique Marseille",
        ),
        (
            "ligue-1",
            "Ligue1",
            "Paris Saint-Germain",
            "Olympique Marseille",
        ),
        ("serie-a", "Serie A", "Inter Milan", "AC Milan"),
        ("la-liga", "La Liga", "Real Madrid", "FC Barcelona"),
        ("la-liga", "LaLiga", "Real Madrid", "FC Barcelona"),
        ("la-liga", "LaLiga EA Sports", "Real Madrid", "FC Barcelona"),
        ("la-liga", "Primera Division", "Real Madrid", "FC Barcelona"),
        ("premier-league", "Premier League", "Arsenal", "Chelsea"),
        (
            "a-league-men",
            "A-League Men",
            "Sydney FC",
            "Melbourne Victory",
        ),
    ];
    let refuse: &[(&str, &str, &str, &str)] = &[
        (
            "bundesliga",
            "2. Bundesliga",
            "Bayern Munich",
            "Borussia Dortmund",
        ),
        (
            "bundesliga",
            "2 Bundesliga",
            "Bayern Munich",
            "Borussia Dortmund",
        ),
        (
            "bundesliga",
            "3. Liga Bundesliga",
            "Bayern Munich",
            "Borussia Dortmund",
        ),
        (
            "bundesliga",
            "Bundesliga 2",
            "Bayern Munich",
            "Borussia Dortmund",
        ),
        (
            "ligue-1",
            "Ligue 2",
            "Paris Saint-Germain",
            "Olympique Marseille",
        ),
        ("premier-league", "Premier League 2", "Arsenal", "Chelsea"),
        ("la-liga", "La Liga 2", "Real Madrid", "FC Barcelona"),
        (
            "la-liga",
            "LaLiga Hypermotion La Liga",
            "Real Madrid",
            "FC Barcelona",
        ),
        ("serie-a", "Serie B Serie A", "Inter Milan", "AC Milan"),
        (
            "a-league-men",
            "A-League Women",
            "Sydney FC",
            "Melbourne Victory",
        ),
    ];
    for (slug, name, h, a) in accept {
        let title = format!("{name} {day} {h} vs {a} 1080p WEB-DL-NX");
        for v in game_verdicts(slug, h, a, start, &title) {
            assert!(v.matched().is_some(), "{title}: {v:?}");
        }
    }
    for (slug, name, h, a) in refuse {
        let title = format!("{name} {day} {h} vs {a} 1080p WEB-DL-NX");
        for v in game_verdicts(slug, h, a, start, &title) {
            assert!(v.matched().is_none(), "{title}: {v:?}");
        }
    }
    // 3. Liga on its own names no catalogue league, and "3. Liga" before Bundesliga's name is a
    // qualifier.
    let v = judge_with(
        "3. Liga 2026 11 07 Bayern Munich vs Borussia Dortmund 1080p WEB-DL-NX",
        None,
        &event("event:bundesliga:90"),
        false,
    );
    assert!(v.matched().is_none(), "{v:?}");
}

#[test]
fn c37_initials_are_one_word_however_they_are_written() {
    use super::names::tokens;
    for (a, b, c) in [
        ("U.S. Open", "US Open", "U S Open"),
        ("D.C. United", "DC United", "D C United"),
        ("L.A. Galaxy", "LA Galaxy", "L A Galaxy"),
        ("A.F.C. Wimbledon", "AFC Wimbledon", "A F C Wimbledon"),
    ] {
        assert_eq!(tokens(a), tokens(b), "{a}/{b}");
        assert_eq!(tokens(a), tokens(c), "{a}/{c}");
        let (pa, pb, pc) = (
            release::parse(&format!("{a} 2026 11 07 x"), None),
            release::parse(&format!("{b} 2026 11 07 x"), None),
            release::parse(&format!("{c} 2026 11 07 x"), None),
        );
        assert_eq!(
            super::names::fold_initials(pa.body.clone()),
            super::names::fold_initials(pb.body.clone()),
            "{a}/{b}"
        );
        assert_eq!(
            super::names::fold_initials(pa.body.clone()),
            super::names::fold_initials(pc.body.clone()),
            "{a}/{c}"
        );
    }
    assert_eq!(tokens("St. Louis City"), tokens("St Louis City"));
    // Tournaments, each way of writing the initials, against the stored dotted name.
    let ev = {
        let mut e = stored(
            "event:lpga-tour:7005",
            "U.S. Women's Open",
            "2026-06-04T12:00:00Z",
        );
        e.ends_at = Some("2026-06-07T21:00:00Z".into());
        e
    };
    for name in ["US", "U S", "U.S.", "u.s", "U. S."] {
        let title = format!("LPGA Tour {name} Women's Open Round 3 2026 06 06 1080p WEB-DL-NX");
        let v = judge_with(&title, Some("2026-06-08T01:00:00Z"), &ev, true);
        assert!(v.matched().is_some(), "{title}: {v:?}");
    }
    // Clubs.
    for (slug, home, away, title_home) in [
        ("mls", "D.C. United", "Atlanta United", "D C United"),
        ("mls", "D.C. United", "Atlanta United", "DC United"),
        ("mls", "DC United", "Atlanta United", "D.C. United"),
        ("mls", "LA Galaxy", "Atlanta United", "L.A. Galaxy"),
        ("mls", "L.A. Galaxy", "Atlanta United", "L A Galaxy"),
        (
            "mls",
            "St. Louis City SC",
            "Atlanta United",
            "St Louis City SC",
        ),
        (
            "mls",
            "St Louis City SC",
            "Atlanta United",
            "St. Louis City SC",
        ),
    ] {
        let title = format!("MLS 2026 11 07 {title_home} vs {away} 1080p WEB-DL-NX");
        for v in game_verdicts(slug, home, away, "2026-11-07T23:30:00Z", &title) {
            assert!(v.matched().is_some(), "{title} ({home}): {v:?}");
        }
    }
}

#[test]
fn c43_an_extra_explicit_tour_claim_is_still_a_second_league() {
    let mut ev = stored(
        "event:lpga-tour:7004",
        "KPMG Women's PGA Championship",
        "2026-06-25T12:00:00Z",
    );
    ev.ends_at = Some("2026-06-28T21:00:00Z".into());
    let publ = Some("2026-06-29T01:00:00Z");
    let v = judge_with(
        "LPGA Tour KPMG Women's PGA Championship Round 3 2026 06 27 1080p WEB-DL-NX",
        publ,
        &ev,
        true,
    );
    assert!(v.matched().is_some(), "{v:?}");
    let v = judge_with(
        "LPGA Tour PGA Tour KPMG Women's PGA Championship Round 3 2026 06 27 1080p WEB-DL-NX",
        publ,
        &ev,
        true,
    );
    assert_eq!(v.refusal(), Some(RefusalCode::AmbiguousLeague), "{v:?}");
}

// ---------------------------------------------------------------------------------- round 9

#[test]
fn c44_initials_fold_inside_a_name_never_across_a_league_or_a_separator() {
    let start = "2026-11-07T15:30:00Z";
    let day = "2026 11 07";
    // (slug, stored home, stored away, titles that must match)
    let accept: Vec<(&str, &str, &str, Vec<String>)> = vec![
        (
            "serie-a",
            "AC Milan",
            "Internazionale",
            vec![
                format!("Serie A A.C. Milan vs Internazionale {day} 1080p WEB-DL-NX"),
                format!("Serie A AC Milan vs Internazionale {day} 1080p WEB-DL-NX"),
                format!("Serie A A C Milan vs Internazionale {day} 1080p WEB-DL-NX"),
            ],
        ),
        (
            "serie-a",
            "AC Milan",
            "AS Roma",
            vec![
                format!("Serie A AC Milan v AS Roma {day} 1080p WEB-DL-NX"),
                format!("Serie A A.C. Milan v A.S. Roma {day} 1080p WEB-DL-NX"),
            ],
        ),
        ("mls", "D.C. United", "LA Galaxy", {
            let mut v = vec![];
            for home in ["D.C. United", "DC United", "D C United"] {
                for sep in ["v", "vs", "at", "@", "-"] {
                    for away in ["LA Galaxy", "L.A. Galaxy", "L A Galaxy"] {
                        v.push(format!("MLS {day} {home} {sep} {away} 1080p WEB-DL-NX"));
                    }
                }
            }
            v
        }),
        (
            "a-league-men",
            "Sydney FC",
            "Melbourne Victory",
            vec![format!(
                "A-League Men {day} Sydney FC v Melbourne Victory 1080p WEB-DL-NX"
            )],
        ),
    ];
    let mut shown = vec![];
    for (slug, home, away, titles) in &accept {
        for title in titles {
            for v in game_verdicts(slug, home, away, start, title) {
                if v.matched().is_none() {
                    shown.push(format!("{title}: {v:?}"));
                }
            }
        }
    }
    assert!(
        shown.is_empty(),
        "{} refused:\n{}",
        shown.len(),
        shown.join("\n")
    );
    // A second tier is still not the top division.
    for title in [
        format!("Serie B AC Milan v AS Roma {day} 1080p WEB-DL-NX"),
        format!("Serie B A.C. Milan v AS Roma {day} 1080p WEB-DL-NX"),
    ] {
        for v in game_verdicts("serie-a", "AC Milan", "AS Roma", start, &title) {
            assert!(v.matched().is_none(), "{title}: {v:?}");
        }
    }
}

#[test]
fn c44_fighters_and_tournaments_with_initials() {
    let ev = stored(
        "event:ufc:5030",
        "UFC 300: B.J. Penn vs. Diego Sanchez",
        "2026-11-07T22:00:00Z",
    );
    for name in ["BJ Penn", "B.J. Penn", "B J Penn", "Penn"] {
        let title = format!("UFC 300 {name} vs Diego Sanchez Main Card 1080p WEB-DL-NX");
        let v = judge_with(&title, Some("2026-11-08T10:00:00Z"), &ev, false);
        assert!(v.matched().is_some(), "{title}: {v:?}");
    }
    let mut pga = stored("event:pga-tour:7010", "U.S. Open", "2026-06-18T12:00:00Z");
    pga.ends_at = Some("2026-06-21T21:00:00Z".into());
    for name in ["U.S. Open", "US Open", "U S Open"] {
        let title = format!("PGA Tour {name} Round 3 2026 06 20 1080p WEB-DL-NX");
        let v = judge_with(&title, Some("2026-06-22T01:00:00Z"), &pga, true);
        assert!(v.matched().is_some(), "{title}: {v:?}");
    }
}

#[test]
fn c45_only_the_occurrence_inside_the_tournaments_name_is_exempt() {
    let mut ev = stored(
        "event:lpga-tour:7004",
        "KPMG Women's PGA Championship",
        "2026-06-25T12:00:00Z",
    );
    ev.ends_at = Some("2026-06-28T21:00:00Z".into());
    let publ = Some("2026-06-29T01:00:00Z");
    for title in [
        "LPGA Tour PGA Championship KPMG Women's PGA Championship Round 3 2026 06 27 1080p WEB-DL-NX",
        "LPGA Tour PGA Tour KPMG Women's PGA Championship Round 3 2026 06 27 1080p WEB-DL-NX",
    ] {
        let v = judge_with(title, publ, &ev, true);
        assert_eq!(
            v.refusal(),
            Some(RefusalCode::AmbiguousLeague),
            "{title}: {v:?}"
        );
    }
    let title = "LPGA Tour KPMG Women's PGA Championship Round 3 2026 06 27 1080p WEB-DL-NX";
    let v = judge_with(title, publ, &ev, true);
    assert!(v.matched().is_some(), "{title}: {v:?}");
}

// --------------------------------------------------------------------------------- round 10

/// The ways a title can write `name`: scene-dotted, spaced, with underscores.
fn styles(name: &str) -> Vec<String> {
    vec![
        name.to_string(),
        name.replace(' ', "."),
        name.replace(' ', "_"),
    ]
}

#[test]
fn c44_c46_initials_are_folded_at_comparison_and_the_split_is_chosen_by_resolution() {
    let start = "2026-11-07T15:30:00Z";
    let day = "2026 11 07";
    // (slug, stored home, stored away, league as written, home as written, away as written)
    let rows: Vec<(&str, &str, &str, &str, &str, &str)> = vec![
        (
            "serie-a",
            "AC Milan",
            "Internazionale",
            "Serie A",
            "A.C. Milan",
            "Internazionale",
        ),
        (
            "serie-a",
            "AC Milan",
            "Internazionale",
            "Serie A",
            "AC Milan",
            "Internazionale",
        ),
        (
            "serie-a",
            "AC Milan",
            "Internazionale",
            "Serie A",
            "A C Milan",
            "Internazionale",
        ),
        (
            "serie-a", "AC Milan", "AS Roma", "Serie A", "AC Milan", "AS Roma",
        ),
        (
            "serie-a",
            "AC Milan",
            "AS Roma",
            "Serie A",
            "A.C. Milan",
            "A.S. Roma",
        ),
        (
            "serie-a", "Juventus", "Napoli", "Serie A", "Juventus", "Napoli",
        ),
        (
            "champions-league",
            "PSV Eindhoven",
            "Liverpool",
            "UEFA Champions League",
            "P.S.V. Eindhoven",
            "Liverpool",
        ),
        (
            "champions-league",
            "PSV Eindhoven",
            "Liverpool",
            "UEFA Champions League",
            "PSV Eindhoven",
            "Liverpool",
        ),
        (
            "champions-league",
            "PSV Eindhoven",
            "Liverpool",
            "UEFA Champions League",
            "P S V Eindhoven",
            "Liverpool",
        ),
        (
            "eredivisie",
            "AZ Alkmaar",
            "Ajax",
            "Eredivisie",
            "A.Z. Alkmaar",
            "Ajax",
        ),
        (
            "bundesliga",
            "RB Leipzig",
            "1. FC Koln",
            "Bundesliga",
            "R.B. Leipzig",
            "1. F.C. Koln",
        ),
        (
            "bundesliga",
            "RB Leipzig",
            "FC Koln",
            "Bundesliga",
            "RB Leipzig",
            "F.C. Koln",
        ),
        (
            "mls",
            "D.C. United",
            "LA Galaxy",
            "MLS",
            "D.C. United",
            "L.A. Galaxy",
        ),
        (
            "mls",
            "D.C. United",
            "LA Galaxy",
            "MLS",
            "D C United",
            "L A Galaxy",
        ),
        (
            "mls",
            "D.C. United",
            "LA Galaxy",
            "MLS",
            "DC United",
            "LA Galaxy",
        ),
        (
            "mls",
            "St. Louis City SC",
            "Atlanta United",
            "MLS",
            "St. Louis City SC",
            "Atlanta United",
        ),
        (
            "mls",
            "St. Louis City SC",
            "Atlanta United",
            "MLS",
            "St Louis City SC",
            "Atlanta United",
        ),
        (
            "bundesliga",
            "FC St. Pauli",
            "Mainz 05",
            "Bundesliga",
            "FC St Pauli",
            "Mainz 05",
        ),
        (
            "primeira-liga",
            "C.D. Leganes",
            "Real Madrid",
            "Primeira Liga",
            "C D Leganes",
            "Real Madrid",
        ),
        (
            "primeira-liga",
            "S.L. Benfica",
            "FC Porto",
            "Primeira Liga",
            "S L Benfica",
            "F C Porto",
        ),
        (
            "primeira-liga",
            "S.L. Benfica",
            "F.C. Porto",
            "Primeira Liga",
            "SL Benfica",
            "FC Porto",
        ),
    ];
    let mut shown = vec![];
    for (slug, h, a, league, hw, aw) in &rows {
        if catalog::find(slug).is_none() {
            continue; // (a league the catalogue does not have)
        }
        for sep in ["v", "vs", "at", "@", "-"] {
            for hs in styles(hw) {
                for as_ in styles(aw) {
                    for title in [
                        format!("{league} {day} {hs} {sep} {as_} 1080p WEB-DL-NX"),
                        format!("{league} {hs} {sep} {as_} {day} 1080p WEB-DL-NX"),
                    ] {
                        // (a scene title has dots between every word)
                        // (a hyphen between dots is no separator in a scene title)
                        let scene = title.replace(' ', ".");
                        let all = if sep == "-" {
                            vec![title]
                        } else {
                            vec![title, scene]
                        };
                        for t in all {
                            for v in game_verdicts(slug, h, a, start, &t) {
                                if v.matched().is_none() {
                                    shown.push(format!("{t}: {v:?}"));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(
        shown.is_empty(),
        "{} refused, e.g.\n{}",
        shown.len(),
        shown
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    // The quoted scene titles.
    for t in [
        "Serie.A.A.C.Milan.vs.Internazionale.2026.11.07.1080p.WEB-DL-NX",
        "UEFA.Champions.League.P.S.V.Eindhoven.vs.Liverpool.2026.11.07.1080p.WEB-DL-NX",
    ] {
        let (slug, h, a) = if t.starts_with("Serie") {
            ("serie-a", "AC Milan", "Internazionale")
        } else {
            ("champions-league", "PSV Eindhoven", "Liverpool")
        };
        for v in game_verdicts(slug, h, a, start, t) {
            assert!(v.matched().is_some(), "{t}: {v:?}");
        }
    }
    // The second tier is still not the top division.
    for t in [
        "Serie B AC Milan v AS Roma 2026 11 07 1080p WEB-DL-NX",
        "Serie.B.A.C.Milan.v.A.S.Roma.2026.11.07.1080p.WEB-DL-NX",
    ] {
        for v in game_verdicts("serie-a", "AC Milan", "AS Roma", start, t) {
            assert!(v.matched().is_none(), "{t}: {v:?}");
        }
    }
    // Two different teams are still two different teams.
    for v in game_verdicts(
        "serie-a",
        "AC Milan",
        "Internazionale",
        start,
        "Serie A AC Milan v Juventus 2026 11 07 1080p WEB-DL-NX",
    ) {
        assert!(v.matched().is_none(), "{v:?}");
    }
}

#[test]
fn c46_fighters_with_initials_and_a_surname_that_is_a_v_word() {
    let publ = Some("2026-11-08T10:00:00Z");
    let ev = stored(
        "event:ufc:5040",
        "UFC 301: B.J. Penn vs. C.B. Dollaway",
        "2026-11-07T22:00:00Z",
    );
    for a in styles("BJ Penn")
        .into_iter()
        .chain(styles("B.J. Penn"))
        .chain(styles("B J Penn"))
    {
        for b in styles("CB Dollaway")
            .into_iter()
            .chain(styles("C.B. Dollaway"))
            .chain(styles("C B Dollaway"))
        {
            let t = format!("UFC 301 {a} vs {b} Main Card 1080p WEB-DL-NX");
            let v = judge_with(&t, publ, &ev, false);
            assert!(v.matched().is_some(), "{t}: {v:?}");
        }
    }
    let ev = stored(
        "event:ufc:5041",
        "UFC 302: Vicente Luque vs. Geoff Neal",
        "2026-11-07T22:00:00Z",
    );
    for t in [
        "UFC 302 Vicente Luque vs Geoff Neal Main Card 1080p WEB-DL-NX",
        "UFC.302.Vicente.Luque.vs.Geoff.Neal.Main.Card.1080p.WEB-DL-NX",
        "UFC 302 Luque v Neal Main Card 1080p WEB-DL-NX",
        "UFC 302 Geoff Neal vs Vicente Luque Main Card 1080p WEB-DL-NX",
    ] {
        let v = judge_with(t, publ, &ev, false);
        assert!(v.matched().is_some(), "{t}: {v:?}");
    }
    // A different pair is still refused.
    let v = judge_with(
        "UFC 302 Vicente Luque vs Gilbert Burns Main Card 1080p WEB-DL-NX",
        publ,
        &ev,
        false,
    );
    assert!(v.matched().is_none(), "{v:?}");
}

// --------------------------------------------------------------------------------- round 11

/// How a club is written in a title: as stored, scene-dotted initials, spaced initials, joined.
fn initials_forms(stored: &str, dotted: &str, spaced: &str, joined: &str) -> Vec<(String, String)> {
    let _ = stored;
    vec![
        ("dotted".into(), dotted.into()),
        ("spaced".into(), spaced.into()),
        ("joined".into(), joined.into()),
    ]
}

#[test]
fn c47_no_stripping_step_takes_a_token_from_an_initials_run() {
    let start = "2026-11-07T15:30:00Z";
    let day = "2026 11 07";
    // (slug, league as written, stored club, its written forms, the other club)
    type Forms = Vec<(String, String)>;
    let rows: Vec<(&str, &str, &str, Forms, &str)> = vec![
        (
            "bundesliga",
            "Bundesliga",
            "VfB Stuttgart",
            initials_forms("", "V.F.B. Stuttgart", "V F B Stuttgart", "VfB Stuttgart"),
            "Bayern Munich",
        ),
        (
            "bundesliga",
            "Bundesliga",
            "VfL Wolfsburg",
            initials_forms("", "V.F.L. Wolfsburg", "V F L Wolfsburg", "VfL Wolfsburg"),
            "Bayern Munich",
        ),
        (
            "bundesliga",
            "Bundesliga",
            "VfL Bochum",
            initials_forms("", "V.F.L. Bochum", "V F L Bochum", "VfL Bochum"),
            "Bayern Munich",
        ),
        (
            "premier-league",
            "Premier League",
            "AFC Bournemouth",
            initials_forms(
                "",
                "A.F.C. Bournemouth",
                "A F C Bournemouth",
                "AFC Bournemouth",
            ),
            "Chelsea",
        ),
        (
            "premier-league",
            "Premier League",
            "AFC Wimbledon",
            initials_forms("", "A.F.C. Wimbledon", "A F C Wimbledon", "AFC Wimbledon"),
            "Chelsea",
        ),
        (
            "serie-a",
            "Serie A",
            "AS Roma",
            initials_forms("", "A.S. Roma", "A S Roma", "AS Roma"),
            "Juventus",
        ),
        (
            "serie-a",
            "Serie A",
            "AC Milan",
            initials_forms("", "A.C. Milan", "A C Milan", "AC Milan"),
            "Juventus",
        ),
        (
            "serie-a",
            "Serie A",
            "SS Lazio",
            initials_forms("", "S.S. Lazio", "S S Lazio", "SS Lazio"),
            "Juventus",
        ),
        (
            "serie-a",
            "Serie A",
            "SSC Napoli",
            initials_forms("", "S.S.C. Napoli", "S S C Napoli", "SSC Napoli"),
            "Juventus",
        ),
        (
            "premier-league",
            "Premier League",
            "FC Porto",
            initials_forms("", "F.C. Porto", "F C Porto", "FC Porto"),
            "Chelsea",
        ),
        (
            "premier-league",
            "Premier League",
            "AT Madrid",
            initials_forms("", "A.T. Madrid", "A T Madrid", "AT Madrid"),
            "Chelsea",
        ),
        (
            "premier-league",
            "Premier League",
            "VV Venlo",
            initials_forms("", "V.V. Venlo", "V V Venlo", "VV Venlo"),
            "Chelsea",
        ),
    ];
    let mut shown = vec![];
    for (slug, league, stored, forms, other) in &rows {
        for (style, form) in forms {
            for sep in ["v", "vs", "at", "@"] {
                for club_first in [true, false] {
                    let (l, r) = if club_first {
                        (form.as_str(), *other)
                    } else {
                        (*other, form.as_str())
                    };
                    let (sh, sa) = if club_first {
                        (*stored, *other)
                    } else {
                        (*other, *stored)
                    };
                    for title in [
                        format!("{league} {day} {l} {sep} {r} 1080p WEB-DL-NX"),
                        format!("{league} {l} {sep} {r} {day} 1080p WEB-DL-NX"),
                    ] {
                        for t in [title.clone(), title.replace(' ', ".")] {
                            for v in game_verdicts(slug, sh, sa, start, &t) {
                                if v.matched().is_none() {
                                    shown.push(format!("[{style}] {t}: {v:?}"));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(
        shown.is_empty(),
        "{} refused, e.g.\n{}",
        shown.len(),
        shown
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    // The quoted title, and the club second.
    for t in [
        "Bundesliga.V.F.B.Stuttgart.vs.Bayern.Munich.2026.11.07.1080p.WEB-DL-NX",
        "Bundesliga.Bayern.Munich.vs.V.F.B.Stuttgart.2026.11.07.1080p.WEB-DL-NX",
        "Bundesliga Bayern Munich vs V.F.B. Stuttgart 2026 11 07 1080p WEB-DL-NX",
    ] {
        for v in game_verdicts("bundesliga", "VfB Stuttgart", "Bayern Munich", start, t) {
            assert!(v.matched().is_some(), "{t}: {v:?}");
        }
    }
    // A leading `v` that really is the separator, and nothing else.
    for t in [
        "Serie A Juventus v A.C. Milan 2026 11 07 1080p WEB-DL-NX",
        "Serie.A.Juventus.v.A.C.Milan.2026.11.07.1080p.WEB-DL-NX",
    ] {
        for v in game_verdicts("serie-a", "Juventus", "AC Milan", start, t) {
            assert!(v.matched().is_some(), "{t}: {v:?}");
        }
    }
    // Another club is still another club.
    for v in game_verdicts(
        "bundesliga",
        "VfB Stuttgart",
        "Bayern Munich",
        start,
        "Bundesliga V F L Bochum vs Bayern Munich 2026 11 07 1080p WEB-DL-NX",
    ) {
        assert!(v.matched().is_none(), "{v:?}");
    }
}

#[test]
fn c47_fighters_whose_initials_begin_with_v_or_a() {
    let publ = Some("2026-11-08T10:00:00Z");
    for (stored, home, away) in [
        (
            "UFC 303: V.J. Rock vs. Diego Sanchez",
            "V.J. Rock",
            "Diego Sanchez",
        ),
        (
            "UFC 304: A.J. McKee vs. Patricio Pitbull",
            "A.J. McKee",
            "Patricio Pitbull",
        ),
    ] {
        let ev = stored_event(stored);
        let forms = |n: &str| -> Vec<String> {
            let spaced = n
                .replace('.', " ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            vec![
                n.to_string(),
                spaced.clone(),
                spaced.replace(' ', "."),
                n.replace(". ", "").replace('.', ""),
            ]
        };
        for a in forms(home) {
            for sep in ["vs", "v"] {
                let t = format!("UFC 303 {a} {sep} {away} Main Card 1080p WEB-DL-NX")
                    .replace("UFC 303", &stored[..7]);
                let v = judge_with(&t, publ, &ev, false);
                assert!(v.matched().is_some(), "{t}: {v:?}");
                // and with the fighter second
                let t2 = format!(
                    "{} {away} {sep} {a} Main Card 1080p WEB-DL-NX",
                    &stored[..7]
                );
                let v = judge_with(&t2, publ, &ev, false);
                assert!(v.matched().is_some(), "{t2}: {v:?}");
            }
        }
    }
}

fn stored_event(name: &str) -> EventRow {
    let n = name
        .split(':')
        .next()
        .unwrap_or("UFC 300")
        .replace("UFC ", "");
    stored(
        &format!("event:ufc:{}", 6000 + n.parse::<u32>().unwrap_or(1)),
        name,
        "2026-11-07T22:00:00Z",
    )
}

// --------------------------------------------------------------------------------- round 12

/// As `game_verdicts`, with the stored round.
fn round_verdicts(
    slug: &str,
    home: &str,
    away: &str,
    start: &str,
    round: &str,
    title: &str,
) -> Vec<Verdict> {
    let league = catalog::find(slug).expect("league");
    let mut ev = event("event:nba:60");
    ev.id = format!("event:{slug}:7003");
    ev.league = slug.into();
    ev.name = format!("{home} v {away}");
    ev.starts_at = start.into();
    ev.ends_at = None;
    ev.home_id = Some("1".into());
    ev.home_name = Some(home.into());
    ev.away_id = Some("2".into());
    ev.away_name = Some(away.into());
    ev.season = Some("2026-2027".into());
    ev.round = Some(round.into());
    let teams = vec![team(slug, "1", home), team(slug, "2", away)];
    let published = at(start) + time::Duration::hours(10);
    [true, false]
        .into_iter()
        .map(|complete| {
            let cx = Context::new(
                league,
                &ev,
                &teams,
                &[],
                false,
                complete,
                known_spans(true),
                true,
            );
            judge(&release::parse(title, Some(published)), &ev, &cx)
        })
        .collect()
}

#[test]
fn c48_c49_claims_are_settled_before_initials_are_kept() {
    let start = "2026-11-07T15:30:00Z";
    let styles = |s: &str| vec![s.to_string(), s.replace(' ', "."), s.replace(' ', "_")];
    let mut shown = vec![];
    let mut want = |slug: &str, h: &str, a: &str, round: &str, title: String| {
        for t in styles(&title) {
            for v in round_verdicts(slug, h, a, start, round, &t) {
                if v.matched().is_none() {
                    shown.push(format!("{t}: {v:?}"));
                }
            }
        }
    };
    // The quoted NBA title and the game claim in every writing, before and after the teams.
    for g in ["G 7", "G7", "Game 7", "G.7", "game 7"] {
        want(
            "nba",
            "Oklahoma City Thunder",
            "Indiana Pacers",
            "Game 7",
            format!("NBA 2026 11 07 Indiana Pacers at Oklahoma City Thunder {g} 1080p WEB-DL-NX"),
        );
        want(
            "nba",
            "Oklahoma City Thunder",
            "Indiana Pacers",
            "Game 7",
            format!("NBA 2026 11 07 {g} Indiana Pacers at Oklahoma City Thunder 1080p WEB-DL-NX"),
        );
        want(
            "nba",
            "Oklahoma City Thunder",
            "Indiana Pacers",
            "Game 7",
            format!("NBA {g} Indiana Pacers vs Oklahoma City Thunder 2026 11 07 1080p WEB-DL-NX"),
        );
    }
    // Group claims: the letter belongs to the claim, a club's initials to the club.
    for (club, stored) in [
        ("A.C. Milan", "AC Milan"),
        ("A C Milan", "AC Milan"),
        ("AC Milan", "AC Milan"),
        ("F.C. Porto", "FC Porto"),
        ("F C Porto", "FC Porto"),
        ("FC Porto", "FC Porto"),
        ("Borussia Dortmund", "Borussia Dortmund"),
        ("P.S.V. Eindhoven", "PSV Eindhoven"),
        ("V.F.B. Stuttgart", "VfB Stuttgart"),
    ] {
        for (letter, other) in [
            ("A", "Juventus"),
            ("F", "Juventus"),
            ("B", "Juventus"),
            ("H", "Shakhtar Donetsk"),
        ] {
            let round = format!("Group {letter}");
            // the claim before the left side, club left and right
            want(
                "champions-league",
                stored,
                other,
                &round,
                format!("UCL 2026 11 07 Group {letter} {club} vs {other} 1080p WEB-DL-NX"),
            );
            want(
                "champions-league",
                other,
                stored,
                &round,
                format!("UCL 2026 11 07 Group {letter} {other} vs {club} 1080p WEB-DL-NX"),
            );
            want(
                "champions-league",
                stored,
                other,
                &round,
                format!("UCL 2026 11 07 {club} vs {other} Group {letter} 1080p WEB-DL-NX"),
            );
            want(
                "champions-league",
                other,
                stored,
                &round,
                format!("UCL 2026 11 07 {other} vs {club} Group {letter} 1080p WEB-DL-NX"),
            );
        }
    }
    // Rounds, matchdays and legs before a club that starts with initials.
    for (claim, round) in [
        ("R1", "Round 1"),
        ("R 1", "Round 1"),
        ("Round 1", "Round 1"),
        ("MD 3", "Matchday 3"),
        ("MD3", "Matchday 3"),
        ("Leg 2", "Leg 2"),
    ] {
        for (club, stored) in [
            ("A.S. Roma", "AS Roma"),
            ("P.S.V. Eindhoven", "PSV Eindhoven"),
            ("A.F.C. Bournemouth", "AFC Bournemouth"),
        ] {
            want(
                "premier-league",
                stored,
                "Chelsea",
                round,
                format!("Premier League 2026 11 07 {claim} {club} vs Chelsea 1080p WEB-DL-NX"),
            );
            want(
                "premier-league",
                "Chelsea",
                stored,
                round,
                format!("Premier League 2026 11 07 Chelsea vs {club} {claim} 1080p WEB-DL-NX"),
            );
            want(
                "premier-league",
                "Chelsea",
                stored,
                round,
                format!("Premier League 2026 11 07 {claim} Chelsea vs {club} 1080p WEB-DL-NX"),
            );
        }
    }
    assert!(
        shown.is_empty(),
        "{} refused, e.g.\n{}",
        shown.len(),
        shown
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    // The two quoted titles as written.
    for (slug, h, a, round, t) in [
        (
            "nba",
            "Oklahoma City Thunder",
            "Indiana Pacers",
            "Game 7",
            "NBA.2026.11.07.Indiana.Pacers.at.Oklahoma.City.Thunder.G.7.1080p.WEB-DL-NX",
        ),
        (
            "champions-league",
            "FC Porto",
            "Shakhtar Donetsk",
            "Group H",
            "UCL.2026.11.07.Group.H.F.C.Porto.vs.Shakhtar.Donetsk.1080p.WEB-DL-NX",
        ),
    ] {
        for v in round_verdicts(slug, h, a, start, round, t) {
            assert!(v.matched().is_some(), "{t}: {v:?}");
        }
    }
    // A different round of the same event is still a different round.
    for v in round_verdicts(
        "champions-league",
        "FC Porto",
        "Shakhtar Donetsk",
        start,
        "Group H",
        "UCL.2026.11.07.Group.G.F.C.Porto.vs.Shakhtar.Donetsk.1080p.WEB-DL-NX",
    ) {
        assert!(v.matched().is_none(), "{v:?}");
    }
}

#[test]
fn c48_youth_and_reserve_markers_written_as_letters_and_digits() {
    let start = "2026-11-07T15:30:00Z";
    for t in [
        "Serie A 2026 11 07 Juventus U21 v Napoli 1080p WEB-DL-NX",
        "Serie A 2026 11 07 Juventus U 21 v Napoli 1080p WEB-DL-NX",
        "Serie.A.2026.11.07.Juventus.U21.v.Napoli.1080p.WEB-DL-NX",
        "Serie A 2026 11 07 Juventus Youth v Napoli 1080p WEB-DL-NX",
        "Serie A 2026 11 07 Juventus Reserves v Napoli 1080p WEB-DL-NX",
        "Serie A 2026 11 07 Juventus B v Napoli 1080p WEB-DL-NX",
        "Serie A 2026 11 07 Juventus II v Napoli 1080p WEB-DL-NX",
    ] {
        for v in game_verdicts("serie-a", "Juventus", "Napoli", start, t) {
            // (refused, or at most a match that carries a caution: never a clean one)
            assert!(!sure(&v), "{t}: {v:?}");
        }
    }
}
