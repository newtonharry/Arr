//! Title folding shared by catalog lookup and release matching.
//!
//! Folding is lossy on purpose: compatibility decomposition, combining marks removed, a few
//! letters that do not decompose (æ, ø, ß, ł, ...) transliterated, lowercase, `&` -> `and`,
//! apostrophes dropped, other punctuation turned into separators. Letters and digits from any
//! script are preserved, so non-Latin titles still compare meaningfully.

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

const APOSTROPHES: [char; 5] = ['\'', '\u{2019}', '\u{2018}', '\u{02BC}', '`'];

/// Folded characters, keeping non-alphanumeric characters as-is (callers decide how
/// to treat separators).
pub fn fold_chars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.nfkd() {
        if is_combining_mark(c) {
            continue;
        }
        match c {
            'æ' | 'Æ' => out.push_str("ae"),
            'œ' | 'Œ' => out.push_str("oe"),
            'ø' | 'Ø' => out.push('o'),
            'ß' | 'ẞ' => out.push_str("ss"),
            'ł' | 'Ł' => out.push('l'),
            'đ' | 'Đ' | 'ð' | 'Ð' => out.push('d'),
            'þ' | 'Þ' => out.push_str("th"),
            'ı' => out.push('i'),
            '&' => out.push_str(" and "),
            c if APOSTROPHES.contains(&c) => {}
            c => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// Folded alphanumeric tokens.
pub fn tokens(input: &str) -> Vec<String> {
    fold_chars(input)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Comparison key: folded tokens without a leading article, joined without spaces
/// (so "S.W.A.T." and "SWAT" or "Slow Horses" and "SlowHorses" compare equal).
pub fn key_from_tokens(tokens: &[String]) -> String {
    let start = match tokens.first().map(String::as_str) {
        Some("the" | "a" | "an") if tokens.len() > 1 => 1,
        _ => 0,
    };
    tokens[start..].concat()
}

pub fn key(input: &str) -> String {
    key_from_tokens(&tokens(input))
}

/// Franchise-prefixed forms of known aliases, derived only from the item's own metadata.
///
/// The canonical title is `prefix + rest` when `rest` is itself a known alias; the metadata
/// then says the prefix (e.g. a franchise name) is detachable. Every other alias that is a longer
/// form of the same `rest` (it ends with it) is then also known with that prefix prepended:
/// "Nebula Arin: The Last Keeper" + alias "Arin: The Last Keeper" + alias "The Legend of
/// Arin: The Last Keeper" gives "Nebula The Legend of Arin: The Last Keeper". Nothing is
/// stripped from release names and no substring of a title is ever matched on its own.
pub fn prefixed_aliases(canonical: &str, aliases: &[&str]) -> Vec<Vec<String>> {
    let toks = |t: &str| tokens(split_trailing_year(t).0);
    let aliases: Vec<Vec<String>> = aliases
        .iter()
        .map(|a| toks(a))
        .filter(|t| !t.is_empty())
        .collect();
    let full = toks(canonical);
    let mut out: Vec<Vec<String>> = vec![];
    for rest in aliases
        .iter()
        .filter(|r| r.len() < full.len() && full.ends_with(r))
    {
        let prefix = &full[..full.len() - rest.len()];
        if prefix
            .iter()
            .all(|t| matches!(t.as_str(), "the" | "a" | "an"))
        {
            continue;
        }
        for longer in aliases
            .iter()
            .filter(|b| b.len() > rest.len() && b.ends_with(rest) && !b.starts_with(prefix))
        {
            let composite = [prefix, longer.as_slice()].concat();
            if !out.contains(&composite) {
                out.push(composite);
            }
        }
    }
    out
}

/// Splits a trailing "(YYYY)" disambiguator off a native title, e.g. "Doctor Who (2005)".
pub fn split_trailing_year(title: &str) -> (&str, Option<u16>) {
    let t = title.trim_end();
    if let Some(open) = t.strip_suffix(')').and_then(|rest| rest.rfind('(')) {
        let inner = &t[open + 1..t.len() - 1];
        if inner.len() == 4
            && inner.bytes().all(|b| b.is_ascii_digit())
            && let Ok(year) = inner.parse::<u16>()
            && (1900..=2100).contains(&year)
        {
            return (t[..open].trim_end(), Some(year));
        }
    }
    (t, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_unicode_and_punctuation() {
        assert_eq!(key("Amélie"), "amelie");
        assert_eq!(key("Pokémon: Detective Pikachu"), "pokemondetectivepikachu");
        assert_eq!(key("Æon Flux"), "aeonflux");
        assert_eq!(
            key("Marvel’s Agents of S.H.I.E.L.D."),
            "marvelsagentsofshield"
        );
        assert_eq!(key("Law & Order"), "lawandorder");
        assert_eq!(key("The Matrix"), "matrix");
        assert_eq!(key("Matrix"), "matrix");
        assert_eq!(key("ＷＡＲ"), "war");
        assert_eq!(key("Straße"), "strasse");
        // Non-Latin scripts are preserved, not erased.
        assert_eq!(key("Слово пацана"), "словопацана");
        assert_ne!(key("Слово пацана"), "");
        assert_eq!(key("進撃の巨人"), "進撃の巨人");
    }

    #[test]
    fn leading_article_only_dropped_when_more_tokens_follow() {
        assert_eq!(key("The"), "the");
        assert_eq!(key("A"), "a");
        assert_eq!(key("A Quiet Place"), "quietplace");
    }

    #[test]
    fn franchise_prefix_is_only_carried_onto_longer_forms_of_a_known_alias() {
        let keys = |p: &str, a: &[&str]| -> Vec<String> {
            prefixed_aliases(p, a)
                .iter()
                .map(|t| key_from_tokens(t))
                .collect()
        };
        let title = "Nebula Arin: The Last Keeper";
        assert_eq!(
            keys(
                title,
                &[
                    "Arin: The Last Keeper",
                    "The Legend of Arin: The Last Keeper",
                    "Arin – Der letzte Hüter",
                ]
            ),
            vec!["nebulathelegendofarinthelastkeeper"]
        );
        // Without the bare alias the metadata does not say the prefix is detachable.
        assert!(keys(title, &["The Legend of Arin: The Last Keeper"]).is_empty());
        // A leading article is not a prefix; an alias that already carries it adds nothing.
        assert!(keys("The Keeper", &["Keeper", "Last Keeper"]).is_empty());
        assert!(
            keys(
                title,
                &["Arin: The Last Keeper", "Nebula Saga Arin: The Last Keeper"]
            )
            .is_empty()
        );
        // Aliases that do not end with the bare title are never prefixed.
        assert!(keys(title, &["Arin: The Last Keeper", "Keeper of Arin"]).is_empty());
    }

    #[test]
    fn splits_disambiguation_year() {
        assert_eq!(
            split_trailing_year("Doctor Who (2005)"),
            ("Doctor Who", Some(2005))
        );
        assert_eq!(split_trailing_year("WAR"), ("WAR", None));
        assert_eq!(
            split_trailing_year("Title (Extended)"),
            ("Title (Extended)", None)
        );
        assert_eq!(split_trailing_year("Odd (12345)"), ("Odd (12345)", None));
    }
}
