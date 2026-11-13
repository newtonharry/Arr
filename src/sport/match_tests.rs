//! The release matcher, table-tested: every real title the sport work has collected, judged
//! against an event of the shape it should be, plus near-misses that must be refused.
//! Everything here is local data; nothing reaches an indexer or the schedule source.

use super::Part;
use super::catalog;
use super::data::{EventRow, TeamRow};
use super::find::{self, Surroundings};
use super::matcher::{Confidence, Context, RefusalCode, Verdict, judge, settle_unlabelled};
use super::release;
use crate::prowlarr::ProwlarrRelease;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub(super) fn at(s: &str) -> OffsetDateTime {
    OffsetDateTime::parse(s, &Rfc3339).expect("test time")
}

pub(super) struct Ev {
    pub(super) id: &'static str,
    pub(super) name: &'static str,
    pub(super) start: &'static str,
    pub(super) end: Option<&'static str>,
    pub(super) home: Option<(&'static str, &'static str)>,
    pub(super) away: Option<(&'static str, &'static str)>,
    pub(super) round: Option<&'static str>,
    pub(super) season: Option<&'static str>,
}

const fn game(
    id: &'static str,
    name: &'static str,
    start: &'static str,
    home: (&'static str, &'static str),
    away: (&'static str, &'static str),
    round: Option<&'static str>,
) -> Ev {
    Ev {
        id,
        name,
        start,
        end: None,
        home: Some(home),
        away: Some(away),
        round,
        // (the feed gives every game its season)
        season: Some("2026-2027"),
    }
}

const fn card(id: &'static str, name: &'static str, start: &'static str) -> Ev {
    Ev {
        id,
        name,
        start,
        end: None,
        home: None,
        away: None,
        round: None,
        season: None,
    }
}

pub(super) const EVENTS: &[Ev] = &[
    // MMA: UFC and PFL cards, named as the schedule source names them.
    card(
        "event:ufc:1",
        "UFC Fight Night: Cortes Acosta vs. Walker",
        "2026-11-07T22:00:00Z",
    ),
    card(
        "event:ufc:2",
        "UFC Fight Night: Ankalaev vs. Walker",
        "2024-01-13T23:00:00Z",
    ),
    card(
        "event:ufc:3",
        "UFC 333: Volkanovski vs. Evloev",
        "2026-10-24T14:00:00Z",
    ),
    card(
        "event:ufc:4",
        "UFC 334: Gane vs. Hokit",
        "2026-11-14T22:00:00Z",
    ),
    card(
        "event:ufc:5",
        "UFC Fight Night: Walker vs. Hill",
        "2022-02-19T23:00:00Z",
    ),
    card(
        "event:ufc:6",
        "UFC event on 2026-12-19",
        "2026-12-19T22:00:00Z",
    ),
    card(
        "event:pfl:10",
        "PFL Tampa: Cyborg vs. Vieira",
        "2026-08-22T21:30:00Z",
    ),
    card(
        "event:pfl:11",
        "PFL Washington DC: Jean vs. Rodriguez",
        "2026-07-26T00:00:00Z",
    ),
    card(
        "event:pfl:12",
        "PFL Brussels: Smith vs. Jones",
        "2026-09-12T18:00:00Z",
    ),
    card(
        "event:pfl:13",
        "PFL Austin: Eblen vs. Kasanganay 2",
        "2026-07-18T21:00:00Z",
    ),
    // Soccer.
    game(
        "event:a-league-men:20",
        "Melbourne City v Western Sydney Wanderers",
        "2026-11-07T08:35:00Z",
        ("1", "Melbourne City"),
        ("2", "Western Sydney Wanderers"),
        None,
    ),
    game(
        "event:a-league-men:21",
        "Melbourne Victory v Melbourne City",
        "2026-10-24T08:00:00Z",
        ("3", "Melbourne Victory"),
        ("1", "Melbourne City"),
        None,
    ),
    game(
        "event:a-league-men:22",
        "Auckland FC v Sydney FC",
        "2026-05-23T09:00:00Z",
        ("4", "Auckland FC"),
        ("5", "Sydney FC"),
        Some("Grand Final"),
    ),
    game(
        "event:premier-league:30",
        "Liverpool v Crystal Palace",
        "2019-01-19T15:00:00Z",
        ("1", "Liverpool"),
        ("2", "Crystal Palace"),
        None,
    ),
    game(
        "event:premier-league:31",
        "Crystal Palace v Liverpool",
        "2018-08-20T19:00:00Z",
        ("2", "Crystal Palace"),
        ("1", "Liverpool"),
        None,
    ),
    game(
        "event:premier-league:32",
        "Chelsea v Manchester United",
        "2026-10-31T17:30:00Z",
        ("3", "Chelsea"),
        ("4", "Manchester United"),
        Some("Matchweek 9"),
    ),
    game(
        "event:premier-league:33",
        "Liverpool v Manchester City",
        "2026-10-11T15:30:00Z",
        ("1", "Liverpool"),
        ("5", "Manchester City"),
        Some("Matchweek 6"),
    ),
    game(
        "event:champions-league:40",
        "Arsenal v Paris Saint-Germain",
        "2026-05-30T19:00:00Z",
        ("2", "Arsenal"),
        ("1", "Paris Saint-Germain"),
        Some("Final"),
    ),
    game(
        "event:champions-league:41",
        "Bayern Munich v Paris Saint-Germain",
        "2026-04-28T19:00:00Z",
        ("3", "Bayern Munich"),
        ("1", "Paris Saint-Germain"),
        Some("Semi-final"),
    ),
    game(
        "event:la-liga:50",
        "Real Madrid v Barcelona",
        "2026-10-25T19:00:00Z",
        ("2", "Real Madrid"),
        ("1", "Barcelona"),
        Some("Matchday 10"),
    ),
    game(
        "event:bundesliga:90",
        "Koln v RB Leipzig",
        "2026-11-09T19:30:00Z",
        ("1", "1. FC Koln"),
        ("2", "RB Leipzig"),
        None,
    ),
    // Basketball: the away side is written first, as the UTC day has already turned.
    game(
        "event:nba:60",
        "Washington Wizards at Indiana Pacers",
        "2026-11-12T00:00:00Z",
        ("2", "Indiana Pacers"),
        ("1", "Washington Wizards"),
        None,
    ),
    game(
        "event:nba:61",
        "San Antonio Spurs at Chicago Bulls",
        "2026-11-12T01:00:00Z",
        ("4", "Chicago Bulls"),
        ("3", "San Antonio Spurs"),
        None,
    ),
    game(
        "event:nba:62",
        "Los Angeles Lakers at Milwaukee Bucks",
        "2026-11-12T01:30:00Z",
        ("6", "Milwaukee Bucks"),
        ("5", "Los Angeles Lakers"),
        None,
    ),
    // The same two teams on the next night (a series), and a return fixture a month later.
    game(
        "event:nba:64",
        "Los Angeles Lakers at Milwaukee Bucks",
        "2026-11-13T01:30:00Z",
        ("6", "Milwaukee Bucks"),
        ("5", "Los Angeles Lakers"),
        None,
    ),
    game(
        "event:premier-league:34",
        "Liverpool v Manchester City",
        "2026-11-10T20:00:00Z",
        ("1", "Liverpool"),
        ("5", "Manchester City"),
        Some("Matchweek 12"),
    ),
    card(
        "event:ufc:7",
        "UFC 332: Silva vs. Wang",
        "2026-10-03T20:00:00Z",
    ),
    // Rugby.
    Ev {
        season: Some("2022"),
        ..game(
            "event:six-nations:70",
            "Scotland v England",
            "2022-02-05T14:15:00Z",
            ("1", "Scotland"),
            ("2", "England"),
            Some("Round 1"),
        )
    },
    game(
        "event:nrl:100",
        "Penrith Panthers v Melbourne Storm",
        "2026-09-26T09:00:00Z",
        ("1", "Penrith Panthers"),
        ("2", "Melbourne Storm"),
        None,
    ),
    // Golf.
    Ev {
        id: "event:lpga-tour:80",
        name: "DOW Invitational",
        start: "2023-07-20T04:00:00Z",
        end: Some("2023-07-23T04:00:00Z"),
        home: None,
        away: None,
        round: None,
        season: None,
    },
    Ev {
        id: "event:pga-tour:81",
        name: "World Wide Technology Championship",
        start: "2026-11-05T05:00:00Z",
        end: Some("2026-11-08T05:00:00Z"),
        home: None,
        away: None,
        round: None,
        season: None,
    },
];

pub(super) fn row(e: &Ev) -> EventRow {
    EventRow {
        id: e.id.into(),
        league: e.id.split(':').nth(1).unwrap().into(),
        season: e.season.map(str::to_string),
        starts_at: e.start.into(),
        ends_at: e.end.map(str::to_string),
        name: e.name.into(),
        short_name: None,
        home_id: e.home.map(|h| h.0.into()),
        home_name: e.home.map(|h| h.1.into()),
        away_id: e.away.map(|h| h.0.into()),
        away_name: e.away.map(|h| h.1.into()),
        round: e.round.map(str::to_string),
        venue: None,
        state: "played".into(),
    }
}

pub(super) fn all_events() -> Vec<EventRow> {
    EVENTS.iter().map(row).collect()
}

pub(super) fn event(id: &str) -> EventRow {
    all_events()
        .into_iter()
        .find(|e| e.id == id)
        .unwrap_or_else(|| panic!("no fixture {id}"))
}

pub(super) fn teams_of(league: &str) -> Vec<TeamRow> {
    let list: &[(&str, &str, Option<&str>)] = match league {
        "a-league-men" => &[
            ("1", "Melbourne City", None),
            ("2", "Western Sydney Wanderers", None),
            ("3", "Melbourne Victory", None),
            ("4", "Auckland FC", None),
            ("5", "Sydney FC", None),
        ],
        "premier-league" => &[
            ("1", "Liverpool", None),
            ("2", "Crystal Palace", None),
            ("3", "Chelsea", None),
            ("4", "Manchester United", Some("Man United")),
            ("5", "Manchester City", Some("Man City")),
            ("6", "Newcastle United", None),
            ("7", "Everton", None),
        ],
        "champions-league" => &[
            ("1", "Paris Saint-Germain", None),
            ("2", "Arsenal", None),
            ("3", "Bayern Munich", None),
        ],
        "la-liga" => &[
            ("1", "Barcelona", None),
            ("2", "Real Madrid", None),
            ("3", "Atletico Madrid", None),
        ],
        "bundesliga" => &[
            ("1", "1. FC Koln", None),
            ("2", "RB Leipzig", None),
            ("3", "Bayer Leverkusen", None),
        ],
        "nba" => &[
            ("1", "Washington Wizards", None),
            ("2", "Indiana Pacers", None),
            ("3", "San Antonio Spurs", None),
            ("4", "Chicago Bulls", None),
            ("5", "Los Angeles Lakers", None),
            ("6", "Milwaukee Bucks", None),
            ("7", "Los Angeles Clippers", None),
        ],
        "six-nations" => &[("1", "Scotland", None), ("2", "England", None)],
        "nrl" => &[
            ("1", "Penrith Panthers", None),
            ("2", "Melbourne Storm", None),
        ],
        _ => &[],
    };
    list.iter()
        .map(|(id, name, short)| TeamRow {
            league: league.into(),
            id: (*id).into(),
            name: (*name).into(),
            short_name: short.map(str::to_string),
            abbreviation: (*name == "San Antonio Spurs").then(|| "SAS".to_string()),
        })
        .collect()
}

pub(super) fn judge_with(
    title: &str,
    published: Option<&str>,
    ev: &EventRow,
    earlier_same_pair: bool,
) -> Verdict {
    judge_known(title, published, ev, earlier_same_pair, true, true)
}

/// The stretches of time a schedule is recorded in full for: all of it, or none.
pub(super) fn known_spans(known: bool) -> Vec<(OffsetDateTime, OffsetDateTime)> {
    if known {
        vec![(
            OffsetDateTime::UNIX_EPOCH,
            OffsetDateTime::UNIX_EPOCH + time::Duration::days(365 * 200),
        )]
    } else {
        vec![]
    }
}

/// As `judge_with`, saying whether the league's team list and schedule are whole and recent.
fn time_of_event(ev: &EventRow) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(&ev.starts_at, &Rfc3339).ok()
}

/// The publish time a test gives a release: as written, three hours after the event's start
/// when none is given, and none at all for `Some("none")`.
pub(super) fn published_for(published: Option<&str>, ev: &EventRow) -> Option<OffsetDateTime> {
    match published {
        Some("none") => None,
        Some(p) => Some(at(p)),
        None => time_of_event(ev).map(|t| t + time::Duration::hours(3)),
    }
}

pub(super) fn judge_known(
    title: &str,
    published: Option<&str>,
    ev: &EventRow,
    earlier_same_pair: bool,
    teams_complete: bool,
    schedule_complete: bool,
) -> Verdict {
    // A release is put up after the event it is of: with no time given, three hours after its
    // start. `Some("none")` is a release with no publish time at all.
    let parsed = release::parse(title, published_for(published, ev));
    let teams = if teams_complete {
        teams_of(&ev.league)
    } else {
        vec![]
    };
    let neighbours: Vec<EventRow> = if schedule_complete {
        all_events()
            .into_iter()
            .filter(|o| o.league == ev.league)
            .collect()
    } else {
        vec![]
    };
    let cx = Context::new(
        catalog::find(&ev.league).unwrap(),
        ev,
        &teams,
        &neighbours,
        earlier_same_pair,
        teams_complete,
        known_spans(schedule_complete),
        true,
    );
    judge(&parsed, ev, &cx)
}

fn verdict(title: &str, published: Option<&str>, id: &str) -> Verdict {
    judge_with(title, published, &event(id), false)
}

#[derive(Debug)]
pub(super) enum Want {
    /// part, confidence, reason
    Match(Part, Confidence, Option<&'static str>),
    Refused(RefusalCode),
}
use Confidence::{Likely, Sure};
use RefusalCode::*;
use Want::{Match as M, Refused as R};

/// (title, publish time, the event it is judged against, what must come out).
/// The rows of the table that are `sure` once they carry an uploader's tag: the ones that are,
/// and the ones capped only for having none, with a known tag appended (a bracket one for a
/// forum-style title). What the properties are run over.
pub(super) fn sure_table() -> Vec<(String, Option<&'static str>, &'static str)> {
    table()
        .into_iter()
        .filter_map(|(title, published, id, want)| match want {
            Want::Match(_, Confidence::Sure, None) => Some((title.to_string(), published, id)),
            Want::Match(_, Confidence::Likely, Some("no_uploader_tag")) => {
                let tagged = if title.trim_end().ends_with(']') {
                    format!("{title} [NX]")
                } else {
                    format!("{title}-DARKSPORT")
                };
                Some((tagged, published, id))
            }
            _ => None,
        })
        .collect()
}

pub(super) fn table() -> Vec<(&'static str, Option<&'static str>, &'static str, Want)> {
    vec![
        // ---- MMA, from the live indexers
        (
            "UFC.Fight.Night.293.Cortes-Acosta.vs.Walker.Prelims.1080p.WEB-DL.H264.Fight-BB",
            Some("2026-11-08T15:17:00Z"),
            "event:ufc:1",
            M(Part::Prelims, Likely, Some("serial_unverified")),
        ),
        (
            "UFC.Fight.Night.234.Ankalaev.vs.Walker.2.1080p.WEB-DL.H264.Fight-BB",
            Some("2024-01-14T06:46:00Z"),
            "event:ufc:2",
            M(Part::Main, Likely, Some("serial_unverified")),
        ),
        (
            "UFC.Fight.Night.234.Ankalaev.vs.Walker.2.Prelims.720p.WEB-DL.H264.Fight-BB",
            Some("2024-01-13T23:00:00Z"),
            "event:ufc:2",
            M(Part::Prelims, Likely, Some("serial_unverified")),
        ),
        (
            "PFL Tampa Cyborg vs Vieira 2026 08 22 720p WEB-DL H264-FBB",
            Some("2026-08-24T01:20:00Z"),
            "event:pfl:10",
            M(Part::Whole, Likely, Some("unlabelled_whole")),
        ),
        (
            "PFL Tampa Cyborg vs Vieira 2026 08 22 720p WEB-DL H 264-FBB",
            Some("2026-08-25T01:20:00Z"),
            "event:pfl:10",
            M(Part::Whole, Likely, Some("unlabelled_whole")),
        ),
        (
            "PFL Washington DC Jean vs Rodriguez 2026 07 25 720p WEB-DL H 264-FBB",
            Some("2026-07-27T01:00:00Z"),
            "event:pfl:11",
            M(Part::Whole, Likely, Some("unlabelled_whole")),
        ),
        (
            "PFL MENA 9 2026 720p WEB H 264-JFF",
            Some("2026-09-13T01:00:00Z"),
            "event:pfl:12",
            R(NotTheEvent),
        ),
        (
            "PFL 7 2026 Brussels 720p WEB-DL H 264-FBB",
            Some("2026-09-14T01:00:00Z"),
            "event:pfl:12",
            M(Part::Whole, Likely, Some("number_unverified")),
        ),
        (
            "PFL 7 2026 Brussels 720p WEB-DL H 264-FBB",
            Some("2026-09-14T01:00:00Z"),
            "event:pfl:10",
            R(NotTheEvent),
        ),
        (
            "One Championship ONE Fight Night 49 60fps 1080p WEBRip h264-TJ [TJET]",
            Some("2026-11-08T01:00:00Z"),
            "event:ufc:1",
            R(NoLeague),
        ),
        (
            "One Championship ONE Friday Fights 177 60fps 720p WEBRip h264-TJ [TJET]",
            Some("2026-11-08T01:00:00Z"),
            "event:ufc:1",
            R(NoLeague),
        ),
        (
            "UFC 334 Embedded-Vlog Series-Episode 2 1080p WEBRip h264-TJ [TJET]",
            Some("2026-11-11T10:21:00Z"),
            "event:ufc:4",
            R(NotTheEvent),
        ),
        (
            "UFC 334 Countdown 1080p WEBRip h264-TJ [TJET]",
            Some("2026-11-10T12:35:00Z"),
            "event:ufc:4",
            R(NotTheEvent),
        ),
        (
            "UFC 334 Countdown 720p WEBRip h264-TJ [TJET]",
            Some("2026-11-10T12:34:00Z"),
            "event:ufc:4",
            R(NotTheEvent),
        ),
        (
            "UFC 334 Embedded-Vlog Series-Episode 3 1080p WEBRip h264-TJ [TJET]",
            Some("2026-11-12T16:05:00Z"),
            "event:ufc:4",
            R(NotTheEvent),
        ),
        (
            "UFC 333 Embedded-Vlog Series-Episode 6 1080p WEBRip h264-TJ [TJET]",
            Some("2026-10-25T12:02:00Z"),
            "event:ufc:3",
            R(NotTheEvent),
        ),
        (
            "UFC 333 Countdown 720p WEBRip h264-TJ [TJET]",
            Some("2026-10-22T08:00:00Z"),
            "event:ufc:3",
            R(NotTheEvent),
        ),
        (
            "UFC.333.Volkanovski.vs.Evloev.1080p.WEB-DL.H264-nVa",
            Some("2026-11-02T01:07:00Z"),
            "event:ufc:3",
            M(Part::Main, Likely, Some("unlabelled_main")),
        ),
        (
            "UFC.333.Volkanovski.vs.Evloev.Main.Card.1080p.WEB-DL.H264-nVa",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
            M(Part::Main, Sure, None),
        ),
        (
            "UFC.333.Prelims.1080p.WEB-DL.H264.Fight-BB",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
            M(Part::Prelims, Sure, None),
        ),
        (
            "UFC.333.Early.Prelims.1080p.WEB-DL.H264.Fight-BB",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
            M(Part::EarlyPrelims, Sure, None),
        ),
        (
            "UFC.333.Volkanovski.vs.Evloev.Main.Card.And.Prelims.1080p.IPTV.MKV/H.265",
            Some("2026-10-28T01:24:00Z"),
            "event:ufc:3",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "UFC 333 Volkanovski Vs Evloev 1080P 60FPS Full Event WEB-DL AAC2.0. X265 POOTLED",
            Some("2026-10-27T19:15:00Z"),
            "event:ufc:3",
            M(Part::Whole, Sure, None),
        ),
        (
            "UFC 333&nbsp;Volkanovski Vs Evloev 1080P 60FPS Full Event WEB-DL AAC2.0. X265 POOTLED",
            Some("2026-10-27T19:15:00Z"),
            "event:ufc:3",
            M(Part::Whole, Sure, None),
        ),
        (
            "UFC.Fight.Night.201.Walker.vs.Hill.1080p.WEB-DL.H264-SHREDDiE",
            Some("2022-02-20T03:15:00Z"),
            "event:ufc:5",
            M(Part::Main, Likely, Some("serial_unverified")),
        ),
        (
            "UFC.Fight.Night.201.Walker.vs.Hill.Prelims.1080p.WEB-DL.H264.Fight-BB",
            Some("2022-02-19T23:00:00Z"),
            "event:ufc:5",
            M(Part::Prelims, Likely, Some("serial_unverified")),
        ),
        (
            "UFC.Fight.Night.201.Walker.vs.Hill.Prelims.1080p.WEB-DL.H264.Fight-BB",
            Some("2022-02-19T23:00:00Z"),
            "event:ufc:1",
            R(PublishedBeforeEvent),
        ),
        (
            "UFC.Fight.Night.201.Walker.vs.Hill.1080p.WEB-DL.H264-SHREDDiE",
            Some("2026-11-08T03:15:00Z"),
            "event:ufc:1",
            R(FightersMismatch),
        ),
        (
            "UFC 334 Gane KO Hokit 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            R(NotTheEvent),
        ),
        // ---- soccer
        (
            "A-League 2026 11 07 Melbourne City Vs Western Sydney Wanderers 1080p HDTV H264-DARKSPORT",
            None,
            "event:a-league-men:20",
            M(Part::Whole, Sure, None),
        ),
        (
            "A-League 2026 10 24 Melbourne Victory Vs Melbourne City 1080p HDTV H264-DARKSPORT",
            None,
            "event:a-league-men:21",
            M(Part::Whole, Sure, None),
        ),
        (
            "A-League Mens 2026 05 23 Final Auckland FC Vs Sydney FC 1080p HDTV H264-DARKSPORT",
            None,
            "event:a-league-men:22",
            M(Part::Whole, Sure, None),
        ),
        (
            "Frauen-Bundesliga 2026 11 09 Koln vs Leipzig 720p WEB h264-ViCEROYS",
            None,
            "event:bundesliga:90",
            R(OtherCompetition),
        ),
        (
            "2 Bundesliga 2026 10 31 FC Kaiserslautern vs Hertha BSC 720p WEB h264-ViCEROYS",
            None,
            "event:bundesliga:90",
            R(QualifiedLeague),
        ),
        (
            "EPL.2019.01.19.Liverpool.vs.Crystal.Palace.XviD-AFG",
            None,
            "event:premier-league:30",
            M(Part::Whole, Sure, None),
        ),
        (
            "EPL.2019.01.19.Liverpool.vs.Crystal.Palace.XviD-AFG",
            None,
            "event:premier-league:31",
            R(WrongDate),
        ),
        (
            "EPL - Liverpool vs Crystal Palace 19.01.2019 720p HD",
            None,
            "event:premier-league:30",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "EPL - Crystal Palace vs Liverpool 720p HD 20.08.2018",
            None,
            "event:premier-league:31",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "EPL - Crystal Palace vs Liverpool 720p HD 20.08.2018",
            None,
            "event:premier-league:30",
            R(WrongDate),
        ),
        (
            "Match Of The Day 2026 11 08 1080p HEVC x265-MeGusta",
            None,
            "event:premier-league:32",
            R(NoLeague),
        ),
        (
            "EPL 2026-2027 / Matchweek 9 / 31.10.2026 / Chelsea FC vs Manchester United [Soccer, WEB-DL HD/720p/25fps MKV/H.264, EN, TNT Sport HD]",
            None,
            "event:premier-league:32",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "EPL 2026-2027 / Matchweek 6 / 11.10.2026 / Liverpool vs. Manchester City [Soccer, IPTV/720p/50fps MKV/H.264, NL, Viaplay TV HD]",
            None,
            "event:premier-league:33",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "Primera Division 2026-2027 / Matchday 10 / 25.10.2026 / Barcelona - Real Madrid [Soccer, WEB-DL HD/1080p, MKV/H.264, EN]",
            None,
            "event:la-liga:50",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "UEFA Champions League / Final / 30.05.2026 / Paris Saint-Germain @ Arsenal London [Soccer, WEB-DL HD/1080p/60fps, MKV/H.264, EN/CBS]",
            None,
            "event:champions-league:40",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "UEFA Champions League / SF / G1 / 28.04.2026 / Paris Saint-Germain @ Bayern Munich [Soccer, WEB-DL HD/1080p/50fps, MKV/H.264, EN/P",
            None,
            "event:champions-league:41",
            M(Part::Whole, Likely, Some("round_unverified")), // (the channel was cut off: "EN/P")
        ),
        // ---- basketball
        (
            "NBA 2026-2027 / RS / 11.11.2026 / Washington Wizards @ Indiana Pacers [Basketball, WEB-DL HD/720p/60fps, MKV/H.264, EN/DAZN-IND]",
            None,
            "event:nba:60",
            M(Part::Whole, Likely, Some("round_unverified")),
        ),
        (
            "NBA 2026-2027 / RS / 11.11.2026 / San Antonio Spurs @ Chicago Bulls [Basketball, WEB-DL HD/1080p/60fps, MKV/H.264, EN/DAZN-SAS]",
            None,
            "event:nba:61",
            M(Part::Whole, Likely, Some("round_unverified")),
        ),
        (
            "NBA 2026-2027 / RS / 11.11.2026 / Los Angeles Lakers @ Milwaukee Bucks [Basketball, WEB-DL HD/720p/60fps, MKV/H.264, EN/SN Spectrum]",
            None,
            "event:nba:62",
            M(Part::Whole, Likely, Some("round_unverified")),
        ),
        (
            "Euroleague 2026-27 / 07.10.2026 / Paris @ Lyon-Villeurbanne [Basketball, WEB-DL HD/1080p/50fps, MKV/H.264, Romanian]",
            None,
            "event:nba:60",
            R(NoLeague),
        ),
        // ---- rugby
        (
            "Rugby League Super League Wigan vs Wakefield 26 Sep 2026 BBC 720p H.264-teaspoon",
            None,
            "event:nrl:100",
            R(NoLeague),
        ),
        (
            "Rugby League Super League Warrington vs Hull KR 19 Sep 2026 BBC 720p-teaspoon",
            None,
            "event:nrl:100",
            R(NoLeague),
        ),
        (
            "BBC.Women's.Six.Nations.2026.05.17.Ireland.v.Scotland.1080i.HDTV.h264-NX",
            None,
            "event:six-nations:70",
            R(OtherCompetition),
        ),
        (
            "Guinness Six Nations 2022 / Round1 / Scotland. vs. England [Rugby, 05.02.2022, HDTV/1080p, MKV/H.264, EN]",
            None,
            "event:six-nations:70",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        // ---- golf
        (
            "LPGA / DOW Invitational / Day 4 / 22.07.2023 [Golf, WEB-DL HD/1080p/30fps, MP4/H.264, EN/NBC]",
            None,
            "event:lpga-tour:80",
            M(Part::Round(4), Likely, Some("no_uploader_tag")),
        ),
        (
            "LET / La Sella Open / Day 1 / 20.07.2023 [Golf, WEB-DL HD/1080p/30fps, MP4/H.264, EN/NBC]",
            None,
            "event:lpga-tour:80",
            R(NoLeague),
        ),
        // ---- games, anime and shows that mention a league
        (
            "NBA 2K27 Update v2 1-RUNE",
            None,
            "event:nba:60",
            R(NotAnEvent),
        ),
        (
            "PGA TOUR 2K25 (v2026.03.27 & All DLCs) (Hypervisor|DenuvOwO)",
            None,
            "event:pga-tour:81",
            R(NotAnEvent),
        ),
        (
            "Aoashi S02E03 Premier League Match 7-Kashiwa Dai Business School 1080p CR WEB-DL MULTi AAC2.0 H.264-VARYG",
            None,
            "event:premier-league:32",
            R(NotAnEvent),
        ),
        (
            "Tom Scott England 2026 S01E31 1080p NBLA WEB-DL AAC2 0 H 264-NTb",
            None,
            "event:premier-league:32",
            R(NotAnEvent),
        ),
        (
            "Premier League Years S05E12 1080p WEB-DL",
            None,
            "event:premier-league:32",
            R(NotAnEvent),
        ),
        // ---- near misses
        // the date: two days off is another game, one day off is the same (the UTC day turns)
        (
            "A-League 2026 11 09 Melbourne City Vs Western Sydney Wanderers 1080p",
            None,
            "event:a-league-men:20",
            R(WrongDate),
        ),
        (
            "A-League 2026 11 05 Melbourne City Vs Western Sydney Wanderers 1080p",
            None,
            "event:a-league-men:20",
            R(WrongDate),
        ),
        (
            "A-League 2026 11 08 Melbourne City Vs Western Sydney Wanderers 1080p",
            None,
            "event:a-league-men:20",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "A-League 2026 11 06 Melbourne City Vs Western Sydney Wanderers 1080p",
            None,
            "event:a-league-men:20",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "EPL.2019.01.21.Liverpool.vs.Crystal.Palace.XviD-AFG",
            None,
            "event:premier-league:30",
            R(WrongDate),
        ),
        // day first, never month first
        (
            "EPL 05.02.2019 Liverpool vs Crystal Palace 720p",
            None,
            "event:premier-league:30",
            R(WrongDate),
        ),
        (
            "EPL 01.19.2019 Liverpool vs Crystal Palace 720p",
            None,
            "event:premier-league:30",
            R(DateUnreadable),
        ),
        (
            "EPL Liverpool vs Crystal Palace 720p HD",
            None,
            "event:premier-league:30",
            R(NoDate),
        ),
        // the competition
        (
            "NBL 2026 11 11 Washington Wizards vs Indiana Pacers 720p",
            None,
            "event:nba:60",
            R(OtherLeague),
        ),
        (
            "NBA NBL 2026 11 11 Washington Wizards vs Indiana Pacers 720p",
            None,
            "event:nba:60",
            R(AmbiguousLeague),
        ),
        (
            "Premier League 2019 01 19 Liverpool vs Crystal Palace 720p",
            None,
            "event:premier-league:30",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "Premier League U21 2019 01 19 Liverpool vs Crystal Palace 720p",
            None,
            "event:premier-league:30",
            R(OtherCompetition),
        ),
        (
            "Premier League 2019 01 19 Liverpool Reserves vs Crystal Palace 720p",
            None,
            "event:premier-league:30",
            R(OtherCompetition),
        ),
        (
            "Scottish Premier League 2019 01 19 Liverpool vs Crystal Palace 720p",
            None,
            "event:premier-league:30",
            R(QualifiedLeague),
        ),
        // the teams
        (
            "A-League 2026 11 07 Melbourne City Vs Sydney FC 1080p",
            None,
            "event:a-league-men:20",
            R(TeamMismatch),
        ),
        (
            "A-League 2026 11 07 Western Sydney Wanderers Vs Melbourne City 1080p",
            None,
            "event:a-league-men:20",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "A-League 2026 11 07 Melbourne Vs Western Sydney Wanderers 1080p",
            None,
            "event:a-league-men:20",
            R(AmbiguousTeam),
        ),
        (
            "EPL 2026 10 31 Manchester vs Chelsea 720p",
            None,
            "event:premier-league:32",
            R(AmbiguousTeam),
        ),
        (
            "EPL 2026 10 31 Man United vs Chelsea FC 720p",
            None,
            "event:premier-league:32",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "A-League 2026 11 07 Banana Melbourne City Vs Western Sydney Wanderers 1080p",
            None,
            "event:a-league-men:20",
            R(TeamMismatch),
        ),
        (
            "A-League 2026 11 07 Melbourne City Vs Western Sydney Wanderers Highlights 1080p",
            None,
            "event:a-league-men:20",
            R(NotTheEvent),
        ),
        (
            "A-League 2026 11 07 Melbourne City 3-1 Western Sydney Wanderers 1080p",
            None,
            "event:a-league-men:20",
            R(NoTeams),
        ),
        (
            "A-League Melbourne City Vs Western Sydney Wanderers Preseason Friendly Cup 2026 11 07",
            None,
            "event:a-league-men:20",
            R(NotTheEvent),
        ),
        (
            "EPL 2026-2027 / Matchweek 7 / 31.10.2026 / Chelsea FC vs Manchester United [Soccer, WEB-DL HD/720p, EN]",
            None,
            "event:premier-league:32",
            R(RoundMismatch),
        ),
        (
            "EPL 2024-2025 / Matchweek 9 / 31.10.2026 / Chelsea FC vs Manchester United [Soccer, WEB-DL HD/720p, EN]",
            None,
            "event:premier-league:32",
            R(WrongSeason),
        ),
        // the card
        (
            "UFC Fight Night 333 Volkanovski vs Evloev 1080p WEB-DL",
            Some("2026-10-25T01:00:00Z"),
            "event:ufc:3",
            R(SeriesMismatch),
        ),
        (
            "UFC 335 Volkanovski vs Evloev 1080p WEB-DL",
            Some("2026-10-25T01:00:00Z"),
            "event:ufc:3",
            R(NumberMismatch),
        ),
        (
            "UFC.Fight.Night.293.Cortes-Acosta.vs.Walker.Prelims.1080p.WEB-DL.H264.Fight-BB",
            Some("2026-11-08T15:17:00Z"),
            "event:ufc:2",
            R(FightersMismatch),
        ),
        (
            "UFC.Fight.Night.293.Cortes-Acosta.vs.Walker.Prelims.1080p.WEB-DL.H264.Fight-BB",
            Some("2026-12-20T15:17:00Z"),
            "event:ufc:6",
            R(NoEvidence),
        ),
        // the same fighters in another year; a rematch published long after
        (
            "PFL Tampa Cyborg vs Vieira 2025 08 22 720p WEB-DL H264-FBB",
            None,
            "event:pfl:10",
            R(WrongDate),
        ),
        (
            "UFC.Fight.Night.234.Ankalaev.vs.Walker.2.1080p.WEB-DL.H264.Fight-BB",
            Some("2026-06-01T00:00:00Z"),
            "event:ufc:2",
            R(OutsideWindow),
        ),
        (
            "UFC.Fight.Night.234.Ankalaev.vs.Walker.2.1080p.WEB-DL.H264.Fight-BB",
            Some("none"),
            "event:ufc:2",
            R(NoEvidence),
        ),
        (
            "UFC 334 Gane vs Hokit 1080p WEB-DL",
            Some("2026-11-07T22:00:00Z"),
            "event:ufc:4",
            R(PublishedBeforeEvent),
        ),
        (
            "UFC 334 Gane vs Hokit Banana 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            R(FightersMismatch),
        ),
        (
            "UFC 334 Banana 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            R(NotTheEvent),
        ),
        (
            "UFC 334 Gane vs Hokit 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            M(Part::Main, Likely, Some("unlabelled_main")),
        ),
        (
            "UFC 334 Gane vs Hokit Main Card 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            M(Part::Main, Likely, Some("no_uploader_tag")),
        ),
        // a title that gives an unknown date or no names has too little evidence for a guess
        (
            "UFC 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            R(NoEvidence),
        ),
        (
            "PFL 2026 720p WEB-DL",
            Some("2026-08-24T01:20:00Z"),
            "event:pfl:10",
            R(NoEvidence),
        ),
        // ---- review round 1: consecutive fixtures, siblings, hidden words, bouts, PFL, partial names
        // C1: the same two teams two nights running: a title that fits both is a guess for both
        (
            "NBA 2026 11 12 Los Angeles Lakers vs Milwaukee Bucks 1080p WEB-DL",
            Some("2026-11-13T05:00:00Z"),
            "event:nba:62",
            M(Part::Whole, Likely, Some("another_fixture_fits")),
        ),
        (
            "NBA 2026 11 12 Los Angeles Lakers vs Milwaukee Bucks 1080p WEB-DL",
            Some("2026-11-13T05:00:00Z"),
            "event:nba:64",
            M(Part::Whole, Likely, Some("another_fixture_fits")),
        ),
        (
            "NBA 2026 11 11 Los Angeles Lakers vs Milwaukee Bucks 1080p WEB-DL",
            None,
            "event:nba:62",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "NBA 2026 11 11 Los Angeles Lakers vs Milwaukee Bucks 1080p WEB-DL",
            None,
            "event:nba:64",
            R(WrongDate),
        ),
        (
            "NBA 2026 11 14 Los Angeles Lakers vs Milwaukee Bucks 1080p WEB-DL",
            None,
            "event:nba:64",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        // C3: text in a [..], (..) or {..} group is read like any other
        (
            "EPL.2019.01.19.Liverpool.vs.Crystal.Palace.[Highlights].1080p.WEB-DL",
            None,
            "event:premier-league:30",
            R(NotTheEvent),
        ),
        (
            "EPL.2019.01.19.Liverpool.vs.Crystal.Palace.(Highlights).1080p.WEB-DL",
            None,
            "event:premier-league:30",
            R(NotTheEvent),
        ),
        (
            "EPL.2019.01.19.Liverpool.vs.Crystal.Palace.{Highlights}.1080p.WEB-DL",
            None,
            "event:premier-league:30",
            R(NotTheEvent),
        ),
        (
            "EPL.2019.01.19.Liverpool.vs.Crystal.Palace.1080p.WEB-DL.Highlights-DARKSPORT",
            None,
            "event:premier-league:30",
            R(NotTheEvent),
        ),
        (
            "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p WEB-DL [Weigh-In]",
            None,
            "event:premier-league:30",
            R(NotTheEvent),
        ),
        // (a copy with another commentary is the game; it is named, and not recommended)
        (
            "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p WEB-DL [Spanish Commentary]",
            None,
            "event:premier-league:30",
            M(Part::Whole, Likely, Some("unverified_text")),
        ),
        (
            "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p WEB-DL [EN, One Two Three Four Five Six]",
            None,
            "event:premier-league:30",
            M(Part::Whole, Likely, Some("unverified_text")),
        ),
        (
            "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p WEB-DL [Women's]",
            None,
            "event:premier-league:30",
            R(OtherCompetition),
        ),
        (
            "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p WEB-DL [S01E03]",
            None,
            "event:premier-league:30",
            R(NotAnEvent),
        ),
        (
            "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p WEB-DL [Bundesliga]",
            None,
            "event:premier-league:30",
            R(AmbiguousLeague),
        ),
        (
            "UFC.333.Volkanovski.vs.Evloev.Main.Card.1080p.WEB-DL-nVa [Prelims]",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
            R(NotTheEvent),
        ),
        (
            "UFC.333.Volkanovski.vs.Evloev.1080p.WEB-DL-nVa [Main Card]",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
            M(Part::Main, Sure, None),
        ),
        (
            "EPL 2019 01 19 Liverpool vs Crystal Palace Resum&shy;en 1080p",
            None,
            "event:premier-league:30",
            R(NotTheEvent),
        ),
        (
            "EPL 2019 01 19 Liverpool vs Crystal Palace Resum\u{200b}en 1080p",
            None,
            "event:premier-league:30",
            R(NotTheEvent),
        ),
        (
            "EPL 2019 01 19 Liverpool vs Crystal Palace&nbsp;Highlights 1080p",
            None,
            "event:premier-league:30",
            R(NotTheEvent),
        ),
        (
            "EPL 2019 01 19 Liverpool vs Crystal Palace &lt;Highlights&gt; 1080p",
            None,
            "event:premier-league:30",
            R(NotTheEvent),
        ),
        // C4: a segment after the second team is read
        (
            "EPL 2019.01.19 Liverpool vs Crystal Palace | Resumen 1080p WEB-DL",
            None,
            "event:premier-league:30",
            R(NotTheEvent),
        ),
        (
            "EPL 2019.01.19 Liverpool vs Crystal Palace | Zebra 1080p WEB-DL",
            None,
            "event:premier-league:30",
            R(NotTheEvent),
        ),
        (
            "EPL 2019.01.19 Liverpool vs Crystal Palace / Quokka / 1080p WEB-DL",
            None,
            "event:premier-league:30",
            R(NotTheEvent),
        ),
        (
            "EPL 2019.01.19 Liverpool vs Crystal Palace | BBC | 1080p WEB-DL",
            None,
            "event:premier-league:30",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p WEB-DL Resumen-BB",
            None,
            "event:premier-league:30",
            R(NotTheEvent),
        ),
        (
            "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p WEB-DL Zebra Quokka",
            None,
            "event:premier-league:30",
            M(Part::Whole, Likely, Some("unverified_text")),
        ),
        // C5: one bout, a part of a card, or a bare "Main" is not the card
        (
            "UFC 334 Gane vs Hokit Main Event 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            R(SingleBout),
        ),
        (
            "UFC 334 Gane vs Hokit Co-Main Event 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            R(SingleBout),
        ),
        (
            "UFC 334 Gane vs Hokit Full Fight 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            R(SingleBout),
        ),
        (
            "UFC 334 Gane vs Hokit Fight 3 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            R(SingleBout),
        ),
        (
            "UFC 334 Prelims Fight 2 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            R(SingleBout),
        ),
        (
            "UFC 334 Gane vs Hokit Main 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            R(SingleBout),
        ),
        (
            "UFC 334 Gane vs Hokit Round 2 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            R(NotTheEvent),
        ),
        (
            "UFC 334 Hokit vs Smith 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            R(FightersMismatch),
        ),
        (
            "UFC 334 Smith vs Jones Prelims 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            R(FightersMismatch),
        ),
        // C6: an unlabelled PFL file may be the whole event, the main card or one fight
        (
            "PFL Tampa Cyborg vs Vieira 2026 08 22 720p WEB-DL-FBB",
            Some("2026-08-24T01:20:00Z"),
            "event:pfl:10",
            M(Part::Whole, Likely, Some("unlabelled_whole")),
        ),
        (
            "PFL Tampa Cyborg vs Vieira 2026 08 22 Full Event 720p WEB-DL-FBB",
            Some("2026-08-24T01:20:00Z"),
            "event:pfl:10",
            M(Part::Whole, Sure, None),
        ),
        (
            "PFL Tampa Cyborg vs Vieira 2026 08 22 Main Card 720p WEB-DL-FBB",
            Some("2026-08-24T01:20:00Z"),
            "event:pfl:10",
            M(Part::Main, Sure, None),
        ),
        // C7: aliases across countries and tiers
        (
            "Russian Premier League 2019 01 19 Liverpool vs Crystal Palace 720p",
            None,
            "event:premier-league:30",
            R(QualifiedLeague),
        ),
        (
            "Premier League 2 2019 01 19 Liverpool vs Crystal Palace 720p",
            None,
            "event:premier-league:30",
            R(QualifiedLeague),
        ),
        (
            "Scottish Premiership 2019 01 19 Liverpool vs Crystal Palace 720p",
            None,
            "event:premier-league:30",
            R(NoLeague),
        ),
        (
            "A-League Women 2026 11 07 Melbourne City vs Western Sydney Wanderers 720p",
            None,
            "event:a-league-men:20",
            R(OtherCompetition),
        ),
        (
            "Bundesliga 2 2026 11 09 Koln vs RB Leipzig 720p",
            None,
            "event:bundesliga:90",
            R(QualifiedLeague),
        ),
        (
            "AFC Champions League 2026 04 28 Paris Saint-Germain vs Bayern Munich 720p",
            None,
            "event:champions-league:41",
            R(QualifiedLeague),
        ),
        (
            "NBA G League 2026 11 11 Washington Wizards vs Indiana Pacers 720p",
            None,
            "event:nba:60",
            R(NotTheEvent),
        ),
        (
            "NBA Summer League 2026 11 11 Washington Wizards vs Indiana Pacers 720p",
            None,
            "event:nba:60",
            R(NotTheEvent),
        ),
        // date order and the date a replay was uploaded
        (
            "EPL 2026 11 03 Chelsea vs Manchester United 720p",
            Some("2026-11-03T20:00:00Z"),
            "event:premier-league:32",
            R(WrongDate),
        ),
        (
            "EPL 11.10.2026 Liverpool vs Manchester City 720p",
            None,
            "event:premier-league:33",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "EPL 11.10.2026 Liverpool vs Manchester City 720p",
            None,
            "event:premier-league:34",
            R(WrongDate),
        ),
        (
            "EPL 10.11.2026 Liverpool vs Manchester City 720p",
            None,
            "event:premier-league:34",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "EPL 31.10.26 Chelsea vs Manchester United 720p",
            None,
            "event:premier-league:32",
            R(NoDate),
        ),
        (
            "EPL 2026 31 10 Chelsea vs Manchester United 720p",
            None,
            "event:premier-league:32",
            R(NoDate),
        ),
        (
            "EPL 2026 10 31 Chelsea vs Manchester United Repost 2026 12 01 720p",
            None,
            "event:premier-league:32",
            R(WrongDate),
        ),
        // two clubs in one city, and a word two clubs share
        (
            "A-League 2026 11 07 Sydney vs Melbourne 720p",
            None,
            "event:a-league-men:20",
            R(AmbiguousTeam),
        ),
        (
            "NBA 2026 11 12 Los Angeles vs Milwaukee 720p",
            None,
            "event:nba:62",
            R(AmbiguousTeam),
        ),
        (
            "La Liga 2026 10 25 Madrid vs Barcelona 720p",
            None,
            "event:la-liga:50",
            R(AmbiguousTeam),
        ),
        (
            "EPL 2026 10 31 United vs Chelsea 720p",
            None,
            "event:premier-league:32",
            R(AmbiguousTeam),
        ),
        // team-name prefixes
        (
            "Bundesliga 2026 11 09 Leverkusen vs RB Leipzig 720p",
            None,
            "event:bundesliga:90",
            R(TeamMismatch),
        ),
        (
            "Bundesliga 2026 11 09 1 FC Koln vs RB Leipzig 720p",
            None,
            "event:bundesliga:90",
            M(Part::Whole, Likely, Some("no_uploader_tag")), // the "1" is part of the club's name
        ),
        (
            "Bundesliga 2026 11 09 Koln vs Leipzig 720p",
            None,
            "event:bundesliga:90",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "EPL 2019 01 19 Liverpool FC vs Crystal Palace FC 720p",
            None,
            "event:premier-league:30",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "EPL 2019 01 19 Liverpool vs Palace 720p",
            None,
            "event:premier-league:30",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "EPL 2019 01 19 FC Liverpool vs Crystal Palace Ladies 720p",
            None,
            "event:premier-league:30",
            R(OtherCompetition),
        ),
        // a late replacement
        (
            "EPL 2026 10 31 Chelsea vs Everton 720p",
            None,
            "event:premier-league:32",
            R(TeamMismatch),
        ),
        (
            "UFC 334 Gane vs Smith Main Card 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            R(FightersMismatch),
        ),
        (
            "UFC 334 Hokit vs Gane Main Card 1080p WEB-DL",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            M(Part::Main, Likely, Some("no_uploader_tag")),
        ),
        // a numbered card and a Fight Night with the same number
        (
            "UFC.293.Cortes-Acosta.vs.Walker.1080p.WEB-DL.H264-DARKSPORT",
            Some("2026-11-08T15:17:00Z"),
            "event:ufc:1",
            R(SeriesMismatch),
        ),
        (
            "UFC Fight Night 334 Gane vs Hokit 1080p WEB-DL-DARKSPORT",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            R(SeriesMismatch),
        ),
        (
            "UFC 334 Gane vs Hokit Prelims 1080p WEB-DL-DARKSPORT",
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
            M(Part::Prelims, Sure, None),
        ),
        // a surname two fights share
        (
            "UFC Fight Night Silva vs Walker 1080p WEB-DL",
            Some("2026-11-08T15:17:00Z"),
            "event:ufc:1",
            R(FightersMismatch),
        ),
        (
            "UFC 332 Silva vs Walker 1080p WEB-DL",
            Some("2026-10-04T01:00:00Z"),
            "event:ufc:7",
            R(FightersMismatch),
        ),
        (
            "UFC 332 Silva vs Wang Prelims 1080p WEB-DL",
            Some("2026-10-04T01:00:00Z"),
            "event:ufc:7",
            M(Part::Prelims, Likely, Some("no_uploader_tag")),
        ),
        (
            "PFL Washington DC Jean vs Smith Full Event 720p WEB-DL",
            Some("2026-07-27T01:00:00Z"),
            "event:pfl:11",
            R(FightersMismatch),
        ),
        // rematch numerals
        (
            "PFL Austin Eblen vs Kasanganay II Full Event 720p WEB-DL",
            Some("2026-07-19T01:00:00Z"),
            "event:pfl:13",
            M(Part::Whole, Likely, Some("no_uploader_tag")),
        ),
        (
            "UFC.Fight.Night.234.Ankalaev.vs.Walker.3.Prelims.720p.WEB-DL.H264.Fight-BB",
            Some("2024-01-13T23:00:00Z"),
            "event:ufc:2",
            M(Part::Prelims, Likely, Some("serial_unverified")),
        ),
        // golf
        (
            "LPGA / DOW Invitational / Day 5 / 24.07.2023 [Golf, WEB-DL HD/1080p]",
            None,
            "event:lpga-tour:80",
            R(NotTheEvent),
        ),
        (
            "LPGA / DOW Invitational / Day 4 / 02.07.2023 [Golf, WEB-DL HD/1080p]",
            None,
            "event:lpga-tour:80",
            R(WrongDate),
        ),
        (
            "LPGA / Other Open / Day 4 / 22.07.2023 [Golf, WEB-DL HD/1080p]",
            None,
            "event:lpga-tour:80",
            R(TournamentMismatch),
        ),
        (
            "LPGA / DOW Invitational / Day 1 / 22.07.2023 [Golf, WEB-DL HD/1080p]",
            None,
            "event:lpga-tour:80",
            R(WrongDate),
        ),
        (
            "LPGA / DOW Invitational / Round 3 / 22.07.2023 [Golf, WEB-DL HD/1080p]",
            None,
            "event:lpga-tour:80",
            M(Part::Round(3), Likely, Some("no_uploader_tag")),
        ),
    ]
}

#[test]
fn every_title_has_its_verdict() {
    let mut failures = vec![];
    for (title, published, id, want) in table() {
        let got = verdict(title, published, id);
        let ok = match (&want, &got) {
            (M(part, conf, reason), Verdict::Match(m)) => {
                m.part == *part && m.confidence == *conf && m.reason == *reason
            }
            (R(code), Verdict::Refused(r)) => r.code == *code,
            _ => false,
        };
        if !ok {
            failures.push(format!(
                "{title}\n    against {id}: want {want:?}, got {got:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} wrong:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn a_release_is_sure_of_at_most_one_event() {
    // Every title of the table, against every event of the fixture set.
    let events = all_events();
    let mut titles: Vec<(String, Option<String>)> = table()
        .into_iter()
        .map(|(t, p, _, _)| (t.to_string(), p.map(str::to_string)))
        .collect();
    // And a title written from each event's own fields.
    for e in &events {
        let league = catalog::find(&e.league).unwrap();
        let alias = league.aliases[0];
        let d = at(&e.starts_at).date();
        let title = match (&e.home_name, &e.away_name) {
            (Some(h), Some(a)) => format!(
                "{alias} {:04} {:02} {:02} {h} vs {a} 1080p WEB-DL",
                d.year(),
                u8::from(d.month()),
                d.day()
            ),
            _ => format!("{} 1080p WEB-DL", e.name.replace(": ", " ")),
        };
        titles.push((title, Some(e.starts_at.clone())));
    }
    for (title, published) in &titles {
        let sure: Vec<&str> = events
            .iter()
            .filter(|e| {
                matches!(
                    judge_with(title, published.as_deref(), e, false),
                    Verdict::Match(ref m) if m.confidence == Confidence::Sure
                )
            })
            .map(|e| e.id.as_str())
            .collect();
        assert!(sure.len() <= 1, "{title} is sure of {sure:?}");
    }
    // The titles written from an event match that event, and no other one with sure.
    for e in &events {
        let league = catalog::find(&e.league).unwrap();
        if league.sport == catalog::Sport::Golf || e.id == "event:ufc:6" {
            continue;
        }
        let d = at(&e.starts_at).date();
        let title = match (&e.home_name, &e.away_name) {
            (Some(h), Some(a)) => format!(
                "{} {:04} {:02} {:02} {h} vs {a} 1080p WEB-DL",
                league.aliases[0],
                d.year(),
                u8::from(d.month()),
                d.day()
            ),
            _ => format!("{} Main Card 1080p WEB-DL", e.name.replace(": ", " ")),
        };
        let v = judge_with(&title, Some(&e.starts_at), e, false);
        assert!(matches!(v, Verdict::Match(_)), "{title}: {v:?}");
    }
}

#[test]
fn shoulder_words_refuse_a_perfect_title() {
    let tails = [
        "Countdown",
        "Embedded",
        "Weigh-In",
        "Weigh Ins",
        "Press Conference",
        "Highlights",
        "Post-Fight",
        "Inside the Octagon",
        "Preview",
        "Interview",
        "Recap",
        "Behind the Scenes",
        "Face-Off",
        "Documentary",
        "Podcast",
        "Analysis",
        "Condensed",
    ];
    for t in tails {
        for title in [
            format!("UFC 334 Gane vs Hokit Main Card {t} 1080p WEB-DL"),
            format!("UFC 334 {t} Gane vs Hokit 1080p WEB-DL"),
            format!("UFC 334 Gane vs Hokit 1080p WEB-DL {t}"),
        ] {
            let v = verdict(&title, Some("2026-11-15T01:00:00Z"), "event:ufc:4");
            assert_eq!(v.refusal(), Some(NotTheEvent), "{title}: {v:?}");
        }
        let v = verdict(
            &format!("A-League 2026 11 07 Melbourne City Vs Western Sydney Wanderers {t} 1080p"),
            None,
            "event:a-league-men:20",
        );
        assert!(v.refusal().is_some(), "{t}: {v:?}");
    }
}

#[test]
fn a_word_no_rule_knows_refuses_even_when_it_is_not_listed() {
    // None of these words is on a list: the zone rule is what refuses them.
    for w in ["Zebra", "Quokka", "Marmalade"] {
        let v = verdict(
            &format!("UFC 334 {w} 1080p WEB-DL"),
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
        );
        assert_eq!(v.refusal(), Some(NotTheEvent), "{w}");
        let v = verdict(
            &format!("UFC 334 Gane vs Hokit {w} 1080p"),
            Some("2026-11-15T01:00:00Z"),
            "event:ufc:4",
        );
        assert!(v.refusal().is_some(), "{w}");
        let v = verdict(
            &format!("A-League {w} 2026 11 07 Melbourne City Vs Western Sydney Wanderers 1080p"),
            None,
            "event:a-league-men:20",
        );
        assert!(v.refusal().is_some(), "{w}");
    }
}

#[test]
fn an_earlier_meeting_of_the_pair_makes_a_rematch_certain() {
    let t = "UFC.Fight.Night.Ankalaev.vs.Walker.2.Prelims.720p.WEB-DL.H264.Fight-BB";
    let ev = event("event:ufc:2");
    let without = judge_with(t, Some("2024-01-13T23:00:00Z"), &ev, false);
    let with = judge_with(t, Some("2024-01-13T23:00:00Z"), &ev, true);
    assert_eq!(without.matched().unwrap().confidence, Confidence::Likely);
    // The existence of an earlier meeting does not make a numeral right: only a stored ordinal
    // equal to it does.
    assert_eq!(with.matched().unwrap().confidence, Confidence::Likely);
    assert_eq!(with.matched().unwrap().reason, Some("rematch_unverified"));
    // The schedule's own "2" is checked against the release's.
    let pfl = event("event:pfl:13");
    for (title, want) in [
        (
            "PFL Austin Eblen vs Kasanganay 2 2026 07 18 Full Event 720p-NX",
            Some(Confidence::Sure),
        ),
        (
            "PFL Austin Eblen vs Kasanganay 3 2026 07 18 Full Event 720p-NX",
            None,
        ),
        (
            "PFL Austin Eblen vs Kasanganay 2026 07 18 Full Event 720p-NX",
            Some(Confidence::Likely),
        ),
    ] {
        let v = judge_with(title, None, &pfl, false);
        assert_eq!(v.matched().map(|m| m.confidence), want, "{title}: {v:?}");
    }
    // The earlier event is found in the stored events by its fighters.
    let mut earlier = row(&EVENTS[1]);
    earlier.id = "event:ufc:99".into();
    earlier.starts_at = "2023-01-14T23:00:00Z".into();
    assert!(find::earlier_same_pair(&event("event:ufc:2"), &[earlier]));
    assert!(!find::earlier_same_pair(
        &event("event:ufc:1"),
        &[row(&EVENTS[4])]
    ));
}

#[test]
fn unlabelled_files_are_told_from_their_siblings() {
    let unlabelled = || {
        let m = verdict(
            "UFC.333.Volkanovski.vs.Evloev.1080p.WEB-DL.H264-nVa",
            Some("2026-11-02T01:07:00Z"),
            "event:ufc:3",
        );
        m.matched().unwrap().clone()
    };
    let labelled = |t: &str| {
        verdict(t, Some("2026-10-28T01:25:00Z"), "event:ufc:3")
            .matched()
            .unwrap()
            .clone()
    };
    // Beside a prelims file of the same uploader it is the main card, for certain.
    let mut items = vec![
        (Some("bb".to_string()), {
            let mut m = unlabelled();
            m.unlabelled = true;
            m
        }),
        (
            Some("bb".to_string()),
            labelled("UFC.333.Prelims.1080p.WEB-DL.H264.Fight-BB"),
        ),
    ];
    settle_unlabelled(&mut items);
    assert_eq!(
        (items[0].1.part, items[0].1.confidence),
        (Part::Main, Confidence::Sure)
    );
    // Beside an explicit main card it is the whole event.
    let mut items = vec![
        (Some("nva".to_string()), unlabelled()),
        (
            Some("nva".to_string()),
            labelled("UFC.333.Volkanovski.vs.Evloev.Main.Card.1080p.WEB-DL.H264-nVa"),
        ),
    ];
    settle_unlabelled(&mut items);
    assert_eq!(
        (items[0].1.part, items[0].1.reason),
        (Part::Whole, Some("unlabelled_full"))
    );
    // Another uploader's labels say nothing about it.
    let mut items = vec![
        (Some("other".to_string()), unlabelled()),
        (
            Some("nva".to_string()),
            labelled("UFC.333.Volkanovski.vs.Evloev.Main.Card.1080p.WEB-DL.H264-nVa"),
        ),
    ];
    settle_unlabelled(&mut items);
    assert_eq!(
        (items[0].1.part, items[0].1.confidence),
        (Part::Main, Confidence::Likely)
    );
}

#[test]
fn quality_language_and_format_come_from_the_title_and_its_block() {
    let r = release::parse(
        "EPL 2026-2027 / Matchweek 6 / 11.10.2026 / Liverpool vs. Manchester City [Soccer, IPTV/720p/50fps MKV/H.264, NL, Viaplay TV HD]",
        None,
    );
    assert_eq!(r.language.as_deref(), Some("Dutch"));
    assert_eq!(r.fps, Some(50));
    assert_eq!(r.quality.resolution(), Some(720));
    let r = release::parse(
        "UFC 333 Volkanovski Vs Evloev 1080P 60FPS Full Event WEB-DL AAC2.0. X265 POOTLED",
        None,
    );
    assert_eq!((r.quality.resolution(), r.fps), (Some(1080), Some(60)));
}

pub(super) fn candidate(title: &str, seeders: i64, published: &str) -> ProwlarrRelease {
    serde_json::from_value(serde_json::json!({
        "guid": format!("g-{title}"),
        "title": title,
        "size": 4_000_000_000i64,
        "indexerId": 1,
        "indexer": "Fake",
        "seeders": seeders,
        "leechers": 1,
        "publishDate": published,
        "infoHash": format!("{:040x}", title.len() * 7919 + title.bytes().map(u64::from).sum::<u64>() as usize),
    }))
    .unwrap()
}

#[test]
fn the_list_keeps_matches_ranks_them_and_counts_the_rest() {
    let ev = event("event:ufc:3");
    let league = catalog::find("ufc").unwrap();
    let found = vec![
        candidate(
            "UFC.333.Volkanovski.vs.Evloev.Main.Card.1080p.WEB-DL.H264-nVa",
            3,
            "2026-10-28T01:25:00Z",
        ),
        candidate(
            "UFC.333.Volkanovski.vs.Evloev.Main.Card.720p.WEB-DL.H264-nVa",
            60,
            "2026-10-28T01:25:00Z",
        ),
        candidate(
            "UFC.333.Prelims.1080p.WEB-DL.H264.Fight-BB",
            20,
            "2026-10-28T01:25:00Z",
        ),
        candidate(
            "UFC 333 Embedded-Vlog Series-Episode 6 1080p WEBRip h264-TJ [TJET]",
            5,
            "2026-10-25T12:02:00Z",
        ),
        candidate(
            "UFC 333 Countdown 720p WEBRip h264-TJ [TJET]",
            5,
            "2026-10-22T08:00:00Z",
        ),
        candidate(
            "UFC 334 Countdown 720p WEBRip h264-TJ [TJET]",
            5,
            "2026-10-26T08:00:00Z",
        ),
        candidate(
            "UFC 333 Volkanovski Vs Evloev 1080P 60FPS Full Event WEB-DL AAC2.0. X265 POOTLED",
            26,
            "2026-10-27T19:15:00Z",
        ),
    ];
    let around = Surroundings::all_known();
    let judged = find::judge_all(found.clone(), &ev, league, &around, None);
    let (all, skipped) = (judged.candidates, judged.skipped);
    assert_eq!(all.len(), 4);
    assert_eq!(skipped.get(&NotTheEvent), Some(&3));
    // The healthy 720p beats the thin 1080p within a part.
    let mains: Vec<&find::Candidate> = all.iter().filter(|c| c.m.part == Part::Main).collect();
    assert!(mains[0].release.title.as_deref().unwrap().contains("720p"));
    assert_eq!(
        all.iter()
            .filter(|c| c.m.confidence == Confidence::Sure)
            .count(),
        4
    );
    // Asking for the prelims: the whole-event file also answers; the main card files do not.
    let narrowed = find::Found {
        candidates: all.clone(),
        skipped: skipped.clone(),
        ..Default::default()
    }
    .for_part(Some(Part::Prelims));
    let (prelims, skipped) = (narrowed.candidates, narrowed.skipped);
    let _ = found;
    assert_eq!(prelims.len(), 2);
    assert!(
        prelims
            .iter()
            .all(|c| matches!(c.m.part, Part::Prelims | Part::Whole))
    );
    assert_eq!(skipped.get(&AnotherPart), Some(&2));
}

#[test]
fn search_texts_are_few_and_plain() {
    let q = |id: &str| find::queries_for(&event(id), catalog::find(&event(id).league).unwrap());
    assert_eq!(
        q("event:ufc:4"),
        vec!["UFC 334", "UFC gane hokit", "UFC gane"]
    );
    assert_eq!(
        q("event:pfl:10"),
        vec!["PFL cyborg vieira", "PFL cyborg", "PFL tampa"]
    );
    assert_eq!(
        q("event:a-league-men:20"),
        vec![
            "A-League 2026 11 07",
            "A-League melbourne city western sydney wanderers",
            "melbourne city western sydney wanderers 07.11.2026"
        ]
    );
    assert_eq!(
        q("event:lpga-tour:80"),
        vec!["LPGA Tour dow invitational", "dow invitational"]
    );
    assert_eq!(q("event:ufc:6"), vec!["UFC 2026 12 19"]);
    for e in all_events() {
        assert!(
            find::queries_for(&e, catalog::find(&e.league).unwrap()).len() <= find::MAX_QUERIES
        );
    }
}

#[test]
fn an_event_is_over_by_the_clock_of_its_sport() {
    use catalog::Sport::*;
    let mut e = event("event:ufc:4");
    let start = at(&e.starts_at);
    assert!(find::availability(&e, Mma, start + time::Duration::hours(7)).is_err());
    assert!(find::availability(&e, Mma, start + time::Duration::hours(8)).is_ok());
    let g = event("event:nba:60");
    let s = at(&g.starts_at);
    assert!(find::availability(&g, Basketball, s + time::Duration::minutes(209)).is_err());
    assert!(find::availability(&g, Basketball, s + time::Duration::minutes(210)).is_ok());
    // A tournament's end date comes early: play goes on into that evening.
    let t = event("event:pga-tour:81");
    let end = at(t.ends_at.as_deref().unwrap());
    assert!(find::availability(&t, Golf, end + time::Duration::hours(23)).is_err());
    assert!(find::availability(&t, Golf, end + time::Duration::hours(24)).is_ok());
    e.state = "cancelled".into();
    assert_eq!(
        find::availability(&e, Mma, start + time::Duration::days(3)),
        Err("cancelled")
    );
    e.state = "postponed".into();
    assert_eq!(
        find::availability(&e, Mma, start + time::Duration::days(3)),
        Err("postponed")
    );
}
