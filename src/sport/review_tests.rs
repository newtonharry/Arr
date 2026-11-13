//! Tests of the classes of mistake the first review found, rather than of single titles: one
//! release lent evidence by another, a fixture judged alone, words that hide from the checks, an
//! unbounded title or result list, and the limits on how often the indexers are asked. The
//! single titles are rows of the table in `match_tests`.

use super::Part;
use super::catalog;
use super::data::EventRow;
use super::find::{self, Found, Searched, Surroundings};
use super::gate::{CopiesGate, MAX_SLOTS, Served};
use super::match_tests::{
    Want, all_events, at, candidate, event, judge_known, judge_with, known_spans, published_for,
    table, teams_of,
};
use super::matcher::{Confidence, Context, Match, RefusalCode, Verdict, judge, settle_unlabelled};
use super::release;
use crate::import::rfc3339;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use time::Duration as TDuration;

fn judge_among(
    title: &str,
    published: Option<&str>,
    ev: &EventRow,
    events: &[EventRow],
) -> Verdict {
    let parsed = release::parse(title, published_for(published, ev));
    let teams = teams_of(&ev.league);
    let cx = Context::new(
        catalog::find(&ev.league).unwrap(),
        ev,
        &teams,
        events,
        false,
        true,
        known_spans(true),
        true,
    );
    judge(&parsed, ev, &cx)
}

// ------------------------------------------------------------------------------------------ C1

/// Why the first "no release is sure of two events" test missed consecutive fixtures: its
/// fixture set had no pair of events with the same two teams close together, and every title it
/// judged was written from one event's own date. This one makes the pairs: the same two teams
/// again after a day (a series), after a day and a bit, and after two or three days; the home
/// side reversed or not; the pair's first game at each hour around the turn of the UTC day; and
/// a title dated each day from before the first to after the second.
#[test]
fn back_to_back_fixtures_are_never_both_sure() {
    for base_id in ["event:nba:62", "event:premier-league:33", "event:nrl:100"] {
        let base = event(base_id);
        let league = catalog::find(&base.league).unwrap();
        for hour in [0i64, 1, 8, 15, 23] {
            for gap in [24i64, 26, 47, 48, 72] {
                for reversed in [false, true] {
                    let start = at("2026-11-12T00:00:00Z") + TDuration::hours(hour);
                    let mut a = base.clone();
                    a.id = format!("event:{}:9001", base.league);
                    a.round = None;
                    a.starts_at = rfc3339(start);
                    let mut b = a.clone();
                    b.id = format!("event:{}:9002", base.league);
                    b.starts_at = rfc3339(start + TDuration::hours(gap));
                    if reversed {
                        std::mem::swap(&mut b.home_id, &mut b.away_id);
                        std::mem::swap(&mut b.home_name, &mut b.away_name);
                    }
                    let both = [a.clone(), b.clone()];
                    for offset in -3i64..=4 {
                        let d = (start + TDuration::days(offset)).date();
                        let title = format!(
                            "{} {:04} {:02} {:02} {} vs {} 1080p WEB-DL",
                            league.aliases[0],
                            d.year(),
                            u8::from(d.month()),
                            d.day(),
                            a.home_name.as_deref().unwrap(),
                            a.away_name.as_deref().unwrap()
                        );
                        let verdicts: Vec<Verdict> = both
                            .iter()
                            .map(|e| judge_among(&title, None, e, &both))
                            .collect();
                        let matched: Vec<&Match> =
                            verdicts.iter().filter_map(Verdict::matched).collect();
                        let sure = matched
                            .iter()
                            .filter(|m| m.confidence == Confidence::Sure)
                            .count();
                        assert!(
                            sure <= 1,
                            "{title} (hour {hour}, gap {gap}, reversed {reversed}) is sure of {sure}"
                        );
                        if matched.len() == 2 {
                            assert!(
                                matched
                                    .iter()
                                    .all(|m| m.reason == Some("another_fixture_fits")),
                                "{title}: two fit and neither is marked: {matched:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn two_cards_of_the_same_fighters_and_two_tournaments_in_a_week_are_guesses() {
    // Two cards, the same pair, two days apart; a title naming the pair fits both.
    let mut a = event("event:ufc:4");
    a.id = "event:ufc:9001".into();
    a.name = "UFC 340: Gane vs. Hokit".into();
    a.starts_at = "2026-11-14T22:00:00Z".into();
    let mut b = a.clone();
    b.id = "event:ufc:9002".into();
    b.name = "UFC Fight Night: Gane vs. Hokit".into();
    b.starts_at = "2026-11-16T22:00:00Z".into();
    let both = [a.clone(), b.clone()];
    let title = "UFC Gane vs Hokit Main Card 1080p WEB-DL-NX";
    for e in &both {
        let v = judge_among(title, Some("2026-11-16T12:00:00Z"), e, &both);
        let m = v.matched().expect("fits");
        assert_eq!(
            (m.confidence, m.reason),
            (Confidence::Likely, Some("another_fixture_fits")),
            "{v:?}"
        );
    }
    // Alone it is as it was: names, a date, nothing else that fits.
    let v = judge_among(
        title,
        Some("2026-11-16T12:00:00Z"),
        &a,
        std::slice::from_ref(&a),
    );
    assert_eq!(v.matched().unwrap().confidence, Confidence::Sure);
    // A card's number is its own: a title with the number is not a guess because of a rival.
    let v = judge_among(
        "UFC 340 Gane vs Hokit Main Card 1080p WEB-DL-NX",
        Some("2026-11-16T12:00:00Z"),
        &a,
        &both,
    );
    assert_eq!(v.matched().unwrap().confidence, Confidence::Sure, "{v:?}");

    // Golf: two events of the tour share a word and a week.
    let tour = |id: &str, name: &str, start: &str, end: &str| {
        let mut e = event("event:pga-tour:81");
        e.id = id.into();
        e.name = name.into();
        e.starts_at = start.into();
        e.ends_at = Some(end.into());
        e
    };
    let open = tour(
        "event:pga-tour:9001",
        "Houston Open",
        "2026-04-02T05:00:00Z",
        "2026-04-05T05:00:00Z",
    );
    let champ = tour(
        "event:pga-tour:9002",
        "Houston Championship",
        "2026-04-03T05:00:00Z",
        "2026-04-06T05:00:00Z",
    );
    let both = [open.clone(), champ.clone()];
    for e in &both {
        let v = judge_among(
            "PGA Tour Houston Round 3 2026 04 04 1080p WEB-DL-NX",
            None,
            e,
            &both,
        );
        let m = v.matched().expect("fits");
        assert_eq!(m.reason, Some("another_fixture_fits"), "{v:?}");
    }
    let v = judge_among(
        "PGA Tour Houston Open Round 3 2026 04 04 1080p WEB-DL-NX",
        None,
        &open,
        &both,
    );
    assert_eq!(v.matched().unwrap().confidence, Confidence::Sure, "{v:?}");
    assert_eq!(
        judge_among(
            "PGA Tour Houston Open Round 3 2026 04 04 1080p WEB-DL-NX",
            None,
            &champ,
            &both
        )
        .refusal(),
        Some(RefusalCode::TournamentMismatch)
    );
}

// ------------------------------------------------------------------------------------------ C2

fn m(part: Part, confidence: Confidence, reason: Option<&'static str>, unlabelled: bool) -> Match {
    Match {
        part,
        confidence,
        reason,
        evidence: vec!["league"],
        unlabelled,
        part_established: false,
        unverified: false,
    }
}

#[test]
fn a_sibling_settles_only_the_part_of_a_file_that_is_otherwise_certain() {
    use Confidence::{Likely, Sure};
    let g = || Some("bb".to_string());
    let prelims = || (g(), m(Part::Prelims, Sure, None, false));
    // The one uncertainty is the part: a sure sibling settles it.
    let mut items = vec![
        (g(), m(Part::Main, Likely, Some("unlabelled_main"), true)),
        prelims(),
    ];
    settle_unlabelled(&mut items);
    assert_eq!((items[0].1.confidence, items[0].1.reason), (Sure, None));
    // Which event it is stays a guess whatever a sibling is sure of.
    for reason in [
        "date_only",
        "place_only",
        "rematch_unverified",
        "another_fixture_fits",
        "schedule_unknown",
    ] {
        let mut items = vec![(g(), m(Part::Main, Likely, Some(reason), true)), prelims()];
        settle_unlabelled(&mut items);
        assert_eq!(
            (items[0].1.confidence, items[0].1.reason),
            (Likely, Some(reason)),
            "{reason}"
        );
    }
    // A sibling that is itself a guess lends nothing.
    let mut items = vec![
        (g(), m(Part::Main, Likely, Some("unlabelled_main"), true)),
        (
            g(),
            m(Part::Prelims, Likely, Some("rematch_unverified"), false),
        ),
    ];
    settle_unlabelled(&mut items);
    assert_eq!(items[0].1.reason, Some("unlabelled_main"));
    // Neither does one from another uploader, nor one with no uploader at all.
    let mut items = vec![
        (g(), m(Part::Main, Likely, Some("unlabelled_main"), true)),
        (Some("other".into()), m(Part::Prelims, Sure, None, false)),
        (None, m(Part::Prelims, Sure, None, false)),
    ];
    settle_unlabelled(&mut items);
    assert_eq!(items[0].1.reason, Some("unlabelled_main"));
}

#[test]
fn the_quoted_siblings_do_not_promote_a_date_only_match() {
    let ev = event("event:ufc:4");
    let league = catalog::find("ufc").unwrap();
    let around = Surroundings::all_known();
    let found = vec![
        candidate("UFC 2026 11 14 1080p WEB-DL-FBB", 9, "2026-11-15T01:00:00Z"),
        candidate(
            "UFC 334 Prelims 1080p WEB-DL-FBB",
            9,
            "2026-11-15T01:00:00Z",
        ),
    ];
    let j = find::judge_all(found, &ev, league, &around, None);
    let by = |needle: &str| {
        j.candidates
            .iter()
            .find(|c| c.release.title.as_deref().unwrap().contains(needle))
            .unwrap()
    };
    let dated = by("2026 11 14");
    assert_eq!(
        (dated.m.confidence, dated.m.reason),
        (Confidence::Likely, Some("date_only"))
    );
    assert!(dated.m.confidence != Confidence::Sure);
    assert_eq!(by("Prelims").m.confidence, Confidence::Sure);
    // The rematch numeral stays unverified beside a labelled sibling of the same uploader.
    let ev = event("event:ufc:2");
    let found = vec![
        candidate(
            "UFC.Fight.Night.Ankalaev.vs.Walker.2.WEB-DL.H264.Fight-BB",
            9,
            "2024-01-14T06:46:00Z",
        ),
        candidate(
            "UFC.Fight.Night.Ankalaev.vs.Walker.2.Prelims.WEB-DL.H264.Fight-BB",
            9,
            "2024-01-13T23:00:00Z",
        ),
    ];
    let j = find::judge_all(found, &ev, league, &around, None);
    assert!(
        j.candidates
            .iter()
            .all(|c| c.m.reason == Some("rematch_unverified")),
        "{:?}",
        j.candidates.iter().map(|c| &c.m).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------------- C3 and C4

/// Words that make a release a show about the event, in the languages the indexers carry.
const SHOULDER_PHRASES: &[&str] = &[
    "Highlights",
    "Extended Highlights",
    "Condensed",
    "Recap",
    "Resumen",
    "Resumo",
    "Melhores Momentos",
    "Zusammenfassung",
    "R&eacute;sum&eacute;",
    "Resume",
    "Sintesi",
    "Samenvatting",
    "Studio Replay",
    "Replay Show",
    "Countdown",
    "Weigh-In",
    "Press Conference",
    "Post-Fight",
    "Inside the Octagon",
    "Preview",
];

#[test]
fn a_shoulder_word_is_refused_wherever_it_stands() {
    let places: &[&str] = &[
        "EPL 2019 01 19 Liverpool vs Crystal Palace {} 1080p WEB-DL",
        "EPL 2019 01 19 {} Liverpool vs Crystal Palace 1080p WEB-DL",
        "EPL 2019 01 19 Liverpool vs Crystal Palace | {} 1080p WEB-DL",
        "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p WEB-DL [{}]",
        "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p WEB-DL ({})",
        "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p WEB-DL {{{}}}",
        "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p WEB-DL {}",
        "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p WEB-DL-{}",
        "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p [EN, {}]",
        "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p [{}, EN]",
        "EPL 2019 01 19 Liverpool vs Crystal Palace&nbsp;{} 1080p",
    ];
    let ev = event("event:premier-league:30");
    let mut leaks = vec![];
    for p in places {
        for phrase in SHOULDER_PHRASES {
            let title = p.replace("{}", phrase);
            let v = judge_with(&title, None, &ev, false);
            if v.matched().is_some() {
                leaks.push(title);
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "{} accepted:\n{}",
        leaks.len(),
        leaks.join("\n")
    );
    // And the same words on a fight card.
    let ev = event("event:ufc:4");
    for p in [
        "UFC 334 Gane vs Hokit Main Card {} 1080p WEB-DL",
        "UFC 334 Gane vs Hokit Main Card 1080p WEB-DL [{}]",
        "UFC 334 Gane vs Hokit Main Card 1080p WEB-DL-{}",
    ] {
        for phrase in SHOULDER_PHRASES {
            let title = p.replace("{}", phrase);
            assert!(
                judge_with(&title, Some("2026-11-15T01:00:00Z"), &ev, false)
                    .matched()
                    .is_none(),
                "{title}"
            );
        }
    }
}

/// The zone rule is total: every word of an accepted title is accounted for by a named rule.
/// This runs it over every row of the table that is accepted, and prints what accounts for each
/// word of the first of them, so that a change in the rules shows up as a change in the text.
#[test]
fn every_word_of_an_accepted_title_has_a_rule() {
    let mut checked = 0;
    for (title, published, _id, want) in table() {
        if !matches!(want, Want::Match(..)) {
            continue;
        }
        let r = release::parse(title, published.filter(|p| *p != "none").map(at));
        assert!(!r.oversized, "{title}");
        assert!(!r.roles.is_empty(), "{title}");
        assert!(
            r.unexplained().is_empty(),
            "{title}: no rule for {:?}\n{}",
            r.unexplained(),
            r.explain()
        );
        checked += 1;
    }
    assert!(checked > 40, "{checked}");
    assert_eq!(
        release::parse("UFC.333.Prelims.1080p.WEB-DL.H264.Fight-BB", None).explain(),
        "ufc: the competition's name; 333: card number or serial (judged by the matcher); \
         prelims: part word; 1080p: format; web: format; dl: format; h264: format; \
         fight: uploader's tag; bb: uploader's tag"
    );
    assert_eq!(
        release::parse(
            "EPL 2026-2027 / Matchweek 9 / 31.10.2026 / Chelsea FC vs Manchester United [Soccer, WEB-DL HD/720p/25fps MKV/H.264, EN, TNT Sport HD]",
            None
        )
        .explain(),
        "epl: the competition's name; matchweek: stage, round or season (checked against the event); \
         9: stage, round or season (checked against the event); chelsea: team or fighter (judged by the matcher); \
         fc: team or fighter (judged by the matcher); vs: team or fighter (judged by the matcher); \
         manchester: team or fighter (judged by the matcher); united: team or fighter (judged by the matcher); \
         soccer: sport label; web: format; dl: format; hd: format; 720p: format; 25fps: format; \
         mkv: format; h: format; 264: format; en: language or channel; \
         tnt: language or channel; sport: language or channel; hd: language or channel"
    );
}

#[test]
fn a_word_nothing_accounts_for_is_found_in_every_part_of_a_title() {
    let ev = event("event:premier-league:30");
    for template in [
        "Zebra EPL 2019 01 19 Liverpool vs Crystal Palace 1080p WEB-DL",
        "EPL Zebra 2019 01 19 Liverpool vs Crystal Palace 1080p WEB-DL",
        "EPL 2019 01 19 Zebra Liverpool vs Crystal Palace 1080p WEB-DL",
        "EPL 2019 01 19 Liverpool vs Crystal Palace | Zebra | 1080p WEB-DL",
        "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p Zebra Quokka WEB-DL",
        "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p WEB-DL [Zebra Quokka, EN]",
        "EPL 2019 01 19 Liverpool vs Crystal Palace 1080p [Soccer, Zebra]",
    ] {
        // Refused, or at most a guess: never sure.
        let v = judge_with(template, None, &ev, false);
        assert!(
            v.matched().is_none_or(|m| m.confidence != Confidence::Sure),
            "{template}: {v:?}"
        );
    }
}

// ------------------------------------------------------------------------------------------ C6

#[test]
fn an_unlabelled_pfl_file_answers_no_part_and_is_never_recommended() {
    let ev = event("event:pfl:10");
    let league = catalog::find("pfl").unwrap();
    let around = Surroundings::all_known();
    let found = vec![
        candidate(
            "PFL Tampa Cyborg vs Vieira 2026 08 22 720p WEB-DL H264-FBB",
            30,
            "2026-08-24T01:20:00Z",
        ),
        candidate(
            "PFL Tampa Cyborg vs Vieira 2026 08 22 Full Event 720p WEB-DL H264-FBB",
            3,
            "2026-08-24T01:20:00Z",
        ),
    ];
    let judged = find::judge_all(found, &ev, league, &around, None);
    let all = Found {
        candidates: judged.candidates,
        skipped: judged.skipped,
        complete: true,
        ..Default::default()
    };
    let unlabelled = all
        .candidates
        .iter()
        .find(|c| c.m.reason == Some("unlabelled_whole"))
        .expect("kept as a guess");
    assert_eq!(unlabelled.m.confidence, Confidence::Likely);
    assert!(unlabelled.m.confidence != Confidence::Sure);
    let labelled = all
        .candidates
        .iter()
        .find(|c| c.m.reason.is_none())
        .expect("a labelled file");
    assert!(labelled.m.confidence == Confidence::Sure);
    // Asked for the prelims, only the labelled whole-event file answers.
    let prelims = all.for_part(Some(Part::Prelims));
    assert_eq!(prelims.candidates.len(), 1);
    assert_eq!(prelims.candidates[0].m.reason, None);
    assert_eq!(prelims.skipped.get(&RefusalCode::AnotherPart), Some(&1));
    // On its own, the unlabelled file is listed and still not recommended.
    let alone = find::judge_all(
        vec![candidate(
            "PFL Tampa Cyborg vs Vieira 2026 08 22 720p WEB-DL H264-FBB",
            30,
            "2026-08-24T01:20:00Z",
        )],
        &ev,
        league,
        &around,
        None,
    );
    assert_eq!(alone.candidates.len(), 1);
    assert!(alone.candidates[0].m.confidence != Confidence::Sure);
}

// ------------------------------------------------------------------------------------------ C7

#[test]
fn what_each_judgement_does_when_what_it_depends_on_is_incomplete() {
    let ev = event("event:premier-league:32");
    // Teams: only exact names of the event's two teams are taken without the league's list.
    for (title, teams_complete, want) in [
        (
            "EPL 2026 10 31 Chelsea vs Manchester 1080p WEB-DL",
            false,
            Some(RefusalCode::TeamMismatch),
        ),
        (
            "EPL 2026 10 31 Chelsea vs Manchester 1080p WEB-DL",
            true,
            Some(RefusalCode::AmbiguousTeam),
        ),
        (
            "EPL 2026 10 31 Chelsea vs Man United 1080p WEB-DL",
            false,
            Some(RefusalCode::TeamMismatch),
        ),
        (
            "EPL 2026 10 31 Chelsea vs Manchester United 1080p WEB-DL",
            false,
            None,
        ),
        (
            "EPL 2026 10 31 Chelsea vs United 1080p WEB-DL",
            false,
            Some(RefusalCode::TeamMismatch),
        ),
    ] {
        let v = judge_known(title, None, &ev, false, teams_complete, true);
        assert_eq!(
            v.refusal(),
            want,
            "{title} (teams complete: {teams_complete}): {v:?}"
        );
    }
    // The schedule: without it "no other fixture fits" is not known, so a game is a guess...
    let v = judge_known(
        "EPL 2026 10 31 Chelsea vs Manchester United 1080p WEB-DL",
        None,
        &ev,
        false,
        true,
        false,
    );
    let m = v.matched().unwrap();
    assert_eq!(
        (m.confidence, m.reason),
        (Confidence::Likely, Some("schedule_unknown"))
    );
    // ...and a card with only a date to go on is refused, not guessed.
    let card = event("event:ufc:4");
    let date_only = "UFC 2026 11 14 1080p WEB-DL-FBB";
    let v = judge_known(
        date_only,
        Some("2026-11-15T01:00:00Z"),
        &card,
        false,
        true,
        true,
    );
    assert_eq!(v.matched().unwrap().reason, Some("date_only"));
    let v = judge_known(
        date_only,
        Some("2026-11-15T01:00:00Z"),
        &card,
        false,
        true,
        false,
    );
    assert_eq!(v.refusal(), Some(RefusalCode::NoEvidence), "{v:?}");
    // A numbered card does not depend on the schedule: its number is its own.
    let v = judge_known(
        "UFC 334 Prelims 1080p WEB-DL-FBB",
        Some("2026-11-15T01:00:00Z"),
        &card,
        false,
        true,
        false,
    );
    assert_eq!(v.matched().unwrap().confidence, Confidence::Sure);
}

// ------------------------------------------------------------------------------------------ C9

#[test]
fn a_pathological_title_is_refused_unread_and_fast() {
    let started = Instant::now();
    for title in [
        "UFC ".repeat(100_000),
        "UFC ".repeat(99),
        "a ".repeat(150),
        "EPL ".repeat(60),
        format!("UFC 334 {}", "Gane ".repeat(200)),
        "[".repeat(5_000),
        "[a]".repeat(50),
        "&amp;".repeat(5_000),
        "\u{ad}".repeat(1_000),
    ] {
        let r = release::parse(&title, None);
        assert!(r.oversized, "{} bytes: not refused", title.len());
        assert_eq!(r.roles.len(), 0);
        let ev = event("event:ufc:4");
        assert_eq!(
            judge_with(&title, None, &ev, false).refusal(),
            Some(RefusalCode::TitleTooLong)
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    // The longest real title (a forum title with its whole format block) is read.
    let long = "UEFA Champions League / SF / G1 / 28.04.2026 / Paris Saint-Germain @ Bayern Munich [Soccer, WEB-DL HD/1080p/50fps, MKV/H.264, EN/P";
    assert!(long.len() < release::MAX_TITLE_BYTES / 2);
    assert!(!release::parse(long, None).oversized);
}

#[test]
fn a_large_result_set_is_judged_in_bounds_and_stops_at_the_deadline() {
    let ev = event("event:ufc:4");
    let league = catalog::find("ufc").unwrap();
    let around = Surroundings::default();
    // 5,000 results: ordinary titles, near misses and giant ones.
    let found: Vec<_> = (0..5_000)
        .map(|i| match i % 4 {
            0 => candidate(
                &format!("UFC.334.Gane.vs.Hokit.Main.Card.1080p.WEB-DL.H264-G{i}"),
                5,
                "2026-11-15T01:00:00Z",
            ),
            1 => candidate(&"UFC ".repeat(10_000 + i), 5, "2026-11-15T01:00:00Z"),
            2 => candidate(
                &format!("UFC 334 Zebra{i} Embedded Countdown {}", "x ".repeat(100)),
                5,
                "2026-11-15T01:00:00Z",
            ),
            _ => candidate(
                &format!("NBA 2026 11 12 Team{i} vs Other{i} 720p"),
                5,
                "2026-11-15T01:00:00Z",
            ),
        })
        .collect();
    let started = Instant::now();
    let j = find::judge_all(found.clone(), &ev, league, &around, None);
    let took = started.elapsed();
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert_eq!(j.unjudged, 5_000 - find::MAX_JUDGED);
    let judged: usize = j.skipped.values().sum::<usize>() + j.candidates.len();
    assert_eq!(judged, find::MAX_JUDGED);
    // A deadline that has passed judges nothing, and says how many were left.
    let j = find::judge_all(found, &ev, league, &around, Some(Instant::now()));
    assert_eq!((j.candidates.len(), j.unjudged), (0, 5_000));
}

// ------------------------------------------------------------------------------------------ C8

fn found(complete: bool) -> Searched {
    Searched {
        complete,
        ..Default::default()
    }
}

#[tokio::test]
async fn an_event_is_searched_once_per_cooldown_whatever_is_asked() {
    let gate = CopiesGate::default();
    gate.set_limits(Duration::from_secs(3600), Duration::from_secs(3600), 100);
    let searches = AtomicUsize::new(0);
    let go = |fresh: bool| {
        let searches = &searches;
        let gate = &gate;
        async move {
            gate.serve::<_, _, ()>("event:ufc:1", fresh, || async {
                searches.fetch_add(1, Ordering::SeqCst);
                Ok(found(true))
            })
            .await
            .ok()
            .unwrap()
        }
    };
    assert!(matches!(go(false).await, Served::Searched(_)));
    // Plain, fresh, and fresh again: all the last answer, with an age.
    for fresh in [false, true, true, false] {
        assert!(
            matches!(go(fresh).await, Served::Cached { .. }),
            "fresh {fresh}"
        );
    }
    assert_eq!(searches.load(Ordering::SeqCst), 1);
    // After the cooldown a fresh request searches again; a plain one still waits for the ttl.
    gate.set_limits(Duration::ZERO, Duration::from_secs(3600), 100);
    assert!(matches!(go(false).await, Served::Cached { .. }));
    assert!(matches!(go(true).await, Served::Searched(_)));
    assert_eq!(searches.load(Ordering::SeqCst), 2);
    // A search that did not finish is not repeated within the cooldown either, and is after it.
    let gate2 = CopiesGate::default();
    gate2.set_limits(Duration::from_secs(3600), Duration::from_secs(3600), 100);
    let n = AtomicUsize::new(0);
    for fresh in [false, false, true] {
        gate2
            .serve::<_, _, ()>("event:ufc:2", fresh, || async {
                n.fetch_add(1, Ordering::SeqCst);
                Ok(found(false))
            })
            .await
            .ok()
            .unwrap();
    }
    assert_eq!(n.load(Ordering::SeqCst), 1);
    gate2.set_limits(Duration::ZERO, Duration::from_secs(3600), 100);
    gate2
        .serve::<_, _, ()>("event:ufc:2", false, || async {
            n.fetch_add(1, Ordering::SeqCst);
            Ok(found(false))
        })
        .await
        .ok()
        .unwrap();
    assert_eq!(n.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn simultaneous_requests_for_one_event_share_one_search() {
    let gate = Arc::new(CopiesGate::default());
    let searches = Arc::new(AtomicUsize::new(0));
    let mut tasks = vec![];
    for _ in 0..12 {
        let (gate, searches) = (gate.clone(), searches.clone());
        tasks.push(tokio::spawn(async move {
            gate.serve::<_, _, ()>("event:ufc:1", true, || async {
                searches.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(150)).await;
                Ok(found(true))
            })
            .await
            .ok()
            .unwrap()
        }));
    }
    let mut searched = 0;
    for t in tasks {
        match t.await.unwrap() {
            Served::Searched(_) => searched += 1,
            Served::Cached { .. } => {}
            Served::Busy { .. } => panic!("busy"),
        }
    }
    assert_eq!((searches.load(Ordering::SeqCst), searched), (1, 1));
}

#[tokio::test]
async fn all_events_together_start_a_bounded_number_of_searches() {
    let gate = CopiesGate::default();
    gate.set_limits(Duration::ZERO, Duration::ZERO, 3);
    let searches = AtomicUsize::new(0);
    let (mut searched, mut busy) = (0, 0);
    for i in 0..40 {
        let s = gate
            .serve::<_, _, ()>(&format!("event:ufc:{i}"), true, || async {
                searches.fetch_add(1, Ordering::SeqCst);
                Ok(found(true))
            })
            .await
            .ok()
            .unwrap();
        match s {
            Served::Searched(_) => searched += 1,
            Served::Busy { retry_after } => {
                busy += 1;
                assert!(retry_after <= Duration::from_secs(60));
            }
            Served::Cached { .. } => panic!("nothing was cached for a new event"),
        }
    }
    assert_eq!(
        (searched, busy, searches.load(Ordering::SeqCst)),
        (3, 37, 3)
    );
    // An event with an answer gets it, with its age, when the rate is spent: not another search.
    let s = gate
        .serve::<_, _, ()>("event:ufc:0", true, || async {
            searches.fetch_add(1, Ordering::SeqCst);
            Ok(found(true))
        })
        .await
        .ok()
        .unwrap();
    assert!(matches!(s, Served::Cached { .. }));
    assert_eq!(searches.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn the_gate_remembers_a_bounded_number_of_events_and_not_errors() {
    let gate = CopiesGate::default();
    gate.set_limits(
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        100_000,
    );
    for i in 0..(MAX_SLOTS + 100) {
        gate.serve::<_, _, ()>(&format!("event:ufc:{i}"), false, || async {
            Ok(found(true))
        })
        .await
        .ok()
        .unwrap();
    }
    assert!(gate.remembered() <= MAX_SLOTS, "{}", gate.remembered());
    // A search that fails (no backend) leaves nothing behind and costs nothing next time.
    let gate = CopiesGate::default();
    let tries = AtomicUsize::new(0);
    for _ in 0..3 {
        let r = gate
            .serve::<_, _, &str>("event:ufc:1", false, || async {
                tries.fetch_add(1, Ordering::SeqCst);
                Err("no backend")
            })
            .await;
        assert!(r.is_err());
    }
    assert_eq!(tries.load(Ordering::SeqCst), 3);
}

// ---------------------------------------------------------------------- the property, widened

#[test]
fn no_release_is_sure_of_two_events_whatever_date_it_carries() {
    // Every title of the table and one written from each event, with its date and the days
    // either side, against every event of the fixtures *as neighbours of each other*.
    let events = all_events();
    let mut titles: Vec<(String, Option<String>)> = table()
        .into_iter()
        .map(|(t, p, _, _)| (t.to_string(), p.map(str::to_string)))
        .collect();
    for e in &events {
        let league = catalog::find(&e.league).unwrap();
        if league.sport == catalog::Sport::Golf {
            continue;
        }
        let start = at(&e.starts_at);
        for off in -2i64..=2 {
            let d = (start + TDuration::days(off)).date();
            let title = match (&e.home_name, &e.away_name) {
                (Some(h), Some(a)) => format!(
                    "{} {:04} {:02} {:02} {h} vs {a} 1080p WEB-DL",
                    league.aliases[0],
                    d.year(),
                    u8::from(d.month()),
                    d.day()
                ),
                _ => format!("{} 1080p WEB-DL", e.name.replace(": ", " ")),
            };
            titles.push((title, Some(e.starts_at.clone())));
        }
    }
    for (title, published) in &titles {
        for league in catalog::LEAGUES {
            let here: Vec<EventRow> = events
                .iter()
                .filter(|e| e.league == league.slug)
                .cloned()
                .collect();
            let sure: Vec<&str> = here
                .iter()
                .filter(|e| {
                    matches!(
                        judge_among(title, published.as_deref(), e, &here),
                        Verdict::Match(ref m) if m.confidence == Confidence::Sure
                    )
                })
                .map(|e| e.id.as_str())
                .collect();
            assert!(sure.len() <= 1, "{title} is sure of {sure:?}");
        }
    }
}
