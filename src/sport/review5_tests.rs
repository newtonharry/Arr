//! Tests of review round 5: a word that looks like a filler (a stage, a season, a game number,
//! a numeral) is an assertion about the event, and is checked or capped like any other. No word
//! of a title is left with a role that nothing reads.

use super::catalog;
use super::data::EventRow;
use super::match_tests::{
    at, event, judge_with, known_spans, published_for, sure_table, table, teams_of,
};
use super::matcher::claims::{self, Claim};
use super::matcher::{Confidence, Context, RefusalCode, Verdict, judge};
use super::release::{self, Assertion, Role};

fn sure(v: &Verdict) -> bool {
    matches!(v, Verdict::Match(m) if m.confidence == Confidence::Sure)
}

fn reason(v: &Verdict) -> Option<&'static str> {
    v.matched().and_then(|m| m.reason)
}

// ------------------------------------------------------------------------------- the structure

/// Which reader takes each role. Exhaustive: a role added to the parser with no reader does not
/// compile. A reader is something that decides a verdict from the word, never a word left alone.
fn reader(role: Role) -> &'static str {
    match role {
        Role::Prefix => "closed list (a channel before the name, a connective)",
        Role::League => "the league (judge)",
        Role::Claim(_) => "claims (a stage, a round or a season, against the event)",
        Role::Number => "claims (card number, serial)",
        Role::Series => "claims (serial)",
        Role::Part => "claims (part)",
        Role::Names => "claims (sides)",
        Role::Head => "claims (head)",
        Role::Format => "closed list (technical words)",
        Role::Group => "the known uploaders",
        Role::Language => "closed list (language, channel)",
        Role::Label => "closed list (sport label)",
        Role::Feed => "cap_feeds (the event's teams)",
        Role::Free => "unverified(): caps at likely",
        Role::Unexplained => "unexplained(): refuses",
    }
}

#[test]
fn every_word_is_in_one_bucket_and_every_bucket_has_a_reader() {
    let mut words = 0;
    for (title, published, _id, _want) in table() {
        let r = release::parse(title, published.filter(|p| *p != "none").map(at));
        if r.oversized {
            continue;
        }
        let mut claimed = vec![0usize; r.assertions.len()];
        for (w, role) in &r.roles {
            words += 1;
            assert!(!reader(*role).is_empty());
            match role {
                Role::Claim(i) => {
                    assert!(*i < r.assertions.len(), "{title}: {w} claims nothing");
                    claimed[*i] += 1;
                    // ...and the claim is one the matcher reads.
                    let _ = claims::claim_of(&r.assertions[*i]);
                }
                Role::Number => assert!(r.number.is_some(), "{title}: {w}"),
                Role::Series => assert!(r.series != release::Series::None, "{title}: {w}"),
                Role::Part => assert!(r.part != release::PartHint::None, "{title}: {w}"),
                Role::Names => assert!(r.sides.is_some(), "{title}: {w}"),
                Role::Group => assert!(r.group.is_some(), "{title}: {w}"),
                Role::Feed => assert!(r.feed_codes().contains(&w.as_str()), "{title}: {w}"),
                // A word that is left free is not a claim in disguise: no assertion reads it
                // as one.
                Role::Free => {
                    let is_year = w.len() == 4 && w.bytes().all(|b| b.is_ascii_digit());
                    assert!(
                        release::round_of(std::slice::from_ref(w)).is_none() && !is_year,
                        "{title}: the free word {w:?} is an assertion"
                    );
                }
                _ => {}
            }
        }
        // Every assertion has words (a folded season span has none) and no word is in two.
        for (i, a) in r.assertions.iter().enumerate() {
            if matches!(a, Assertion::Round(_)) {
                assert!(claimed[i] > 0, "{title}: assertion {a:?} has no words");
            }
        }
    }
    assert!(words > 1000, "{words}");
}

// ------------------------------------------------------------------- the findings, as quoted

fn judged(title: &str, published: Option<&str>, ev: &EventRow) -> Verdict {
    judge_with(title, published, ev, false)
}

#[test]
fn c15_distinct_part_words_conflict() {
    let ev = event("event:ufc:3");
    for title in [
        "UFC 333 Prelims [Early Prelims] 1080p WEB-DL-DARKSPORT",
        "UFC 333 Early Prelims [Prelims] 1080p WEB-DL-DARKSPORT",
        "UFC 333 Prelims [Main Card] 1080p WEB-DL-DARKSPORT",
        "UFC 333 Full Event Round 3 1080p WEB-DL-DARKSPORT",
        "UFC 333 Full Event Day 2 1080p WEB-DL-DARKSPORT",
        "UFC 333 Full Event [Prelims] 1080p WEB-DL-DARKSPORT",
    ] {
        let v = judged(title, Some("2026-10-25T01:00:00Z"), &ev);
        assert!(v.matched().is_none(), "{title}: {v:?}");
    }
    // The same part twice is one part.
    let v = judged(
        "UFC 333 Prelims [Prelims] 1080p WEB-DL-DARKSPORT",
        Some("2026-10-25T01:00:00Z"),
        &ev,
    );
    assert!(sure(&v), "{v:?}");
}

#[test]
fn c19_a_card_with_unknown_headliners_is_a_rival_unless_the_release_rules_it_out() {
    let mut a = event("event:ufc:5");
    a.starts_at = "2026-11-15T22:00:00Z".into();
    let mut b = event("event:ufc:4");
    b.starts_at = "2026-11-22T22:00:00Z".into();
    b.name = "UFC 334".into();
    let neighbours = vec![a.clone(), b];
    let teams = teams_of("ufc");
    let league = catalog::find("ufc").unwrap();
    let spans = vec![(at("2026-10-25T00:00:00Z"), at("2026-11-26T00:00:00Z"))];
    let cx = Context::new(league, &a, &teams, &neighbours, false, true, spans, true);
    let at_23 = Some(at("2026-11-23T01:00:00Z"));
    let v = judge(
        &release::parse("UFC Walker vs Hill Main Card 1080p WEB-DL-DARKSPORT", at_23),
        &a,
        &cx,
    );
    assert_eq!(
        v.matched().map(|m| (m.confidence, m.reason)),
        Some((Confidence::Likely, Some("schedule_unknown"))),
        "{v:?}"
    );
    // A title that claims a Fight Night rules a numbered card out.
    let v = judge(
        &release::parse(
            "UFC Fight Night Walker vs Hill Main Card 1080p WEB-DL-DARKSPORT",
            at_23,
        ),
        &a,
        &cx,
    );
    assert!(sure(&v), "{v:?}");
}

#[test]
fn c21_to_c25_stage_season_and_round_words_are_assertions() {
    let nba = event("event:nba:60");
    let epl = event("event:premier-league:32");
    let publ = Some("2026-11-12T06:00:00Z");
    let base = "Washington Wizards vs Indiana Pacers 1080p WEB-DL-DARKSPORT";
    // C21: a game number, a phase.
    for extra in [
        "Game 7",
        "G7",
        "Preseason",
        "Pre-Season",
        "Playoffs",
        "Play-Offs",
        "Regular Season",
        "RS",
        "Group Stage",
        "Quarter Final",
        "Semi Final",
        "Final",
        "Leg 2",
        "Match 3",
        "Matchweek 4",
        "Matchday 4",
        "Round 5",
        "Day 2",
        "Week 6",
    ] {
        let v = judged(&format!("NBA 2026 11 12 {extra} {base}"), publ, &nba);
        assert!(!sure(&v), "{extra}: {v:?}");
        assert!(
            v.matched().is_none() || reason(&v) == Some("round_unverified"),
            "{extra}: {v:?}"
        );
    }
    // C22: a stage of another kind than the stored round does not verify it.
    let v = judged(
        "EPL Final / 31.10.2026 / Chelsea FC vs Manchester United 720p WEB-DL-DARKSPORT",
        Some("2026-10-31T20:00:00Z"),
        &epl,
    );
    assert_eq!(reason(&v), Some("round_unverified"), "{v:?}");
    // C23: every round and every season is kept.
    let v = judged(
        "EPL 2026-2027 / Matchweek 9 / Matchweek 10 / 31.10.2026 / Chelsea FC vs Manchester United 720p WEB-DL-DARKSPORT",
        Some("2026-10-31T20:00:00Z"),
        &epl,
    );
    assert_eq!(v.refusal(), Some(RefusalCode::RoundMismatch), "{v:?}");
    let v = judged(
        "EPL 2026-2027 / 2024-2025 / Matchweek 9 / 31.10.2026 / Chelsea FC vs Manchester United 720p WEB-DL-DARKSPORT",
        Some("2026-10-31T20:00:00Z"),
        &epl,
    );
    assert_eq!(v.refusal(), Some(RefusalCode::WrongSeason), "{v:?}");
    // ...and equal ones are one.
    let v = judged(
        "EPL 2026-2027 / Matchweek 9 / Matchweek 9 / 31.10.2026 / Chelsea FC vs Manchester United 720p WEB-DL-DARKSPORT",
        Some("2026-10-31T20:00:00Z"),
        &epl,
    );
    assert!(sure(&v), "{v:?}");
    // C24: a season word is a season, not only a folded span.
    let v = judged(
        "NBA Season 2024 / 2026 11 12 / Washington Wizards vs Indiana Pacers 1080p WEB-DL-DARKSPORT",
        publ,
        &nba,
    );
    assert_eq!(v.refusal(), Some(RefusalCode::WrongSeason), "{v:?}");
    let v = judged(
        "NBA 2024 2026 11 12 Washington Wizards vs Indiana Pacers 1080p WEB-DL-DARKSPORT",
        publ,
        &nba,
    );
    assert_eq!(v.refusal(), Some(RefusalCode::WrongSeason), "{v:?}");
    // A lone year inside the stored span is not the stored season.
    let v = judged(
        "NBA 2026 / 2026 11 12 / Washington Wizards vs Indiana Pacers 1080p WEB-DL-DARKSPORT",
        publ,
        &nba,
    );
    assert_eq!(reason(&v), Some("season_unverified"), "{v:?}");
    // C25: a claim on a shape that does not support it is checked, not dropped.
    let ufc = event("event:ufc:3");
    let p = Some("2026-10-25T01:00:00Z");
    for title in [
        "UFC 333 2024-2025 Main Card 1080p WEB-DL-DARKSPORT",
        "UFC 333 Season 2024 Main Card 1080p WEB-DL-DARKSPORT",
        "UFC 333 Playoffs Main Card 1080p WEB-DL-DARKSPORT",
        "UFC 333 Matchweek 4 Main Card 1080p WEB-DL-DARKSPORT",
    ] {
        let v = judged(title, p, &ufc);
        assert!(!sure(&v), "{title}: {v:?}");
    }
}

#[test]
fn c26_and_c27_a_numeral_is_verified_only_by_an_equal_stored_ordinal() {
    let ev = event("event:ufc:2");
    let publ = Some("2024-01-13T23:00:00Z");
    let with_earlier = |title: &str| judge_with(title, publ, &ev, true);
    // One older stored meeting exists: that does not make a "9" right.
    for title in [
        "UFC Fight Night Ankalaev vs Walker 9 Main Card 1080p WEB-DL-DARKSPORT",
        "UFC Fight Night Ankalaev vs Walker 2 Main Card 1080p WEB-DL-DARKSPORT",
        "UFC Fight Night Ankalaev vs Walker II Main Card 1080p WEB-DL-DARKSPORT",
        "UFC Fight Night Ankalaev vs Walker III Main Card 1080p WEB-DL-DARKSPORT",
        "UFC Fight Night Ankalaev II vs Walker Main Card 1080p WEB-DL-DARKSPORT",
    ] {
        let v = with_earlier(title);
        assert_eq!(reason(&v), Some("rematch_unverified"), "{title}: {v:?}");
    }
    // Two different numerals in a title conflict.
    let v =
        with_earlier("UFC Fight Night Ankalaev II vs Walker III Main Card 1080p WEB-DL-DARKSPORT");
    assert_eq!(v.refusal(), Some(RefusalCode::FightersMismatch), "{v:?}");
    // An equal stored ordinal verifies it, in digits or Roman.
    let mut stored = ev.clone();
    stored.name = "UFC Fight Night: Ankalaev vs. Walker 2".into();
    for title in [
        "UFC Fight Night Ankalaev vs Walker 2 Main Card 1080p WEB-DL-DARKSPORT",
        "UFC Fight Night Ankalaev vs Walker II Main Card 1080p WEB-DL-DARKSPORT",
    ] {
        let v = judge_with(title, publ, &stored, false);
        assert!(sure(&v), "{title}: {v:?}");
    }
    let v = judge_with(
        "UFC Fight Night Ankalaev vs Walker III Main Card 1080p WEB-DL-DARKSPORT",
        publ,
        &stored,
        false,
    );
    assert_eq!(v.refusal(), Some(RefusalCode::FightersMismatch), "{v:?}");
}

// ------------------------------------------------------- the insertion property for claims

const CLAIMS: &[&str] = &[
    "Final",
    "Semi Final",
    "Quarter-Final",
    "Playoffs",
    "Preseason",
    "Pre-Season",
    "Regular Season",
    "RS",
    "Group Stage",
    "Leg 1",
    "Game 7",
    "G3",
    "Match 3",
    "Matchweek 12",
    "Matchday 4",
    "Round 5",
    "Day 2",
    "Week 6",
    "Season 2024",
    "2024",
    "2024-2025",
    "2024-25",
    "II",
    "III",
    "IV",
    "9",
];
/// The same kind of assertion twice with different values.
const PAIRS: &[(&str, &str)] = &[
    ("Matchweek 3", "Matchweek 4"),
    ("Game 1", "Game 2"),
    ("Round 1", "Round 2"),
    ("Season 2023", "Season 2024"),
    ("2023", "2024"),
    ("2023-2024", "2024-2025"),
    ("II", "III"),
    ("Final", "Semi Final"),
    ("Day 1", "Day 2"),
];

#[test]
fn no_assertion_inserted_into_a_sure_title_leaves_it_sure() {
    let mut leaks: Vec<(String, String)> = vec![];
    let mut tried = 0;
    for (n, (title, published, id)) in sure_table().into_iter().enumerate() {
        let title = title.as_str();
        let ev = event(id);
        let teams = teams_of(&ev.league);
        let cx = Context::new(
            catalog::find(&ev.league).unwrap(),
            &ev,
            &teams,
            &[],
            false,
            true,
            known_spans(true),
            true,
        );
        let when = published_for(published, &ev);
        let base = release::parse(title, when);
        // A title's own assertions, parts and numerals may be repeated: that is one claim.
        // ...and so may what the stored event itself says.
        let stored_round = ev
            .round
            .as_deref()
            .and_then(|t| release::round_of(&crate::title::tokens(t)));
        let stored = |a: &Assertion| match a {
            Assertion::Round(r) => {
                stored_round.is_some_and(|s| r.standing(&s) == release::Standing::Equal)
            }
            Assertion::Season(..) => false,
        };
        let repeats = |v: &release::SportRelease| {
            v.assertions
                .iter()
                .all(|a| base.assertions.contains(a) || stored(a))
                && v.part == base.part
                && v.rematches.iter().all(|r| base.rematches.contains(r))
        };
        let chars: Vec<char> = title.chars().collect();
        let mut cuts: Vec<usize> = vec![0, chars.len()];
        for (i, c) in chars.iter().enumerate() {
            if matches!(c, ' ' | '.' | '/' | '|' | '-' | '_') {
                cuts.push(i);
                cuts.push(i + 1);
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        let step = if n % 2 == 0 { 1 } else { 3 };
        let mut variants: Vec<(String, String)> = vec![];
        for &p in cuts.iter().step_by(step) {
            let (l, r): (String, String) =
                (chars[..p].iter().collect(), chars[p..].iter().collect());
            for w in CLAIMS {
                variants.push((w.to_string(), format!("{l} {w} {r}")));
                variants.push((w.to_string(), format!("{l} [{w}] {r}")));
                variants.push((w.to_string(), format!("{l}{w} {r}")));
            }
            for (a, b) in PAIRS {
                variants.push((format!("{a}+{b}"), format!("{l} {a} {b} {r}")));
                variants.push((format!("{a}+{b}"), format!("{l} {a} [{b}] {r}")));
                variants.push((format!("{a}+{b}"), format!("{l} ({a}) / {b} {r}")));
            }
        }
        for (what, v) in variants {
            let parsed = release::parse(&v, when);
            tried += 1;
            if sure(&judge(&parsed, &ev, &cx)) && !repeats(&parsed) {
                leaks.push((what, v));
            }
        }
    }
    assert!(tried > 20_000, "{tried}");
    leaks.sort();
    leaks.dedup();
    let mut by: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for (w, v) in &leaks {
        by.entry(w).or_default().push(v);
    }
    let report: Vec<String> = by
        .iter()
        .map(|(w, v)| {
            format!(
                "[{w}] {} leaks, e.g.\n    {}",
                v.len(),
                v[..v.len().min(3)].join("\n    ")
            )
        })
        .collect();
    assert!(
        leaks.is_empty(),
        "{} of {tried} insertions left a title sure:\n{}",
        leaks.len(),
        report.join("\n")
    );
    let _ = Claim::Round;
}
