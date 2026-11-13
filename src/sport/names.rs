//! Name rules for the release matcher: which words say nothing about a team, how a team named
//! in a release is told from the teams of its league, and how two fighters' names compare.
//! Pure functions over folded tokens (`crate::title::tokens`); nothing here reads a store.

use super::data::{EventRow, TeamRow};
use crate::title;

/// Words that are part of how a club is written, not of which club it is. Taken out of both
/// sides before names are compared, so "Chelsea FC" is "Chelsea" and "Arsenal London" is
/// "Arsenal". (A word that tells two clubs of a city apart, "United" or "City", is never here.)
const GENERIC_TEAM_WORDS: &[&str] = &[
    "fc", "afc", "cf", "sc", "ac", "fk", "sk", "bk", "cd", "ud", "rc", "rcd", "sv", "ssc", "club",
    "the", "london", "rfc", "rlfc",
];

/// Words that follow a fighter's name and are not part of it.
const NAME_SUFFIXES: &[&str] = &["jr", "sr"];

/// The tokens of a name without the generic words.
pub fn strip_generic(tokens: &[String]) -> Vec<String> {
    tokens
        .iter()
        .filter(|t| !GENERIC_TEAM_WORDS.contains(&t.as_str()))
        .cloned()
        .collect()
}

/// A team of the event's league, as the names a release may call it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamKey {
    pub id: String,
    /// Each variant is non-empty, generic words removed, no duplicates.
    pub variants: Vec<Vec<String>>,
}

fn variants_of(names: &[&str]) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = vec![];
    for n in names {
        let v = strip_generic(&tokens(n));
        // A club's leading number ("1. FC Koln") is part of its name and, as often, left off.
        let bare = (v.len() > 1 && v[0].len() <= 2 && v[0].bytes().all(|b| b.is_ascii_digit()))
            .then(|| v[1..].to_vec());
        for v in std::iter::once(v).chain(bare) {
            if !v.is_empty() && !out.contains(&v) {
                out.push(v);
            }
        }
    }
    out
}

/// The teams a release's sides are resolved against: every team of the league that is known
/// (`sport_teams`), and always the event's own two, so a league whose list was never read can
/// still be matched, though only by exact names.
pub fn team_keys(rows: &[TeamRow], ev: &EventRow) -> Vec<TeamKey> {
    let mut out: Vec<TeamKey> = rows
        .iter()
        .filter_map(|t| {
            let names: Vec<&str> = [Some(t.name.as_str()), t.short_name.as_deref()]
                .into_iter()
                .flatten()
                .collect();
            let variants = variants_of(&names);
            (!variants.is_empty()).then(|| TeamKey {
                id: t.id.clone(),
                variants,
            })
        })
        .collect();
    for (id, name) in [(&ev.home_id, &ev.home_name), (&ev.away_id, &ev.away_name)] {
        let (Some(id), Some(name)) = (id, name) else {
            continue;
        };
        let variants = variants_of(&[name.as_str()]);
        match out.iter_mut().find(|k| &k.id == id) {
            Some(k) => {
                for v in variants {
                    if !k.variants.contains(&v) {
                        k.variants.push(v);
                    }
                }
            }
            None if !variants.is_empty() => out.push(TeamKey {
                id: id.clone(),
                variants,
            }),
            None => {}
        }
    }
    out
}

fn contiguous(hay: &[String], needle: &[String]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// How a side of a release resolves among the teams of the league.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Exactly one team.
    Team(String),
    /// Two or more teams fit ("Manchester", "Melbourne", "City").
    Ambiguous,
    /// None does.
    None,
}

/// A side names a team when its words (generic ones out) equal one of the team's variants. If
/// no team is named exactly, a side that is a whole run of words inside exactly one team's
/// variant ("Wigan" for "Wigan Warriors", "Paris" for "Paris Basketball") names that team; if
/// several teams contain it, it names none of them (that is `Ambiguous`).
///
/// The second step needs every team of the league: "inside exactly one" means nothing when the
/// other clubs are not known. With `complete` false (the league's list was never read, or is
/// stale) only the first step runs, so "Manchester" names no team, though "Manchester United"
/// does.
pub fn resolve_side(side: &[String], teams: &[TeamKey], complete: bool) -> Resolved {
    // (initials the title left apart, "A C Milan", are one word, as in the stored name)
    let run = strip_generic(&fold_initials(side.to_vec()));
    if run.is_empty() || run.iter().map(|t| t.len()).sum::<usize>() < 3 {
        return Resolved::None;
    }
    let exact: Vec<&TeamKey> = teams.iter().filter(|t| t.variants.contains(&run)).collect();
    match exact.as_slice() {
        [one] => return Resolved::Team(one.id.clone()),
        [] => {}
        _ => return Resolved::Ambiguous,
    }
    if !complete {
        return Resolved::None;
    }
    let inside: Vec<&TeamKey> = teams
        .iter()
        .filter(|t| t.variants.iter().any(|v| contiguous(v, &run)))
        .collect();
    match inside.as_slice() {
        [] => Resolved::None,
        [one] => Resolved::Team(one.id.clone()),
        _ => Resolved::Ambiguous,
    }
}

/// Every explicit suffix a name ends with, in order ("jr", "sr"). Several on one name are kept
/// so that they can be compared as written.
pub fn suffixes_of(tokens: &[String]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = tokens
        .iter()
        .rev()
        .map_while(|t| NAME_SUFFIXES.iter().copied().find(|s| *s == t))
        .collect();
    out.reverse();
    out
}

/// The tokens of a name, with the letters of an abbreviation joined: "U.S." and "US" are one
/// word, so are "D.C." and "DC", "F.C." and "FC", "L.A." and "LA".
pub fn tokens(s: &str) -> Vec<String> {
    fold_initials(title::tokens(s))
}

/// Joins runs of two or more single letters ("u", "s" -> "us").
pub fn fold_initials(tokens: Vec<String>) -> Vec<String> {
    let single = |t: &String| t.len() == 1 && t.as_bytes()[0].is_ascii_alphabetic();
    let mut out: Vec<String> = vec![];
    let mut run = String::new();
    let mut n = 0;
    let flush = |out: &mut Vec<String>, run: &mut String, n: &mut usize| {
        if *n > 0 {
            out.push(std::mem::take(run));
        }
        *n = 0;
    };
    for t in tokens {
        if single(&t) {
            run.push_str(&t);
            n += 1;
        } else {
            if n == 1 {
                out.push(std::mem::take(&mut run));
                n = 0;
            }
            flush(&mut out, &mut run, &mut n);
            out.push(t);
        }
    }
    if n == 1 {
        out.push(std::mem::take(&mut run));
        n = 0;
    }
    flush(&mut out, &mut run, &mut n);
    out
}

/// A fighter's name without a trailing Jr/Sr/numeral suffix.
pub fn fighter_tokens(tokens: &[String]) -> Vec<String> {
    let mut t = tokens.to_vec();
    while t.len() > 1
        && t.last()
            .is_some_and(|w| NAME_SUFFIXES.contains(&w.as_str()))
    {
        t.pop();
    }
    t
}

/// Two names for one fighter: equal, or one ends with the other (a surname, or a surname with
/// a second part), with the last word the same. Hyphen and space are the same ("Cortes-Acosta"
/// and "Cortes Acosta").
pub fn same_fighter(a: &[String], b: &[String]) -> bool {
    // (initials apart in the title, "B J Penn", are one word, as in the stored name)
    let (a, b) = (
        fighter_tokens(&fold_initials(a.to_vec())),
        fighter_tokens(&fold_initials(b.to_vec())),
    );
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if title::key_from_tokens(&a) == title::key_from_tokens(&b) {
        return true;
    }
    let (short, long) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    long.ends_with(short)
        || title::key_from_tokens(long).ends_with(&title::key_from_tokens(short))
            && long.last() == short.last()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Vec<String> {
        title::tokens(s)
    }

    fn team(id: &str, name: &str, short: Option<&str>) -> TeamRow {
        TeamRow {
            league: "premier-league".into(),
            id: id.into(),
            name: name.into(),
            short_name: short.map(str::to_string),
            abbreviation: None,
        }
    }

    fn keys() -> Vec<TeamKey> {
        let ev = EventRow {
            id: "event:premier-league:1".into(),
            league: "premier-league".into(),
            season: None,
            starts_at: "2026-11-08T15:00:00Z".into(),
            ends_at: None,
            name: "Chelsea v Manchester United".into(),
            short_name: None,
            home_id: Some("1".into()),
            home_name: Some("Chelsea".into()),
            away_id: Some("2".into()),
            away_name: Some("Manchester United".into()),
            round: None,
            venue: None,
            state: "scheduled".into(),
        };
        team_keys(
            &[
                team("1", "Chelsea", None),
                team("2", "Manchester United", Some("Man United")),
                team("3", "Manchester City", Some("Man City")),
                team("4", "Arsenal", None),
                team("5", "AFC Bournemouth", None),
            ],
            &ev,
        )
    }

    #[test]
    fn generic_words_are_not_part_of_a_club() {
        let k = keys();
        assert_eq!(
            resolve_side(&t("Chelsea FC"), &k, true),
            Resolved::Team("1".into())
        );
        assert_eq!(
            resolve_side(&t("Arsenal London"), &k, true),
            Resolved::Team("4".into())
        );
        assert_eq!(
            resolve_side(&t("Bournemouth"), &k, true),
            Resolved::Team("5".into())
        );
    }

    #[test]
    fn a_short_name_is_a_name() {
        let k = keys();
        assert_eq!(
            resolve_side(&t("Man United"), &k, true),
            Resolved::Team("2".into())
        );
        assert_eq!(
            resolve_side(&t("Man City"), &k, true),
            Resolved::Team("3".into())
        );
    }

    #[test]
    fn a_word_two_clubs_share_names_neither() {
        let k = keys();
        assert_eq!(
            resolve_side(&t("Manchester"), &k, true),
            Resolved::Ambiguous
        );
        assert_eq!(
            resolve_side(&t("United"), &k, true),
            Resolved::Team("2".into())
        );
        assert_eq!(resolve_side(&t("Real Madrid"), &k, true), Resolved::None);
        assert_eq!(resolve_side(&t("Ch"), &k, true), Resolved::None);
    }

    #[test]
    fn fighters_compare_by_surname_and_hyphen() {
        assert!(same_fighter(&t("Cortes-Acosta"), &t("Cortes Acosta")));
        assert!(same_fighter(&t("Saint Denis"), &t("Benoit Saint Denis")));
        assert!(same_fighter(&t("O'Malley Jr."), &t("Sean O'Malley")));
        assert!(!same_fighter(&t("Walker"), &t("Hill")));
        assert!(!same_fighter(&t("Acosta"), &t("Cortes Acosta Lopez")));
    }
}
