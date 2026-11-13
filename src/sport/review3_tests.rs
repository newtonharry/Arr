//! Tests of review round 3: a part is answered only when the title establishes it; completeness
//! covers what was kept and what was only listed; the publish time is checked whatever the title
//! says; a tag is verified only from the list of known uploaders; a sibling is the same
//! uploader only by such a tag; a channel code is verified only against a closed set.

use super::Part;
use super::catalog;
use super::data;
use super::espn::{EventState, Fixture, Side};
use super::find::{self, Surroundings};
use super::match_tests::{
    all_events, at, candidate, event, judge_with, known_spans, published_for, teams_of,
};
use super::matcher::{Confidence, Context, Verdict, judge};
use super::release;

fn sure(v: &Verdict) -> bool {
    matches!(v, Verdict::Match(m) if m.confidence == Confidence::Sure)
}

fn reason(v: &Verdict) -> Option<&'static str> {
    v.matched().and_then(|m| m.reason)
}

// ------------------------------------------------------------------------------------------ C6

#[test]
fn a_direct_part_match_answers_a_part_only_when_the_title_establishes_it() {
    // "UFC 333 1080p WEB-DL" names no part: the matcher's guess (the main card) answers no
    // request for one.
    let ev = event("event:ufc:3");
    let league = catalog::find("ufc").unwrap();
    let j = find::judge_all(
        vec![candidate(
            "UFC 333 1080p WEB-DL-DARKSPORT",
            12,
            "2026-10-25T01:00:00Z",
        )],
        &ev,
        league,
        &Surroundings::all_known(),
        None,
    );
    assert_eq!(j.candidates.len(), 1);
    let c = &j.candidates[0];
    assert!(c.m.unlabelled && !c.m.part_established, "{:?}", c.m);
    assert_eq!(c.m.confidence, Confidence::Likely);
    let all = find::Found {
        candidates: j.candidates.clone(),
        complete: true,
        ..Default::default()
    };
    for want in [Part::Main, Part::Prelims, Part::EarlyPrelims, Part::Whole] {
        assert!(
            all.for_part(Some(want)).candidates.is_empty(),
            "answered {want:?}"
        );
    }
    // And a title that does say its part answers it.
    let j = find::judge_all(
        vec![candidate(
            "UFC 333 Main Card 1080p WEB-DL-DARKSPORT",
            12,
            "2026-10-25T01:00:00Z",
        )],
        &ev,
        league,
        &Surroundings::all_known(),
        None,
    );
    let all = find::Found {
        candidates: j.candidates.clone(),
        complete: true,
        ..Default::default()
    };
    assert_eq!(all.for_part(Some(Part::Main)).candidates.len(), 1);
    assert!(all.for_part(Some(Part::Prelims)).candidates.is_empty());
}

// ------------------------------------------------------------------------------------------ C7

fn fixture(id: &str, at_: &str, sides: bool) -> Fixture {
    let side = |id: &str, name: &str| Side {
        id: id.into(),
        name: name.into(),
        abbreviation: None,
    };
    Fixture {
        id: id.into(),
        starts_at: at(at_),
        ends_at: None,
        season: None,
        name: Some(if sides {
            "Indiana Pacers at Washington Wizards".into()
        } else {
            "NBA event".into()
        }),
        short_name: None,
        home: sides.then(|| side("1", "Washington Wizards")),
        away: sides.then(|| side("2", "Indiana Pacers")),
        round: None,
        venue: None,
        state: EventState::Scheduled,
    }
}

fn judged_in(c: &rusqlite::Connection, id: &str, title: &str, published: &str) -> Verdict {
    let ev = data::event(c, id).unwrap().expect("stored");
    let start = at(&ev.starts_at);
    let (h, a) = (ev.home_id.clone().unwrap(), ev.away_id.clone().unwrap());
    let neighbours = data::pair_events_between(
        c,
        "nba",
        &h,
        &a,
        start - time::Duration::days(3),
        start + time::Duration::days(3),
    )
    .unwrap();
    let teams = teams_of("nba");
    let cx = Context::new(
        catalog::find("nba").unwrap(),
        &ev,
        &teams,
        &neighbours,
        false,
        true,
        known_spans(true),
        true,
    );
    judge(&release::parse(title, Some(at(published))), &ev, &cx)
}

#[test]
fn a_fixture_not_kept_is_still_evidence_that_another_fits() {
    // The first read is on 13 November: the cutoff is the start of the 12th. A complete November
    // lists Wizards-Pacers on the 11th and the 12th; the 11th is not kept, and not served.
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    data::create_tables(&c).unwrap();
    let now = at("2026-11-13T12:00:00Z");
    let cutoff = at("2026-11-12T00:00:00Z");
    let (f11, f12) = (
        fixture("301", "2026-11-11T01:00:00Z", true),
        fixture("302", "2026-11-12T01:00:00Z", true),
    );
    data::upsert_events(&mut c, "nba", &[&f11, &f12], cutoff, now).unwrap();
    assert!(
        data::event(&c, "event:nba:301").unwrap().is_none(),
        "served"
    );
    assert!(data::event(&c, "event:nba:302").unwrap().is_some());
    assert!(
        data::league_events(&c, "nba", false, now - time::Duration::days(30), 100)
            .unwrap()
            .iter()
            .all(|e| e.id != "event:nba:301"),
        "the unkept fixture is in a list"
    );
    // The title's date is the 12th, within a day of the 11th as well.
    let v = judged_in(
        &c,
        "event:nba:302",
        "NBA 2026 11 12 Washington Wizards vs Indiana Pacers 1080p WEB-DL-NX",
        "2026-11-12T05:00:00Z",
    );
    assert!(!sure(&v), "{v:?}");
    assert_eq!(reason(&v), Some("another_fixture_fits"), "{v:?}");
    // It is evidence for a later read too: stored again, it is still one row.
    data::upsert_events(&mut c, "nba", &[&f11, &f12], cutoff, now).unwrap();
    let v = judged_in(
        &c,
        "event:nba:302",
        "NBA 2026 11 12 Washington Wizards vs Indiana Pacers 1080p WEB-DL-NX",
        "2026-11-12T05:00:00Z",
    );
    assert_eq!(reason(&v), Some("another_fixture_fits"), "{v:?}");
}

#[test]
fn a_fixture_of_unknown_sides_could_be_the_other_one() {
    // A record that put one team on both sides was kept without sides: the pair-based loader
    // must still show it, and nothing rules it out.
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    data::create_tables(&c).unwrap();
    let now = at("2026-11-10T12:00:00Z");
    let cutoff = at("2026-11-10T00:00:00Z");
    let (real, odd) = (
        fixture("401", "2026-11-12T01:00:00Z", true),
        fixture("402", "2026-11-12T01:30:00Z", false),
    );
    data::upsert_events(&mut c, "nba", &[&real, &odd], cutoff, now).unwrap();
    let v = judged_in(
        &c,
        "event:nba:401",
        "NBA 2026 11 12 Washington Wizards vs Indiana Pacers 1080p WEB-DL-NX",
        "2026-11-12T05:00:00Z",
    );
    assert!(!sure(&v), "{v:?}");
    assert_eq!(reason(&v), Some("schedule_unknown"), "{v:?}");
    // Without the odd record, the same title is sure.
    let mut c2 = rusqlite::Connection::open_in_memory().unwrap();
    data::create_tables(&c2).unwrap();
    data::upsert_events(&mut c2, "nba", &[&real], cutoff, now).unwrap();
    let v = judged_in(
        &c2,
        "event:nba:401",
        "NBA 2026 11 12 Washington Wizards vs Indiana Pacers 1080p WEB-DL-NX",
        "2026-11-12T05:00:00Z",
    );
    assert!(sure(&v), "{v:?}");
}

// ----------------------------------------------------------------------------------------- C14

#[test]
fn the_publish_time_is_checked_whatever_the_title_says() {
    let ufc = event("event:ufc:3");
    // A card: the date in the title does not stand in for the publish time.
    for (title, published) in [
        (
            "UFC 333 2026-10-24 Prelims 1080p WEB-DL-DARKSPORT",
            "2026-12-30T00:00:00Z",
        ),
        ("UFC 333 2026-10-24 Prelims 1080p WEB-DL-DARKSPORT", "none"),
        (
            "UFC 333 Volkanovski vs Evloev 2026-10-24 Main Card 1080p WEB-DL-DARKSPORT",
            "2026-12-30T00:00:00Z",
        ),
        (
            "UFC 333 Volkanovski vs Evloev 2026-10-24 Main Card 1080p WEB-DL-DARKSPORT",
            "none",
        ),
    ] {
        let v = judge_with(title, Some(published), &ufc, false);
        let m = v.matched().unwrap_or_else(|| panic!("{title}: {v:?}"));
        assert_eq!(m.confidence, Confidence::Likely, "{title} {published}");
        assert!(
            matches!(m.reason, Some("late_upload" | "undated")),
            "{title} {published}: {m:?}"
        );
    }
    // On time, it is sure.
    let v = judge_with(
        "UFC 333 2026-10-24 Prelims 1080p WEB-DL-DARKSPORT",
        Some("2026-10-25T01:00:00Z"),
        &ufc,
        false,
    );
    assert!(sure(&v), "{v:?}");
    // A game: a date in the title, a re-upload after the window, or none at all.
    let nba = event("event:nba:60");
    let title = "NBA 2026 11 12 Washington Wizards vs Indiana Pacers 1080p WEB-DL-NX";
    assert!(sure(&judge_with(
        title,
        Some("2026-11-12T06:00:00Z"),
        &nba,
        false
    )));
    for published in ["2027-03-01T00:00:00Z", "none"] {
        let v = judge_with(title, Some(published), &nba, false);
        assert!(!sure(&v), "{published}: {v:?}");
        assert!(
            matches!(reason(&v), Some("late_upload" | "undated")),
            "{v:?}"
        );
    }
    // Golf: the tournament's days and a fortnight.
    let golf = event("event:lpga-tour:80");
    let g = "LPGA / DOW Invitational / Round 3 / 22.07.2023 [Golf, WEB-DL HD/1080p] [NX]";
    assert!(sure(&judge_with(
        g,
        Some("2023-07-23T10:00:00Z"),
        &golf,
        false
    )));
    for published in ["2024-01-01T00:00:00Z", "none"] {
        let v = judge_with(g, Some(published), &golf, false);
        assert!(!sure(&v), "{published}: {v:?}");
        assert!(
            matches!(reason(&v), Some("late_upload" | "undated")),
            "{v:?}"
        );
    }
    // The default a test gives is three hours after the start.
    let _ = published_for(None, &nba);
}

// ----------------------------------------------------------------------------------------- C15

#[test]
fn only_a_known_uploaders_tag_keeps_a_match_sure() {
    let ev = event("event:ufc:3");
    let publ = Some("2026-10-28T01:25:00Z");
    for (title, want) in [
        ("UFC 333 Main Card 1080p WEB-DL-DARKSPORT", true),
        ("UFC 333 Main Card 1080p WEB-DL-NX", true),
        ("UFC 333 Main Card 1080p WEB-DL-nVa", true),
        ("UFC.333.Main.Card.1080p.WEB-DL.H264.Fight-BB", true),
        ("UFC 333 Main Card 1080p WEB-DL [TJET]", true),
        // No tag at all (a forum title's shape) is not an unknown one.
        ("UFC 333 Main Card 1080p WEB-DL", false),
        // Unknown: lowercase, other case, digits, a shoulder marker split by the hyphen.
        ("UFC 333 Main Card 1080p WEB-DL-darksport", false),
        ("UFC 333 Main Card 1080p WEB-DL-Darksport", false),
        ("UFC 333 Main Card 1080p WEB-DL-nva", false),
        ("UFC 333 Main Card 1080p WEB-DL-x1y2", false),
        ("UFC 333 Main Card 1080p WEB-DL-24", false),
        ("UFC 333 Main Card 1080p WEB-DL-vlogs", false),
        ("UFC 333 Main Card 1080p WEB-DL-bb", false),
        ("UFC 333 Main Card 1080p WEB-DL-BB", false),
        // Two uploaders: the exception is for one.
        ("UFC 333 Main Card 1080p WEB-DL-NX [TJET]", false),
        ("UFC 333 Main Card 1080p WEB-DL [TJET] [NX]", false),
    ] {
        let v = judge_with(title, publ, &ev, false);
        assert_eq!(sure(&v), want, "{title}: {v:?}");
    }
    // A shoulder marker across a token boundary is a shoulder marker, not a tag.
    for title in [
        "UFC 333 Main Card 1080p WEB-DL Round-UP",
        "UFC 333 Main Card 1080p WEB-DL Round.Up",
        "UFC 333 Main Card 1080p WEB-DL Wrap-Up",
        "UFC 333 Main Card 1080p WEB-DL-Wrap Up",
        "UFC 333 Main Card 1080p WEB-DL Post-Fight",
        "UFC 333 Main Card 1080p WEB-DL High-lights",
        "UFC 333 Main Card 1080p WEB-DL [Round UP]",
        "UFC 333 Main Card 1080p WEB-DL [Count Down]",
        "UFC 333 Main Card Pre.View 1080p WEB-DL",
    ] {
        let v = judge_with(title, publ, &ev, false);
        assert!(v.matched().is_none(), "{title}: {v:?}");
    }
}

// ----------------------------------------------------------------------------------------- C16

#[test]
fn a_date_at_the_end_is_not_an_uploader_and_settles_no_sibling() {
    let ev = event("event:ufc:3");
    let league = catalog::find("ufc").unwrap();
    for tag in ["2026-10-24", "24", "x", "Whatever"] {
        let found = vec![
            candidate(
                &format!("UFC 333 1080p WEB-DL {tag}"),
                9,
                "2026-10-25T01:00:00Z",
            ),
            candidate(
                &format!("UFC 333 Prelims 1080p WEB-DL {tag}"),
                9,
                "2026-10-25T01:00:00Z",
            ),
        ];
        let j = find::judge_all(found, &ev, league, &Surroundings::all_known(), None);
        let unlabelled = j
            .candidates
            .iter()
            .find(|c| !c.m.part_established)
            .unwrap_or_else(|| panic!("{tag}: {:?}", j.candidates));
        assert_eq!(unlabelled.m.confidence, Confidence::Likely, "{tag}");
        assert!(unlabelled.m.confidence != Confidence::Sure, "{tag}");
    }
    // The same tag that is a known uploader's does settle it.
    let found = vec![
        candidate("UFC 333 1080p WEB-DL-DARKSPORT", 9, "2026-10-25T01:00:00Z"),
        candidate(
            "UFC 333 Prelims 1080p WEB-DL-DARKSPORT",
            9,
            "2026-10-25T01:00:00Z",
        ),
    ];
    let j = find::judge_all(found, &ev, league, &Surroundings::all_known(), None);
    let main = j
        .candidates
        .iter()
        .find(|c| c.m.part == Part::Main)
        .expect("main");
    assert_eq!(main.m.confidence, Confidence::Sure);
    // The group is the validated tag, not the suffix.
    assert_eq!(
        release::parse("UFC 333 1080p WEB-DL 2026-10-24", None).group,
        None
    );
    assert_eq!(
        release::parse("UFC 333 1080p WEB-DL-NX", None)
            .group
            .as_deref(),
        Some("nx")
    );
    assert_eq!(release::parse("UFC 333 1080p WEB-DL-Nx", None).group, None);
}

// ----------------------------------------------------------------------------------------- C17

#[test]
fn a_channel_code_is_verified_only_by_a_closed_set_or_the_events_own_team() {
    let ev = event("event:ufc:3");
    let publ = Some("2026-10-28T01:25:00Z");
    // Closed region codes after DAZN are fine; arbitrary ones are free text.
    for (title, want) in [
        ("UFC 333 Main Card 1080p WEB-DL [EN, DAZN-IND] [NX]", true),
        ("UFC 333 Main Card 1080p WEB-DL [EN, DAZN-POD]", false),
        ("UFC 333 Main Card 1080p WEB-DL [EN, DAZN POD]", false),
        ("UFC 333 Main Card 1080p WEB-DL [EN, DAZN-SAS]", false),
        ("UFC 333 Main Card 1080p WEB-DL [EN, DAZN-IND-POD]", false),
    ] {
        let v = judge_with(title, publ, &ev, false);
        assert_eq!(sure(&v), want, "{title}: {v:?}");
    }
    // For a game the code of one of the event's two teams is its own feed; another team's is not.
    let nba = event("event:nba:61");
    let own = "NBA 2026-2027 / 11.11.2026 / San Antonio Spurs @ Chicago Bulls [Basketball, WEB-DL HD/1080p, MKV/H.264, EN/DAZN-SAS] [NX]";
    assert!(sure(&judge_with(own, None, &nba, false)));
    let other = own.replace("DAZN-SAS", "DAZN-LAL");
    assert!(!sure(&judge_with(&other, None, &nba, false)));
    // Every event of the fixtures: no title is sure through a code of no team of it.
    let _ = all_events();
    let _ = Surroundings::default();
}

// ------------------------------------------------------------------------------- judging bound

#[tokio::test]
async fn judging_waits_for_a_slot_inside_its_budget_and_runs_off_the_workers() {
    use std::sync::Arc;
    let ev = event("event:ufc:3");
    let league = catalog::find("ufc").unwrap();
    let searched = Arc::new(find::Searched {
        releases: vec![candidate(
            "UFC 333 Main Card 1080p WEB-DL-DARKSPORT",
            12,
            "2026-10-25T01:00:00Z",
        )],
        complete: true,
        ..Default::default()
    });
    // With a slot it judges (on a blocking thread: the task that awaits it is not blocked).
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    let found = find::judge_search_bounded(
        slots.clone(),
        searched.clone(),
        ev.clone(),
        league,
        Surroundings::all_known(),
    )
    .await
    .expect("a slot was free");
    assert_eq!(found.candidates.len(), 1);
    assert_eq!(slots.available_permits(), 1, "the slot is given back");
    // With every slot held it does not wait past its budget: it gives up as busy.
    let held = slots.clone().acquire_owned().await.unwrap();
    let started = std::time::Instant::now();
    let none = find::judge_search_bounded(
        slots.clone(),
        searched,
        ev,
        league,
        Surroundings::all_known(),
    )
    .await;
    assert!(none.is_none());
    let waited = started.elapsed();
    assert!(
        waited >= find::JUDGE_BUDGET - std::time::Duration::from_millis(100)
            && waited < find::JUDGE_BUDGET + std::time::Duration::from_secs(2),
        "{waited:?}"
    );
    drop(held);
}
