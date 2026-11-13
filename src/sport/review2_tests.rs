//! Tests of the structure the second review asked for: a match is `sure` only when closed
//! evidence earns it, so anything unverified, in any place of a title, caps it at `likely`.
//! The insertion property is the test that finds what a list of single titles cannot.

use super::Part;
use super::catalog;
use super::data;
use super::espn;
use super::find::{self, Searched, Surroundings};
use super::gate::{CopiesGate, Served};
use super::match_tests::{
    all_events, at, candidate, event, judge_with, known_spans, published_for, sure_table, teams_of,
};
use super::matcher::{Confidence, Context, Verdict, judge};
use super::release;
use crate::import::rfc3339;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use time::Duration as TDuration;

fn sure(v: &Verdict) -> bool {
    matches!(v, Verdict::Match(m) if m.confidence == Confidence::Sure)
}

// ------------------------------------------------------------------------------------- C and A

/// Words a hostile uploader or a hostile title might add: summaries and shows in several
/// languages, result words and scores, single fights, a hash, and words that are none of these.
const HOSTILE: &[&str] = &[
    "summary",
    "highlights",
    "extended highlights",
    "recap",
    "resumen",
    "zusammenfassung",
    "KO",
    "wins",
    "2-1",
    "110-98",
    "full fight",
    "main event",
    "countdown",
    "0123456789abcdef0123456789abcdef01234567",
];
/// Words that are nothing in particular. None is a known uploader, so none is a tag either:
/// there is no place in a title where one of them keeps it `sure`.
const RANDOM: &[&str] = &["qzvxr", "bromel"];

/// Every way of putting `word` into `title` that the review named: at every boundary between
/// tokens (spaced, and glued to either neighbour), inside each bracket form, after a bar, and in
/// or beside the uploader's slot.
fn insertions(title: &str, word: &str) -> Vec<String> {
    let chars: Vec<char> = title.chars().collect();
    let is_sep = |c: char| matches!(c, ' ' | '.' | '/' | '|' | '-' | '_');
    let mut cuts: Vec<usize> = vec![0, chars.len()];
    for (i, c) in chars.iter().enumerate() {
        if is_sep(*c) {
            cuts.push(i);
            cuts.push(i + 1);
        }
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = vec![];
    for &p in &cuts {
        let (l, r): (String, String) = (chars[..p].iter().collect(), chars[p..].iter().collect());
        out.push(format!("{l} {word} {r}"));
        out.push(format!("{l}{word}{r}"));
        // wrapped in each bracket form, next to the word on either side
        if HOSTILE.contains(&word) {
            out.push(format!("{l} [{word}] {r}"));
            out.push(format!("{l} ({word}) {r}"));
            out.push(format!("{l} {{{word}}} {r}"));
        }
        out.push(format!("{l}{word} {r}"));
        out.push(format!("{l} {word}{r}"));
    }
    for form in [
        "{t} [{w}]",
        "{t} ({w})",
        "{t} {{{w}}}",
        "{t} [EN, {w}]",
        "{t} [{w}, EN]",
        "{t} | {w}",
        "{t} / {w}",
        "{t} | {w} |",
        "{t}-{w}",
        "{t} {w}",
        "{t}.{w}",
        "{w} {t}",
    ] {
        out.push(form.replace("{t}", title).replace("{w}", word));
    }
    // Inside a bracket the title already has.
    if let Some(i) = title.rfind(']') {
        out.push(format!("{}, {word}{}", &title[..i], &title[i..]));
        out.push(format!("{} {word}{}", &title[..i], &title[i..]));
    }
    // In the uploader's slot: instead of the tag, before it, and after it.
    if let Some(i) = title.rfind('-') {
        let (head, tag) = (&title[..i], &title[i + 1..]);
        if !tag.contains(' ') {
            out.push(format!("{head}-{word}"));
        }
        out.push(format!("{head}-{word}-{tag}"));
        out.push(format!("{head} {word}-{tag}"));
        out.push(format!("{head}.{word}-{tag}"));
        out.push(format!("{title} {word}"));
        out.push(format!("{head}-{tag}{word}"));
    }
    out
}

/// Inserting a hostile word anywhere in a title that is `sure` never leaves it `sure`: the word
/// is refused, or is unverified text, or changes which event it is. (A random word may stand
/// in the uploader's own slot, where any word is a tag.)
#[test]
fn nothing_inserted_into_a_sure_title_leaves_it_sure() {
    let mut rows = 0;
    let mut tried = 0;
    let mut leaks: Vec<(String, String)> = vec![];
    for (title, published, id) in sure_table() {
        let title = title.as_str();
        let ev = event(id);
        // The context is the same for every variant of a row: built once.
        let teams = teams_of(&ev.league);
        let neighbours: Vec<data::EventRow> = all_events()
            .into_iter()
            .filter(|o| o.league == ev.league)
            .collect();
        let cx = Context::new(
            catalog::find(&ev.league).unwrap(),
            &ev,
            &teams,
            &neighbours,
            false,
            true,
            known_spans(true),
            true,
        );
        let judge_it = |t: &str| judge(&release::parse(t, published_for(published, &ev)), &ev, &cx);
        // The unmodified title is sure.
        assert!(sure(&judge_it(title)), "{title}");
        rows += 1;
        for word in HOSTILE.iter().chain(RANDOM) {
            for variant in insertions(title, word) {
                tried += 1;
                if sure(&judge_it(&variant)) {
                    leaks.push((word.to_string(), variant));
                }
            }
        }
        // Phrases split by brackets and separators.
        if rows % 5 == 0 {
            for variant in phrase_attacks(title, 4) {
                tried += 1;
                if sure(&judge_it(&variant)) {
                    leaks.push(("phrase".to_string(), variant));
                }
            }
        }
        // The uploader's tag and the channel codes, attacked on their own.
        for (what, variant) in tag_and_channel_attacks(title, rows % 4 == 0) {
            tried += 1;
            if sure(&judge_it(&variant)) {
                leaks.push((what, variant));
            }
        }
    }
    assert!(rows >= 40, "{rows} sure rows");
    assert!(tried > 10_000, "{tried}");
    leaks.sort();
    leaks.dedup();
    let mut by_word: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for (w, v) in &leaks {
        by_word.entry(w.as_str()).or_default().push(v.as_str());
    }
    let report: Vec<String> = by_word
        .iter()
        .map(|(w, v)| {
            format!(
                "[{w}] {} leaks, e.g.\n    {}",
                v.len(),
                v.iter().take(3).cloned().collect::<Vec<_>>().join("\n    ")
            )
        })
        .collect();
    assert!(
        leaks.is_empty(),
        "{} of {tried} insertions left a title sure:\n{}",
        leaks.len(),
        report.join("\n")
    );
}

/// Phrases that are a single bout or a show only when their halves stand together.
const PHRASES: &[(&str, &str)] = &[
    ("Main", "Event"),
    ("Co", "Main"),
    ("Full", "Fight"),
    ("Wrap", "Up"),
    ("Round", "Up"),
    ("Post", "Fight"),
    ("High", "lights"),
    ("Count", "Down"),
];

/// Each half of a phrase put in at every boundary, in each bracket form (none, square, round,
/// curly) and with each separator between them (a space, a bar): the halves can be split by
/// any of it and still read as the phrase.
fn phrase_attacks(title: &str, step: usize) -> Vec<String> {
    let chars: Vec<char> = title.chars().collect();
    let mut cuts: Vec<usize> = vec![chars.len()];
    for (i, c) in chars.iter().enumerate() {
        if matches!(c, ' ' | '.' | '/' | '|' | '-' | '_') {
            cuts.push(i + 1);
        }
    }
    cuts.sort_unstable();
    cuts.dedup();
    let wrap = |f: usize, w: &str| match f {
        0 => w.to_string(),
        1 => format!("[{w}]"),
        2 => format!("({w})"),
        _ => format!("{{{w}}}"),
    };
    let mut out = vec![];
    for &p in cuts.iter().step_by(step) {
        let (l, r): (String, String) = (chars[..p].iter().collect(), chars[p..].iter().collect());
        for (a, b) in PHRASES {
            for fa in 0..4 {
                for fb in 0..4 {
                    for sep in [" ", " | ", "-"] {
                        out.push(format!("{l} {}{sep}{} {r}", wrap(fa, a), wrap(fb, b)));
                    }
                }
            }
        }
    }
    out
}

/// Tags that no list knows, in every shape the review named: lowercase, mixed case, digits, a
/// date, a shoulder word, a word split by the hyphen, a hash, a link.
const UNKNOWN_TAGS: &[&str] = &[
    "qzvxr",
    "QzVxr",
    "QZVXR",
    "24",
    "2026-12-25",
    "Summary",
    "vlogs",
    "vlog",
    "UP",
    "Round-UP",
    "Wrap-Up",
    "High-lights",
    "Post-Fight",
    "Round.UP",
    "Dark-Sport",
    "DARKSPORTS",
    "darksport",
    "Darksport",
    "NVA",
    "nva",
    "tj",
    "GRP",
    "0123456789abcdef0123456789abcdef",
    "www.example.com",
    "KO",
    "DARKSPORT2",
    "DARK-SPORT",
];
/// Real uploaders, put in as a second tag.
const OTHER_TAGS: &[&str] = &["NX", "TJET", "Fight-BB", "nVa", "FBB", "POOTLED"];
/// Channel names, each followed by arbitrary codes.
const CHANNELS: &[&str] = &[
    "DAZN", "TNT", "Viaplay", "SN", "Spectrum", "BBC", "ESPN", "Sky", "NBC", "CBS", "TV", "HD",
];
const CODES: &[&str] = &[
    "POD", "QQQ", "XYZ", "AB", "ZZ", "POOD", "KO", "WIN", "GAL", "SAS", "LAL", "tj", "x1", "007",
    "a-b",
];

/// What a hostile uploader or channel text could do in the last free slots.
fn tag_and_channel_attacks(title: &str, thorough: bool) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![];
    let mut add = |what: &str, v: String| {
        if v != title {
            out.push((what.to_string(), v));
        }
    };
    // (1) An unknown tag in the tag's own place, and where there was none.
    for t in UNKNOWN_TAGS {
        if let Some(i) = title.rfind('-')
            && !title[i..].contains([' ', ']'])
        {
            add("tag", format!("{}-{t}", &title[..i]));
            add("tag", format!("{}.{t}", &title[..i]));
        }
        add("tag", format!("{title}-{t}"));
        add("tag", format!("{title} {t}"));
        add("tag", format!("{title}.{t}"));
        add("tag bracket", format!("{title} [{t}]"));
        add("tag bracket", format!("{title} [EN, {t}]"));
    }
    // (2) A second tag beside the first, in the main text and in a bracket (a title with no tag
    // of its own would simply gain one).
    let tagged = release::parse(title, None).group.is_some();
    for t in OTHER_TAGS.iter().filter(|_| tagged) {
        add("second tag", format!("{title}-{t}"));
        add("second tag", format!("{title} {t}"));
        add("second tag", format!("{title} [{t}]"));
        add("second tag", format!("{title} [{t}] [{t}]"));
        add("second tag", format!("[{t}] {title}"));
    }
    // (3) A code after each channel name, in each of the forms a channel is written.
    // (a code that is a word of a closed list on its own is verified wherever it stands)
    let closed = |k: &str| {
        let r = release::parse(
            &format!("UFC 333 Main Card 1080p WEB-DL [EN, DAZN {k}]"),
            None,
        );
        (r.unverified().is_empty() && r.feed_codes().is_empty())
            || title.to_lowercase().contains(&k.to_lowercase())
    };
    for c in CHANNELS.iter().filter(|_| thorough) {
        for k in CODES.iter().filter(|k| !closed(k)) {
            add("channel", format!("{title} [EN, {c}-{k}]"));
            add("channel", format!("{title} [EN, {c} {k}]"));
            add("channel", format!("{title} [EN/{c}-{k}]"));
            add("channel", format!("{title} [{c}-{k}]"));
            add("channel", format!("{title} [EN, {c}/{k}]"));
        }
    }
    // (4) Every two-letter code after DAZN, not only the ones named above.
    for a in ('a'..='z').filter(|_| thorough) {
        for b in 'a'..='z' {
            let k = format!("{a}{b}");
            if !closed(&k) {
                add("dazn code", format!("{title} [EN, DAZN-{k}]"));
            }
        }
    }
    out
}

#[test]
fn a_title_is_sure_only_if_every_word_is_checked() {
    let ev = event("event:ufc:3");
    let publ = Some("2026-10-28T01:25:00Z");
    // The shape of an uploader's tag: a short plain word after the technical words.
    for (title, want_sure) in [
        ("UFC 333 Main Card 1080p WEB-DL-DARKSPORT", true),
        ("UFC 333 Main Card 1080p WEB-DL.H264-DARKSPORT", true),
        ("UFC.333.Main.Card.1080p.WEB-DL.H264.Fight-BB", true),
        ("UFC 333 Main Card 1080p WEB-DL [TJET]", true),
        ("UFC 333 Main Card 1080p WEB-DL [TGx]", true),
        ("UFC 333 Main Card 1080p WEB-DL X265 POOTLED", true),
        // Words that are tags to nobody
        ("UFC 333 Main Card 1080p WEB-DL [Summary]", false),
        ("UFC 333 Main Card 1080p WEB-DL [EN, Summary]", false),
        ("UFC 333 Main Card 1080p WEB-DL-Summary", false),
        ("UFC 333 Main Card 1080p WEB-DL Whatever", false),
        ("UFC 333 Main Card 1080p WEB-DL Fight-BB Extra", false),
        ("UFC 333 Main Card 1080p Foo WEB-DL-DARKSPORT", false),
        (
            "UFC 333 Main Card 1080p WEB-DL-0123456789abcdef0123456789abcdef",
            false,
        ),
        ("UFC 333 Main Card 1080p WEB-DL-DARKSPORT [Zebra]", false),
        ("UFC 333 Main Card 1080p WEB-DL-DARKSPORT-OTHER", false),
        // A channel outside the closed list, a code outside it, a number outside a codec
        ("UFC 333 Main Card 1080p WEB-DL [EN, Some Channel]", false),
        ("UFC 333 Main Card 1080p WEB-DL [EN, DAZN-IND] [NX]", true),
        ("UFC 333 Main Card 1080p WEB-DL [EN, DAZN-QQQQ]", false),
        ("UFC 333 Main Card 1080p WEB-DL [EN, DAZN-POD]", false),
        ("UFC 333 Main Card 1080p WEB-DL [EN, DAZN-XYZ]", false),
        ("UFC 333 Main Card 1080p WEB-DL Round-UP", false),
        ("UFC 333 Main Card 1080p WEB-DL-vlogs", false),
        ("UFC 333 Main Card 1080p WEB-DL-qzvxr", false),
        ("UFC 333 Main Card 1080p WEB-DL-DARKSPORT [NX]", false),
        ("UFC 333 Main Card 1080p WEB-DL-NX [DARKSPORT]", false),
        ("UFC 333 Main Card 1080p WEB-DL [MMA, EN] [NX]", true),
        ("UFC 333 Main Card 1080p WEB-DL [Soccer, EN] [NX]", false),
        ("UFC 333 Main Card 1080p 7 WEB-DL-DARKSPORT", false),
        ("UFC 333 Main Card 1080p WEB-DL H 264-DARKSPORT", true),
        (
            "UFC 333 Main Card 1080p WEB-DL AAC2.0 H.264-DARKSPORT",
            true,
        ),
    ] {
        let v = judge_with(title, publ, &ev, false);
        assert_eq!(sure(&v), want_sure, "{title}: {v:?}");
    }
    // A glued compound is a shoulder marker: refused, not capped.
    for title in [
        "UFC.333.Main.Card.1080p.WEB-DL.H264.ExtendedHighlights-DARKSPORT",
        "UFC.333.Main.Card.1080p.WEB-DL.H264-Highlights2026",
        "UFC.333.Main.Card.1080p.WEB-DL.H264-Recap",
        "UFC.333.Main.Card.1080p.WEB-DL.H264-Resumen",
        "UFC.333.Main.Card.1080p.WEB-DL.H264-ZusammenfassungKampf",
        "UFC.333.MainCardHighlights.1080p.WEB-DL.H264-DARKSPORT",
    ] {
        let v = judge_with(title, publ, &ev, false);
        assert!(!sure(&v), "{title}: {v:?}");
    }
}

#[test]
fn how_many_real_titles_are_still_sure() {
    // The real titles of the table that were sure before this round and are now; every one that
    // is not, and why. A matcher nothing can pass is a failure too.
    let mut still = 0;
    let mut dropped: Vec<String> = vec![];
    for (title, published, id) in sure_table() {
        let title = title.as_str();
        if sure(&judge_with(title, published, &event(id), false)) {
            still += 1;
        } else {
            dropped.push(title.to_string());
        }
    }
    assert!(dropped.is_empty(), "{dropped:?}");
    assert!(still >= 40, "{still}");
}

// ------------------------------------------------------------------------------------------ B

#[test]
fn a_tournament_word_nothing_checks_is_not_accepted_for_being_ignored() {
    let tour = event("event:pga-tour:81");
    for title in [
        "PGA Tour World Wide Technology Championship Round 3 ExtendedHighlights 2026 11 07 1080p WEB-DL",
        "PGA Tour World Wide Technology Championship Round 3 Zebra 2026 11 07 1080p WEB-DL",
        "PGA Tour World Wide Technology Round 3 Quokka Championship 2026 11 07 1080p WEB-DL",
    ] {
        let v = judge_with(title, None, &tour, false);
        assert!(!sure(&v), "{title}: {v:?}");
    }
    let v = judge_with(
        "PGA Tour World Wide Technology Championship Round 3 2026 11 07 1080p WEB-DL-NX",
        None,
        &tour,
        false,
    );
    assert!(sure(&v), "{v:?}");
}

// ---------------------------------------------------------------------------------- C1 and C7

fn judge_among_spans(
    title: &str,
    published: Option<&str>,
    ev: &data::EventRow,
    events: &[data::EventRow],
    spans: Vec<(time::OffsetDateTime, time::OffsetDateTime)>,
    whole: bool,
) -> Verdict {
    let parsed = release::parse(title, published.map(at));
    let teams = teams_of(&ev.league);
    let cx = Context::new(
        catalog::find(&ev.league).unwrap(),
        ev,
        &teams,
        events,
        false,
        true,
        spans,
        whole,
    );
    judge(&parsed, ev, &cx)
}

#[test]
fn rivals_are_looked_for_over_the_window_the_evidence_allows() {
    // Two cards of the same name a week apart: a title published after both fits both.
    let mut a = event("event:ufc:4");
    a.id = "event:ufc:9001".into();
    a.name = "UFC: Gane vs. Hokit".into();
    a.starts_at = "2026-11-01T22:00:00Z".into();
    let mut b = a.clone();
    b.id = "event:ufc:9002".into();
    b.starts_at = "2026-11-08T22:00:00Z".into();
    let both = [a.clone(), b.clone()];
    for e in &both {
        let v = judge_among_spans(
            "UFC Gane vs Hokit Main Card 1080p WEB-DL",
            Some("2026-11-09T01:00:00Z"),
            e,
            &both,
            known_spans(true),
            true,
        );
        let m = v.matched().expect("fits");
        assert_eq!(m.reason, Some("another_fixture_fits"), "{v:?}");
    }
    // Golf, no date in the title: the publish time is what says which week.
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
        let v = judge_among_spans(
            "PGA Tour Houston Round 3 1080p WEB-DL",
            Some("2026-04-06T12:00:00Z"),
            e,
            &both,
            known_spans(true),
            true,
        );
        let m = v.matched().expect("fits");
        assert_eq!(m.reason, Some("another_fixture_fits"), "{v:?}");
    }
    // A neighbour list cut at its limit is not a complete one, and nor is a schedule with no
    // recorded stretch: the match is a guess, with the reason.
    let one = [open.clone()];
    for (spans, whole) in [(known_spans(true), false), (known_spans(false), true)] {
        let v = judge_among_spans(
            "PGA Tour Houston Open Round 3 1080p WEB-DL-NX",
            Some("2026-04-04T12:00:00Z"),
            &open,
            &one,
            spans,
            whole,
        );
        assert_eq!(
            v.matched().unwrap().reason,
            Some("schedule_unknown"),
            "{v:?}"
        );
    }
    // And a stretch that ends before the days it must cover does not count.
    let day = |s: &str| at(s);
    let v = judge_among_spans(
        "PGA Tour Houston Open Round 3 1080p WEB-DL-NX",
        Some("2026-04-04T12:00:00Z"),
        &open,
        &one,
        vec![(day("2026-03-01T00:00:00Z"), day("2026-04-03T00:00:00Z"))],
        true,
    );
    assert_eq!(v.matched().unwrap().reason, Some("schedule_unknown"));
    let v = judge_among_spans(
        "PGA Tour Houston Open Round 3 1080p WEB-DL-NX",
        Some("2026-04-04T12:00:00Z"),
        &open,
        &one,
        vec![(day("2026-03-01T00:00:00Z"), day("2026-05-01T00:00:00Z"))],
        true,
    );
    assert_eq!(v.matched().unwrap().confidence, Confidence::Sure, "{v:?}");
}

#[test]
fn a_team_list_with_anything_dropped_or_renamed_or_cut_is_not_complete() {
    let whole = br#"{"sports":[{"leagues":[{"teams":[
        {"team":{"id":"1","displayName":"Chelsea"}},
        {"team":{"id":"4","displayName":"Manchester United"}},
        {"team":{"id":"5","displayName":"Manchester City"}}]}]}]}"#;
    let l = espn::parse_team_list(whole).unwrap();
    assert_eq!((l.teams.len(), l.complete), (3, true));
    // An entry with no id: Manchester City is in the league and not in the list.
    let dropped = br#"{"sports":[{"leagues":[{"teams":[
        {"team":{"id":"1","displayName":"Chelsea"}},
        {"team":{"id":"4","displayName":"Manchester United"}},
        {"team":{"displayName":"Manchester City"}}]}]}]}"#;
    let l = espn::parse_team_list(dropped).unwrap();
    assert_eq!((l.teams.len(), l.complete), (2, false));
    // An entry with nothing in it.
    let hollow = br#"{"sports":[{"leagues":[{"teams":[
        {"team":{"id":"1","displayName":"Chelsea"}}, {}]}]}]}"#;
    assert!(!espn::parse_team_list(hollow).unwrap().complete);
    // A team whose name did not pass the name rule is in the league under a neutral name.
    let renamed = br#"{"sports":[{"leagues":[{"teams":[
        {"team":{"id":"1","displayName":"Chelsea"}},
        {"team":{"id":"5","displayName":"Smith wins by KO"}}]}]}]}"#;
    let l = espn::parse_team_list(renamed).unwrap();
    assert_eq!(l.teams[1].name, "Team 5");
    assert!(!l.complete);
    // Nothing at all is not a list.
    let empty = br#"{"sports":[{"leagues":[{"teams":[]}]}]}"#;
    assert!(!espn::parse_team_list(empty).unwrap().complete);
    // And what is recorded is what a judgement reads.
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    data::create_tables(&c).unwrap();
    let now = at("2026-11-10T00:00:00Z");
    assert!(data::teams_read(&c, "premier-league").unwrap().is_none());
    data::note_teams(&c, "premier-league", now, false).unwrap();
    assert_eq!(
        data::teams_read(&c, "premier-league").unwrap(),
        Some((now, false))
    );
    data::note_teams(&c, "premier-league", now, true).unwrap();
    assert_eq!(
        data::teams_read(&c, "premier-league").unwrap(),
        Some((now, true))
    );
    let _ = &mut c;
}

#[test]
fn recorded_stretches_are_current_only_while_they_are() {
    let c = rusqlite::Connection::open_in_memory().unwrap();
    data::create_tables(&c).unwrap();
    let t = at;
    let read = t("2026-11-01T00:00:00Z");
    // November, read on the 1st; December, read on the 8th (after a failed read left a gap).
    data::record_ranges(
        &c,
        "ufc",
        &[
            (t("2026-11-02T00:00:00Z"), t("2026-12-01T00:00:00Z")),
            (t("2026-12-01T00:00:00Z"), t("2027-01-01T00:00:00Z")),
        ],
        read,
    )
    .unwrap();
    // Two days later both are fresh and join.
    let spans = data::complete_spans(&c, "ufc", t("2026-11-03T00:00:00Z")).unwrap();
    assert_eq!(
        spans,
        vec![(t("2026-11-02T00:00:00Z"), t("2027-01-01T00:00:00Z"))]
    );
    // Ten days later neither is fresh, and neither was read after its own days: nothing.
    let spans = data::complete_spans(&c, "ufc", t("2026-11-11T00:00:00Z")).unwrap();
    assert!(spans.is_empty(), "{spans:?}");
    // A stretch read after its days were over stays good for good.
    data::record_ranges(
        &c,
        "ufc",
        &[(t("2026-10-02T00:00:00Z"), t("2026-11-01T00:00:00Z"))],
        t("2026-11-01T06:00:00Z"),
    )
    .unwrap();
    let spans = data::complete_spans(&c, "ufc", t("2027-02-01T00:00:00Z")).unwrap();
    assert_eq!(
        spans,
        vec![(t("2026-10-02T00:00:00Z"), t("2026-11-01T00:00:00Z"))]
    );
}

// ------------------------------------------------------------------------------- C6, C13, C14

#[test]
fn a_part_that_is_not_established_answers_no_part_whatever_else_is_unknown() {
    let ev = event("event:pfl:10");
    let league = catalog::find("pfl").unwrap();
    let title = "PFL Tampa Cyborg vs Vieira 2026 08 22 720p WEB-DL H264-FBB";
    for around in [Surroundings::all_known(), Surroundings::default()] {
        let j = find::judge_all(
            vec![candidate(title, 30, "2026-08-24T01:20:00Z")],
            &ev,
            league,
            &around,
            None,
        );
        assert_eq!(j.candidates.len(), 1);
        let c = &j.candidates[0];
        assert!(!c.m.part_established && c.m.confidence != Confidence::Sure);
        let all = find::Found {
            candidates: j.candidates.clone(),
            complete: true,
            ..Default::default()
        };
        assert!(all.for_part(Some(Part::Prelims)).candidates.is_empty());
        assert!(all.for_part(Some(Part::Main)).candidates.is_empty());
    }
}

#[test]
fn one_team_on_both_sides_is_not_a_game() {
    let mut e = event("event:nba:62");
    e.home_id = Some("5".into());
    e.home_name = Some("Los Angeles Lakers".into());
    let v = judge_with(
        "NBA 2026 11 11 Lakers vs Los Angeles Lakers 1080p WEB-DL",
        None,
        &e,
        false,
    );
    assert!(v.matched().is_none(), "{v:?}");
    // The reader keeps the event, without sides, under the neutral name.
    let m = espn::parse_month(
        br#"{"events":[{"id":"401","date":"2026-11-12T01:30Z","name":"Lakers at Lakers",
          "competitions":[{"competitors":[
            {"homeAway":"home","team":{"id":"5","displayName":"Los Angeles Lakers"}},
            {"homeAway":"away","team":{"id":"5","displayName":"Los Angeles Lakers"}}]}]}]}"#,
        catalog::find("nba").unwrap(),
    )
    .unwrap();
    assert_eq!(m.fixtures.len(), 1);
    let f = &m.fixtures[0];
    assert!(f.home.is_none() && f.away.is_none(), "{f:?}");
}

#[test]
fn a_number_does_not_waive_the_time() {
    let ev = event("event:ufc:3");
    let late = judge_with(
        "UFC 333 Prelims 1080p WEB-DL-NX",
        Some("2026-11-12T00:00:00Z"),
        &ev,
        false,
    );
    let m = late.matched().expect("a late re-upload is listed");
    assert_eq!(
        (m.confidence, m.reason),
        (Confidence::Likely, Some("late_upload"))
    );
    let undated = judge_with("UFC 333 Prelims 1080p WEB-DL-NX", Some("none"), &ev, false);
    assert_eq!(undated.matched().unwrap().reason, Some("undated"));
    let prompt = judge_with(
        "UFC 333 Prelims 1080p WEB-DL-NX",
        Some("2026-10-28T00:00:00Z"),
        &ev,
        false,
    );
    assert_eq!(prompt.matched().unwrap().confidence, Confidence::Sure);
}

// ------------------------------------------------------------------------------------ C8, C12

#[tokio::test]
async fn a_search_dropped_half_way_still_spends_the_events_cooldown() {
    let gate = Arc::new(CopiesGate::default());
    gate.set_limits(Duration::from_secs(3600), Duration::from_secs(3600), 6);
    let searches = Arc::new(AtomicUsize::new(0));
    // Five requests in turn each begin the search and are dropped after it has sent its queries.
    for _ in 0..5 {
        let (g, n) = (gate.clone(), searches.clone());
        let task = tokio::spawn(async move {
            g.serve::<_, _, ()>("event:ufc:1", true, || async {
                n.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(Searched::default())
            })
            .await
            .ok()
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        task.abort();
        let _ = task.await;
    }
    assert_eq!(
        searches.load(Ordering::SeqCst),
        1,
        "one start, however often it is dropped"
    );
    // The next request is told to wait, and does not search.
    let s = gate
        .serve::<_, _, ()>("event:ufc:1", false, || async {
            searches.fetch_add(1, Ordering::SeqCst);
            Ok(Searched::default())
        })
        .await
        .ok()
        .unwrap();
    assert!(matches!(s, Served::Busy { .. }));
    assert_eq!(searches.load(Ordering::SeqCst), 1);
    // The other events still have their searches: the global starts were not all spent.
    for i in 0..4 {
        let s = gate
            .serve::<_, _, ()>(&format!("event:ufc:{}", 10 + i), false, || async {
                Ok(Searched::default())
            })
            .await
            .ok()
            .unwrap();
        assert!(matches!(s, Served::Searched(_)), "{i}");
    }
}

#[test]
fn a_judgement_is_made_again_from_the_search_whatever_it_said_before() {
    // The same search, judged against the event as it was and as it is after a correction.
    let league = catalog::find("premier-league").unwrap();
    let mut was = event("event:premier-league:32");
    was.round = None;
    let searched = Searched {
        releases: vec![candidate(
            "EPL 2026-2027 / 31.10.2026 / Chelsea FC vs Manchester United [Soccer, WEB-DL HD/720p, EN] [NX]",
            20,
            "2026-11-01T00:00:00Z",
        )],
        complete: true,
        ..Default::default()
    };
    let mut around = Surroundings::all_known();
    around.teams = teams_of("premier-league");
    let before = find::judge_search(&searched, &was, league, &around);
    assert!(before.candidates[0].m.confidence == Confidence::Sure);
    // The schedule is corrected: the opponent is Everton.
    let mut now = was.clone();
    now.away_id = Some("7".into());
    now.away_name = Some("Everton".into());
    let after = find::judge_search(&searched, &now, league, &around);
    assert!(after.candidates.is_empty(), "{:?}", after.candidates.len());
    // A new fixture of the same pair appears: the old recommendation is a guess now.
    let mut rival = was.clone();
    rival.id = "event:premier-league:9999".into();
    rival.starts_at = rfc3339(at(&was.starts_at) + TDuration::days(1));
    around.neighbours = vec![rival];
    let with_rival = find::judge_search(&searched, &was, league, &around);
    let c = &with_rival.candidates[0];
    assert!(c.m.confidence != Confidence::Sure);
    assert_eq!(c.m.reason, Some("another_fixture_fits"));
    let _ = all_events;
}

/// The self-review: titles built to be taken for an event that is not theirs, one per rule of the
/// brief and in the manner of a reviewer. Each is judged against the event it imitates and must
/// not come out `sure`.
#[test]
fn hostile_titles_are_never_sure() {
    let rows: &[(&str, Option<&str>, &str)] = &[
        // league names across countries and tiers
        (
            "EPL 2019 01 19 Liverpool vs Crystal Palace Reserves 720p",
            None,
            "event:premier-league:30",
        ),
        (
            "Egyptian Premier League 2019 01 19 Liverpool vs Crystal Palace 720p",
            None,
            "event:premier-league:30",
        ),
        (
            "Premier League 2019 01 19 Liverpool vs Crystal Palace [Women's] 720p",
            None,
            "event:premier-league:30",
        ),
        (
            "Bundesliga 3 2026 11 09 Koln vs RB Leipzig 720p",
            None,
            "event:bundesliga:90",
        ),
        (
            "2. Bundesliga 2026 11 09 Koln vs RB Leipzig 720p",
            None,
            "event:bundesliga:90",
        ),
        (
            "NBA Summer League 2026 11 11 Washington Wizards vs Indiana Pacers 720p",
            None,
            "event:nba:60",
        ),
        (
            "NBA Cup 2026 11 11 Washington Wizards vs Indiana Pacers 720p",
            None,
            "event:nba:60",
        ),
        // dates: the upload date, the wrong order, a second date, two-digit years
        (
            "EPL 2026 11 03 Chelsea vs Manchester United 720p",
            None,
            "event:premier-league:32",
        ),
        (
            "EPL 10.31.2026 Chelsea vs Manchester United 720p",
            None,
            "event:premier-league:32",
        ),
        (
            "EPL 2026 10 31 Chelsea vs Manchester United 2026 11 30 720p",
            None,
            "event:premier-league:32",
        ),
        (
            "EPL 31.10.26 Chelsea vs Manchester United 720p",
            None,
            "event:premier-league:32",
        ),
        (
            "EPL Chelsea vs Manchester United 720p",
            Some("2026-11-01T00:00:00Z"),
            "event:premier-league:32",
        ),
        // two clubs in a city, a name two clubs share
        (
            "EPL 2026 10 31 Man vs Chelsea 720p",
            None,
            "event:premier-league:32",
        ),
        (
            "EPL 2026 10 31 City vs Chelsea 720p",
            None,
            "event:premier-league:32",
        ),
        (
            "NBA 2026 11 12 Los Angeles vs Indiana 720p",
            None,
            "event:nba:62",
        ),
        // prefixes and glued names
        (
            "EPL 2026 10 31 FC Chelsea FC vs Manchester United FC Reserves 720p",
            None,
            "event:premier-league:32",
        ),
        (
            "EPL 2026 10 31 Chelsea Women vs Manchester United 720p",
            None,
            "event:premier-league:32",
        ),
        (
            "EPL 2026 10 31 ChelseaManchester vs United 720p",
            None,
            "event:premier-league:32",
        ),
        // a numbered card and a Fight Night with the same number; a different number
        (
            "UFC Fight Night 333 Volkanovski vs Evloev 1080p WEB-DL-DARKSPORT",
            Some("2026-10-25T01:00:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 332 Volkanovski vs Evloev 1080p WEB-DL-DARKSPORT",
            Some("2026-10-25T01:00:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL-DARKSPORT",
            Some("2026-10-25T01:00:00Z"),
            "event:ufc:7",
        ),
        // shared surnames, late replacements, swapped fighters of two cards
        (
            "UFC 333 Silva vs Evloev Main Card 1080p WEB-DL-DARKSPORT",
            Some("2026-10-25T01:00:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Volkanovski vs Smith Main Card 1080p WEB-DL-DARKSPORT",
            Some("2026-10-25T01:00:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 332 Silva vs Walker Prelims 1080p WEB-DL-DARKSPORT",
            Some("2026-10-04T01:00:00Z"),
            "event:ufc:7",
        ),
        (
            "UFC Fight Night Walker vs Hill 1080p WEB-DL-DARKSPORT",
            Some("2026-11-08T03:00:00Z"),
            "event:ufc:1",
        ),
        // rematch numerals
        (
            "UFC.Fight.Night.234.Ankalaev.vs.Walker.3.1080p.WEB-DL.H264.Fight-BB",
            Some("2024-01-14T06:46:00Z"),
            "event:ufc:2",
        ),
        (
            "PFL Austin Eblen vs Kasanganay Full Event 720p WEB-DL-FBB",
            Some("2026-07-19T01:00:00Z"),
            "event:pfl:13",
        ),
        // part words that do not name the card
        (
            "UFC 333 Volkanovski vs Evloev Main Event 1080p WEB-DL-DARKSPORT",
            Some("2026-10-25T01:00:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Volkanovski vs Evloev Co-Main 1080p WEB-DL-DARKSPORT",
            Some("2026-10-25T01:00:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Volkanovski vs Evloev Fight 2 1080p WEB-DL-DARKSPORT",
            Some("2026-10-25T01:00:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Prelims 1080p WEB-DL-DARKSPORT",
            Some("2026-12-30T01:00:00Z"),
            "event:ufc:3",
        ),
        // golf
        (
            "PGA Tour World Wide Technology Championship Round 3 Highlights 2026 11 07 1080p",
            None,
            "event:pga-tour:81",
        ),
        (
            "PGA Tour World Wide Technology Championship Round 3 2026 11 20 1080p",
            None,
            "event:pga-tour:81",
        ),
        (
            "PGA Tour Mexico Open Round 3 2026 11 07 1080p",
            None,
            "event:pga-tour:81",
        ),
        // text that hides: tags, groups, entities, hashes
        (
            "UFC 333 Main Card 1080p WEB-DL-Summary",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL [Summary]",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL [Zusammenfassung]",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL &lt;Highlights&gt;",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL [K\u{200b}O]",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL 0123456789abcdef0123456789abcdef01234567",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL magnet btih 0123456789abcdef",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL-www.example.com",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card NX 1080p WEB-DL",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "NX UFC 333 Main Card 1080p WEB-DL",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL-NX Summary",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL-NXSummary",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL.NX.Recap",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL-NX-NX",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL-NX [NX]",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL [NX] extra",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL-nx",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL-NX2",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL Fight-BBB",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL Fight BB",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL Fights-BB",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL Round-UP",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL-vlogs",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL [EN, DAZN-POD]",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL [EN, DAZN-IND POD]",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL [EN, DAZN IND IND IND IND]",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL [EN, Sky Sports Main Event]",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL [EN, TNT Sport KO]",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Main Card 1080p WEB-DL [EN, BBC Wins]",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Evloev Wins Volkanovski Main Card 1080p WEB-DL-DARKSPORT",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Volkanovski Wins vs Evloev Main Card 1080p WEB-DL-DARKSPORT",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Volkanovski vs Evloev Evloev Main Card 1080p WEB-DL-DARKSPORT",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Volkanovski Evloev vs Evloev Main Card 1080p WEB-DL-DARKSPORT",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
        (
            "UFC 333 Alexander Volkanovski vs Evloev Main Card 1080p WEB-DL-DARKSPORT",
            Some("2026-10-28T01:25:00Z"),
            "event:ufc:3",
        ),
    ];
    let mut sure_ones = vec![];
    for (title, published, id) in rows {
        let v = judge_with(title, *published, &event(id), false);
        if sure(&v) {
            sure_ones.push(*title);
        }
    }
    assert!(sure_ones.is_empty(), "sure: {sure_ones:#?}");
}
