//! The payload scanner's coverage, as two tables: every form it recognises contradicts when it
//! differs from the event and passes when it agrees, and ordinary payloads are accepted. The list
//! of forms is the one in the module documentation of `arr::sport::payload`.

use arr::sport::Part;
use arr::sport::data::{EventRow, TeamRow};
use arr::sport::get::{Known, PayloadCheck, judge_payload, payload_claims};

fn ev(name: &str, league: &str, start: &str, end: Option<&str>) -> EventRow {
    EventRow {
        id: format!("event:{league}:1"),
        league: league.into(),
        season: None,
        starts_at: start.into(),
        ends_at: end.map(str::to_string),
        name: name.into(),
        short_name: None,
        home_id: None,
        home_name: None,
        away_id: None,
        away_name: None,
        round: None,
        venue: None,
        state: "played".into(),
    }
}

fn ufc() -> EventRow {
    ev(
        "UFC 334: Gane vs. Hokit",
        "ufc",
        "2026-05-09T22:00:00Z",
        None,
    )
}

fn fighters() -> Known {
    Known {
        teams: vec![],
        others: vec![
            vec!["oliveira".into(), "lopes".into()],
            vec!["ankalaev".into(), "rountree".into()],
        ],
    }
}

fn team(id: &str, name: &str) -> TeamRow {
    TeamRow {
        league: "premier-league".into(),
        id: id.into(),
        name: name.into(),
        short_name: None,
        abbreviation: None,
    }
}

fn game() -> (EventRow, Known) {
    let mut e = ev(
        "Chelsea v Everton",
        "premier-league",
        "2026-11-07T15:00:00Z",
        None,
    );
    e.home_id = Some("1".into());
    e.home_name = Some("Chelsea".into());
    e.away_id = Some("2".into());
    e.away_name = Some("Everton".into());
    let teams = vec![
        team("1", "Chelsea"),
        team("2", "Everton"),
        team("3", "Fulham"),
        team("4", "Arsenal"),
    ];
    (
        e,
        Known {
            teams,
            others: vec![],
        },
    )
}

fn check(e: &EventRow, k: &Known, name: &str, part: Part) -> Option<String> {
    payload_claims(e, k, name, part).map(|r| r.render())
}

#[test]
fn every_form_in_the_list_contradicts_when_it_differs_and_passes_when_it_agrees() {
    let (e, k) = (ufc(), fighters());
    // (what, differing name, agreeing name, part)
    let table: &[(&str, &str, &str, Part)] = &[
        // dates: four-digit year in any order, any separator
        (
            "date YMD dots",
            "UFC.2026.01.01",
            "UFC.2026.05.09",
            Part::Whole,
        ),
        ("date YMD dashes", "2026-01-01", "2026-05-09", Part::Whole),
        (
            "date YMD underscores",
            "2026_01_01",
            "2026_05_09",
            Part::Whole,
        ),
        ("date YMD spaces", "2026 01 01", "2026 05 09", Part::Whole),
        ("date DMY", "01.01.2026", "09.05.2026", Part::Whole),
        ("date DMY dashes", "01-01-2026", "09-05-2026", Part::Whole),
        ("date MDY", "01.31.2026", "05.09.2026", Part::Whole),
        ("date compact YMD", "20260101", "20260509", Part::Whole),
        ("date compact DMY", "01012026", "09052026", Part::Whole),
        ("date compact MDY", "12312026", "05092026", Part::Whole),
        // two-digit year
        ("date DMY short year", "01.01.26", "09.05.26", Part::Whole),
        (
            "date short year dashes",
            "01-01-26",
            "09-05-26",
            Part::Whole,
        ),
        // month names
        (
            "date month abbreviation",
            "Jan.01.2026",
            "May.09.2026",
            Part::Whole,
        ),
        (
            "date month name day first",
            "1 January 2026",
            "9 May 2026",
            Part::Whole,
        ),
        (
            "date month name, dotted",
            "01.Jan.2026",
            "09.May.2026",
            Part::Whole,
        ),
        // card numbers: every one, and across the path
        ("card number", "UFC.333", "UFC.334", Part::Whole),
        (
            "card number, second",
            "UFC.334.UFC.333",
            "UFC.334.UFC.334",
            Part::Whole,
        ),
        (
            "series number",
            "UFC.Fight.Night.293",
            "UFC.334",
            Part::Whole,
        ),
        // pairs
        (
            "pair vs, other fighters",
            "Gane.vs.Lopes",
            "Gane.vs.Hokit",
            Part::Whole,
        ),
        ("pair x", "Oliveira.x.Lopes", "Gane.x.Hokit", Part::Whole),
        ("pair v", "Rountree.v.Hokit", "Gane.v.Hokit", Part::Whole),
        (
            "pair versus",
            "Ankalaev.versus.Gane",
            "Gane.versus.Hokit",
            Part::Whole,
        ),
        (
            "pair spaced hyphen",
            "Gane - Lopes",
            "Gane - Hokit",
            Part::Whole,
        ),
        (
            "pair after technical words",
            "1080p.Oliveira.vs.Lopes",
            "1080p.Gane.vs.Hokit",
            Part::Whole,
        ),
        // parts
        ("part prelims", "Prelims", "Main.Card", Part::Main),
        ("part prelim", "Prelim", "Main.Card", Part::Main),
        (
            "part preliminary card",
            "Preliminary.Card",
            "Main.Card",
            Part::Main,
        ),
        (
            "part preliminaries",
            "Preliminaries",
            "Main.Card",
            Part::Main,
        ),
        (
            "part early prelims",
            "Early.Prelims",
            "Main.Card",
            Part::Main,
        ),
        (
            "part early preliminary card",
            "Early.Preliminary.Card",
            "Main.Card",
            Part::Main,
        ),
        (
            "part main card for whole",
            "Main.Card",
            "Full.Event",
            Part::Whole,
        ),
        (
            "part full event for main",
            "Full.Event",
            "Main.Card",
            Part::Main,
        ),
        (
            "part full card for prelims",
            "Full.Card",
            "Prelims",
            Part::Prelims,
        ),
        (
            "part complete for main",
            "Complete",
            "Main.Card",
            Part::Main,
        ),
        (
            "part main event is one fight",
            "Main.Event",
            "Main.Card",
            Part::Main,
        ),
        (
            "part two in one name",
            "Main.Card.1080p.Prelims",
            "Main.Card.1080p",
            Part::Main,
        ),
        (
            "part Spanish preliminares",
            "Preliminares",
            "Cartelera.Principal",
            Part::Main,
        ),
        (
            "part Spanish cartelera preliminar",
            "Cartelera.Preliminar",
            "Cartelera.Principal",
            Part::Main,
        ),
        (
            "part Spanish estelar for prelims",
            "Estelar",
            "Preliminares",
            Part::Prelims,
        ),
        (
            "part Portuguese card preliminar",
            "Card.Preliminar",
            "Card.Principal",
            Part::Main,
        ),
        // markers
        ("shoulder word", "Countdown", "Main.Card", Part::Main),
        (
            "press conference",
            "UFC.334.Press.Conference",
            "UFC.334.Main.Card",
            Part::Main,
        ),
        (
            "weigh in",
            "UFC.334.Weigh.In",
            "UFC.334.Main.Card",
            Part::Main,
        ),
        (
            "post fight",
            "UFC.334.Post.Fight.Show",
            "UFC.334.Main.Card",
            Part::Main,
        ),
        (
            "behind the scenes",
            "Behind.The.Scenes",
            "Main.Card",
            Part::Main,
        ),
        ("episode SxxExx", "UFC.S01E02", "UFC.334", Part::Whole),
        ("episode Sxx.Exx", "UFC.S01.E02", "UFC.334", Part::Whole),
        ("episode Sxx Exx", "UFC S01 E02", "UFC 334", Part::Whole),
        ("episode 1x02", "UFC 1x02", "UFC 334", Part::Whole),
        ("episode word", "UFC.Episode.2", "UFC.334", Part::Whole),
        ("episode ep", "UFC.Ep.2", "UFC.334", Part::Whole),
        // leagues and sports
        ("another league", "NBA.Finals", "UFC.334", Part::Whole),
        ("another sport", "Basketball", "UFC.334", Part::Whole),
    ];
    for (what, bad, good, part) in table {
        assert!(
            check(&e, &k, bad, *part).is_some(),
            "{what}: {bad} should contradict"
        );
        assert!(
            check(&e, &k, good, *part).is_none(),
            "{what}: {good} should pass: {:?}",
            check(&e, &k, good, *part)
        );
    }
    // Card numbers across the path: the components alone say nothing, the whole path says 333.
    let files = vec!["UFC/333/video.mkv".to_string()];
    assert!(matches!(
        judge_payload(&e, &k, None, &files, Part::Main),
        PayloadCheck::Contradicts(_)
    ));
    let files = vec!["UFC/334/video.mkv".to_string()];
    assert_eq!(
        judge_payload(&e, &k, None, &files, Part::Main),
        PayloadCheck::Fine
    );
    // Ambiguous day and month: it contradicts only when no reading is within a day of the event.
    let mut may = ufc();
    may.starts_at = "2026-05-09T22:00:00Z".into();
    assert!(
        check(&may, &k, "05.09.2026", Part::Whole).is_none(),
        "MDY reading"
    );
    assert!(
        check(&may, &k, "09.05.2026", Part::Whole).is_none(),
        "DMY reading"
    );
    let mut sep = ev("UFC 335: A vs. B", "ufc", "2026-09-05T22:00:00Z", None);
    sep.id = "event:ufc:2".into();
    assert!(
        check(&sep, &k, "05.09.2026", Part::Whole).is_none(),
        "ambiguous: one reading fits"
    );
    assert!(check(&sep, &k, "05.09.2027", Part::Whole).is_some());
    // The tournament's days and a golf round.
    let golf = ev(
        "KPMG Women's PGA Championship",
        "lpga-tour",
        "2026-06-25T12:00:00Z",
        Some("2026-06-28T21:00:00Z"),
    );
    for (name, part, contradicts) in [
        ("2026.06.27", Part::Round(3), false),
        ("2026.06.10", Part::Round(3), true),
        ("Round.3", Part::Round(3), false),
        ("Round.2", Part::Round(3), true),
        ("Round.2.Round.3", Part::Round(3), true),
        ("video", Part::Round(3), false),
        ("1080p", Part::Round(3), false),
        ("The.Masters", Part::Round(3), true),
        ("KPMG.Womens.PGA.Championship", Part::Round(3), false),
    ] {
        assert_eq!(
            check(&golf, &Known::default(), name, part).is_some(),
            contradicts,
            "golf {name}"
        );
    }
    // Clubs, side by side: a side that is another club contradicts, even if the other is right.
    let (g, gk) = game();
    for (name, contradicts) in [
        ("Chelsea.vs.Everton", false),
        ("Everton.v.Chelsea", false),
        ("Chelsea.vs.Fulham", true),
        ("Arsenal.v.Everton", true),
        ("Chelsea.x.Arsenal", true),
        ("Chelsea.vs.Somebody.Else", false),
    ] {
        assert_eq!(
            check(&g, &gk, name, Part::Whole).is_some(),
            contradicts,
            "club {name}"
        );
    }
    // A round of a team sport is a round of the competition, not a part.
    let mut nrl = ev("Sharks v Panthers", "nrl", "2026-05-09T09:00:00Z", None);
    nrl.round = Some("Round 12".into());
    for (name, contradicts) in [
        ("NRL.2026.Round.12", false),
        ("NRL.2026.Round.12.Sharks.vs.Panthers", false),
        ("Round.12", false),
        ("Round.13", true),
        ("Matchweek.9", true),
        ("Sharks.v.Panthers", false),
    ] {
        if arr::sport::catalog::find("nrl").is_some() {
            assert_eq!(
                check(&nrl, &Known::default(), name, Part::Whole).is_some(),
                contradicts,
                "nrl {name}"
            );
        }
    }
    nrl.round = None;
    if arr::sport::catalog::find("nrl").is_some() {
        assert!(check(&nrl, &Known::default(), "NRL.2026.Round.12", Part::Whole).is_none());
        assert!(check(&nrl, &Known::default(), "Matchweek.9", Part::Whole).is_none());
    }
}

#[test]
fn ordinary_payloads_are_accepted() {
    let (e, k) = (ufc(), fighters());
    let main = "UFC.334.Gane.vs.Hokit.Main.Card.1080p.WEB-DL.H264-DARKSPORT";
    // Names that claim nothing, and names that claim what is true.
    for (name, part) in [
        ("video", Part::Main),
        ("1080p", Part::Main),
        ("1080p-DARKSPORT", Part::Main),
        ("1080p.WEB-DL.H264-DARKSPORT", Part::Main),
        // a group tag that is a number
        ("UFC.334.Main.Card.1080p.WEB-DL-1234", Part::Main),
        ("UFC.334.Main.Card.1080p.WEB-DL.H264-2026", Part::Main),
        // given names and initials
        ("Ciryl.vs.Josh", Part::Main),
        ("C.Gane.vs.J.Hokit", Part::Main),
        ("Gane.vs.Hokit", Part::Main),
        ("Hokit.v.Gane", Part::Main),
        // a labelled checksum, an id in brackets, a hex run
        ("video.CRC32.20240101", Part::Main),
        ("video.[CRC32.20240101]", Part::Main),
        ("video.[ABCD1234]", Part::Main),
        ("video.0123456789abcdef", Part::Main),
        ("video.MD5.20231231", Part::Main),
        // a lone year; a year in a resolution or codec token
        ("UFC.334.2026", Part::Main),
        ("UFC.334.Main.Card.2160p.HEVC.DDP5.1.Atmos", Part::Main),
        // the dates that agree
        ("UFC.334.2026.05.09.Main.Card", Part::Main),
        ("UFC.334.20260509", Part::Main),
        ("UFC.334.09.05.2026", Part::Main),
        ("UFC.334.9.May.2026", Part::Main),
        // each part by its own name
        ("UFC.334.Prelims.1080p.WEB-DL.H264.Fight-BB", Part::Prelims),
        (
            "UFC.334.Early.Prelims.720p.WEB-DL.H264.Fight-BB",
            Part::EarlyPrelims,
        ),
        ("UFC.334.Preliminary.Card.1080p", Part::Prelims),
        ("UFC.334.Full.Event.1080p", Part::Whole),
        ("UFC.334.Cartelera.Principal", Part::Main),
        (main, Part::Main),
    ] {
        assert!(
            check(&e, &k, name, part).is_none(),
            "{name}: {:?}",
            check(&e, &k, name, part)
        );
    }
    // Whole payloads: a typical scene folder and file, the extras, the torrent name repeated in
    // the file name, a file named with a resolution and a group only.
    let ok = |name: Option<&str>, files: &[&str], part: Part| {
        let files: Vec<String> = files.iter().map(|f| f.to_string()).collect();
        judge_payload(&e, &k, name, &files, part)
    };
    assert_eq!(
        ok(
            Some(main),
            &[
                &format!("{main}/{main}.mkv"),
                &format!("{main}/{main}.nfo"),
                &format!("{main}/{main}.sfv"),
                &format!("{main}/{main}.srt"),
                &format!("{main}/Proof/proof.jpg"),
                &format!("{main}/readme.txt"),
            ],
            Part::Main
        ),
        PayloadCheck::Fine
    );
    assert_eq!(
        ok(Some(main), &[&format!("{main}.mkv")], Part::Main),
        PayloadCheck::Fine
    );
    assert_eq!(
        ok(Some(main), &["1080p-DARKSPORT.mkv"], Part::Main),
        PayloadCheck::Fine
    );
    // A team sport: the competition's round in a folder.
    let (g, gk) = game();
    let files = vec!["Premier.League.2026.Round.12/Chelsea.vs.Everton.1080p.mkv".to_string()];
    assert_eq!(
        judge_payload(&g, &gk, None, &files, Part::Whole),
        PayloadCheck::Fine
    );
    // A second video of any container is a second video.
    for extra in [
        "bonus.flv",
        "Sample/sample.mkv",
        ".hidden/x.mkv",
        "d.iso",
        "a/b/c.avi",
    ] {
        assert_eq!(
            ok(Some(main), &["video.mkv", extra], Part::Main),
            PayloadCheck::SeveralVideos,
            "{extra}"
        );
    }
}

#[test]
fn the_reasons_a_person_sees_are_rendered_from_typed_values() {
    use arr::sport::reason::Reason;
    let golf = ev(
        "KPMG Women's PGA Championship",
        "lpga-tour",
        "2026-06-25T12:00:00Z",
        Some("2026-06-28T21:00:00Z"),
    );
    // A wrong golf round keeps its reason (it used to become the generic sentence).
    let r = payload_claims(&golf, &Known::default(), "Round.2", Part::Round(3)).unwrap();
    assert_eq!(r.render(), "the download says it is round 2, not round 3");
    // The common failures, as a person reads them.
    let (e, k) = (ufc(), fighters());
    assert_eq!(
        payload_claims(&e, &k, "UFC.333", Part::Main)
            .unwrap()
            .render(),
        "the download's own names are not this event (another card number)"
    );
    assert_eq!(
        payload_claims(&e, &k, "Prelims", Part::Main)
            .unwrap()
            .render(),
        "the download says it is the prelims, not the main card"
    );
    assert_eq!(
        Reason::SeveralVideos.render(),
        "more than one video file; this copy needs picking apart by hand"
    );
    assert_eq!(
        Reason::AlreadyHave.render(),
        "Arr already has a file for this part of the event; nothing is overwritten"
    );
    assert!(
        Reason::NoSpace {
            free_mib: 100,
            needed_mib: 4096,
            min_mib: 0
        }
        .render()
        .starts_with("not enough free space in the library")
    );
    assert!(
        Reason::ClientSignIn
            .render()
            .contains("sign in to the download client")
    );
    assert!(
        Reason::ClientUnreachable
            .render()
            .contains("could not reach the download client")
    );
    assert!(
        Reason::ClientRefused
            .render()
            .contains("refused this torrent")
    );
    assert!(Reason::ClientNoSpace.render().contains("no space"));
}

// ------------------------------------------------------------------------------ round 5

#[test]
fn a_year_anywhere_in_a_date_and_a_season_that_is_not_one() {
    let (e, k) = (ufc(), fighters());
    // The 9 May 2026 card: the year first, in the middle and last, with every separator and
    // compact; a different day contradicts, the right day passes.
    for (bad, good) in [
        ("01.2026.01", "09.2026.05"),
        ("01-2026-01", "09-2026-05"),
        ("01_2026_01", "05_2026_09"),
        ("01 2026 01", "09 2026 05"),
        ("01202601", "09202605"),
        ("20260101", "20260509"),
        ("20260901", "20260905"),
        ("01012026", "09052026"),
    ] {
        assert!(check(&e, &k, bad, Part::Whole).is_some(), "{bad}");
        assert!(
            check(&e, &k, good, Part::Whole).is_none(),
            "{good}: {:?}",
            check(&e, &k, good, Part::Whole)
        );
    }
    // A season span is never a date: `2026-27` followed by a day and a month is the season, then
    // a date of its own.
    for name in ["2026-27", "2026-2027", "2026.27", "UFC.2026-27.09.05.2026"] {
        assert!(
            check(&e, &k, name, Part::Whole).is_none(),
            "{name}: {:?}",
            check(&e, &k, name, Part::Whole)
        );
    }
    assert!(check(&e, &k, "2026-27.01.01.2026", Part::Whole).is_some());
}

#[test]
fn only_a_surname_identifies_a_fighter() {
    let e = ufc();
    // Another card of the league: "Josh Emmett vs. Kevin Vallejos".
    let k = Known {
        teams: vec![],
        others: vec![vec!["emmett".into(), "vallejos".into()]],
    };
    // Given names are never a contradiction.
    for name in [
        "Ciryl.vs.Josh",
        "Josh.vs.Kevin",
        "Ciryl.Gane.vs.Josh",
        "C.Gane.vs.Kevin.Vallej",
    ] {
        assert!(
            check(&e, &k, name, Part::Whole).is_none(),
            "{name}: {:?}",
            check(&e, &k, name, Part::Whole)
        );
    }
    // A surname of another stored fighter is, even with the right other side.
    for name in ["Gane.vs.Emmett", "Vallejos.v.Hokit", "Josh.Emmett.vs.Hokit"] {
        assert!(check(&e, &k, name, Part::Whole).is_some(), "{name}");
    }
    // A surname of this event's own is fine.
    let own = Known {
        teams: vec![],
        others: vec![vec!["gane".into(), "other".into()]],
    };
    assert!(check(&e, &own, "Gane.vs.Hokit", Part::Whole).is_none());
    // The surnames the code takes from a name.
    use arr::sport::payload::surnames_of;
    assert_eq!(
        surnames_of("UFC Fight Night: Josh Emmett vs. Kevin Vallejos"),
        ["emmett", "vallejos"]
    );
    assert_eq!(
        surnames_of("UFC 300: Jon Jones Jr vs. Stipe Miocic"),
        ["jones", "miocic"]
    );
    assert_eq!(
        surnames_of("UFC 301: Pantoja vs. Royval"),
        ["pantoja", "royval"]
    );
}

#[test]
fn a_side_is_cleaned_of_what_is_not_a_name_before_it_is_resolved() {
    let (g, gk) = game();
    // Chelsea v Everton: a wrong team behind a league prefix, a date prefix, a round prefix or a
    // technical suffix, on either side, still contradicts; the right ones pass.
    for (name, contradicts) in [
        ("EPL.Arsenal.vs.Everton.1080p", true),
        ("Premier.League.Arsenal.v.Everton", true),
        ("Chelsea.vs.EPL.Fulham", true),
        ("2026.11.07.Arsenal.vs.Everton", true),
        ("Round.12.Arsenal.vs.Everton", true),
        ("Arsenal.vs.Everton.Round.12", true),
        ("Chelsea.vs.Arsenal.1080p.WEB-DL", true),
        ("Chelsea.vs.Arsenal.Group.H", true),
        ("Group.H.Arsenal.vs.Everton", true),
        ("EPL.Chelsea.vs.Everton.1080p", false),
        ("2026.11.07.Chelsea.vs.Everton", false),
        ("Round.12.Chelsea.vs.Everton", false),
        ("Chelsea.vs.Everton.Round.12", false),
        ("Group.H.Chelsea.vs.Everton.1080p.WEB-DL", false),
    ] {
        assert_eq!(
            check(&g, &gk, name, Part::Whole).is_some(),
            contradicts,
            "{name}: {:?}",
            check(&g, &gk, name, Part::Whole)
        );
    }
}

#[test]
fn components_are_hard_boundaries_and_every_folder_is_judged() {
    let g = {
        let mut e = ev(
            "Chelsea v Everton",
            "premier-league",
            "2026-11-07T15:00:00Z",
            None,
        );
        e.home_id = Some("1".into());
        e.home_name = Some("Chelsea".into());
        e.away_id = Some("2".into());
        e.away_name = Some("Everton".into());
        e
    };
    let (_, gk) = game();
    let ok = |f: &str| judge_payload(&g, &gk, None, &[f.to_string()], Part::Whole);
    // A season folder and a dated file: no date is invented across the boundary.
    assert_eq!(
        ok("EPL/2026-27/07.11.2026.Chelsea.vs.Everton.1080p.mkv"),
        PayloadCheck::Fine
    );
    assert_eq!(
        ok("EPL/2026/27/07.11.2026.Chelsea.vs.Everton.mkv"),
        PayloadCheck::Fine
    );
    // A folder that holds only text files is still judged (a card number, a part, a date).
    let e = ufc();
    let k = fighters();
    let files = vec!["video.mkv".to_string(), "UFC.333/readme.nfo".to_string()];
    assert!(matches!(
        judge_payload(&e, &k, Some("UFC.334.Main.Card"), &files, Part::Main),
        PayloadCheck::Contradicts(_)
    ));
    let files = vec!["video.mkv".to_string(), "Prelims/notes.txt".to_string()];
    assert!(matches!(
        judge_payload(&e, &k, None, &files, Part::Main),
        PayloadCheck::Contradicts(_)
    ));
    // A text file's own name is not judged (only a video's).
    let files = vec!["video.mkv".to_string(), "UFC.333.nfo".to_string()];
    assert_eq!(
        judge_payload(&e, &k, None, &files, Part::Main),
        PayloadCheck::Fine
    );
    // The one reading across a boundary: a league word and a number that is the whole of the next.
    assert!(matches!(
        judge_payload(&e, &k, None, &["UFC/333/video.mkv".to_string()], Part::Main),
        PayloadCheck::Contradicts(_)
    ));
    assert_eq!(
        judge_payload(&e, &k, None, &["UFC/334/video.mkv".to_string()], Part::Main),
        PayloadCheck::Fine
    );
}

#[test]
fn every_alias_of_a_major_takes_part_in_the_golf_check() {
    // Round 1 of the Masters picked: another major under any of its names contradicts.
    let masters = ev(
        "Masters Tournament",
        "pga-tour",
        "2026-04-09T12:00:00Z",
        Some("2026-04-12T21:00:00Z"),
    );
    let k = Known::default();
    for other in [
        "The.Open.2026.Round.1",
        "Open.Championship.Round.1",
        "British.Open.Round.1",
        "The.Open.Championship.Round.1",
        "US.Open.Round.1",
        "U.S.Open.Round.1",
        "U.S..Open.Round.1",
        "PGA.Championship.Round.1",
    ] {
        assert!(
            check(&masters, &k, other, Part::Round(1)).is_some(),
            "{other}"
        );
    }
    for same in [
        "The.Masters.Round.1",
        "Masters.Tournament.Round.1",
        "Masters.Round.1",
        "Round.1",
    ] {
        assert!(
            check(&masters, &k, same, Part::Round(1)).is_none(),
            "{same}: {:?}",
            check(&masters, &k, same, Part::Round(1))
        );
    }
    // And the reverse: The Open picked, the Masters contradicts, The Open in any form passes.
    let open = ev(
        "The Open Championship",
        "pga-tour",
        "2026-07-16T12:00:00Z",
        Some("2026-07-19T21:00:00Z"),
    );
    assert!(check(&open, &k, "The.Masters.Round.1", Part::Round(1)).is_some());
    for same in [
        "The.Open.Round.1",
        "British.Open.Round.1",
        "Open.Championship.Round.1",
    ] {
        assert!(check(&open, &k, same, Part::Round(1)).is_none(), "{same}");
    }
}

// ------------------------------------------------------------------------------ round 6

#[test]
fn a_rematch_ordinal_and_a_suffix_are_not_the_surname() {
    use arr::sport::payload::{participant_words, surnames_of};
    // The surname is the last word once a trailing ordinal or suffix is off.
    assert_eq!(
        surnames_of("PFL Austin: Eblen vs. Kasanganay 2"),
        ["eblen", "kasanganay"]
    );
    assert_eq!(
        surnames_of("PFL Austin: Eblen vs. Kasanganay II"),
        ["eblen", "kasanganay"]
    );
    assert_eq!(
        surnames_of("PFL Austin: Eblen vs. Kasanganay 3"),
        ["eblen", "kasanganay"]
    );
    assert_eq!(
        surnames_of("UFC 300: Alexander Volkanovski Jr vs. Ilia Topuria"),
        ["volkanovski", "topuria"]
    );
    // A multi-word surname is read by its last word; the event's own fighters pass by any word.
    assert_eq!(
        surnames_of("UFC 1: Junior dos Santos vs. Rodrigo de la Rosa"),
        ["santos", "rosa"]
    );
    assert_eq!(
        surnames_of("UFC 2: Joao Cortes Acosta vs. Someone Else"),
        ["acosta", "else"]
    );
    assert!(
        participant_words("UFC 1: Junior dos Santos vs. Cain Velasquez")
            .contains(&"dos".to_string())
    );

    let e = ev(
        "PFL Austin: Eblen vs. Kasanganay 2",
        "ufc",
        "2026-05-09T22:00:00Z",
        None,
    );
    let neighbours = Known {
        teams: vec![],
        others: vec![
            surnames_of("PFL Austin: Kasanganay vs. Someone"),
            surnames_of("PFL Houston: Eblen vs. Another 3"),
        ],
    };
    for name in [
        "PFL.Austin.Eblen.vs.Kasanganay.2.Full.Event.720p-NX",
        "PFL.Austin.Eblen.vs.Kasanganay.II.Full.Event.720p-NX",
        "PFL.Austin.Eblen.vs.Kasanganay.3.720p-NX",
        "Eblen.vs.Kasanganay.Jr",
        "Kasanganay.2.vs.Eblen",
    ] {
        assert!(
            check(&e, &neighbours, name, Part::Whole).is_none(),
            "{name}: {:?}",
            check(&e, &neighbours, name, Part::Whole)
        );
    }
    // Another event's fighter is still another fighter.
    assert!(check(&e, &neighbours, "Eblen.vs.Someone.2", Part::Whole).is_some());
    // A Jr fighter and a multi-word surname of the picked event always pass.
    let jr = ev(
        "UFC 300: Alexander Volkanovski Jr vs. Ilia Topuria",
        "ufc",
        "2026-05-09T22:00:00Z",
        None,
    );
    let k = Known {
        teams: vec![],
        others: vec![surnames_of("UFC 299: Volkanovski vs. Other")],
    };
    assert!(check(&jr, &k, "Volkanovski.Jr.vs.Topuria", Part::Whole).is_none());
    assert!(check(&jr, &k, "Volkanovski.vs.Topuria", Part::Whole).is_none());
    let dos = ev(
        "UFC 1: Junior dos Santos vs. Cain Velasquez",
        "ufc",
        "2026-05-09T22:00:00Z",
        None,
    );
    let k = Known {
        teams: vec![],
        others: vec![surnames_of("UFC 2: Santos vs. Other")],
    };
    assert!(check(&dos, &k, "Dos.Santos.vs.Velasquez", Part::Whole).is_none());
    assert!(check(&dos, &k, "Santos.vs.Velasquez", Part::Whole).is_none());
}
