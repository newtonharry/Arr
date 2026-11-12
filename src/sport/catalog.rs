//! The leagues Arr knows, compiled into the binary. Each one was checked against the live
//! schedule source (see `docs/API.md`); nothing is discovered at run time, so a person can only
//! ever follow a league listed here and the source is only asked for paths written below.

use crate::title;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sport {
    Soccer,
    Basketball,
    RugbyUnion,
    RugbyLeague,
    Golf,
    Mma,
}

impl Sport {
    pub fn as_str(self) -> &'static str {
        match self {
            Sport::Soccer => "soccer",
            Sport::Basketball => "basketball",
            Sport::RugbyUnion => "rugby_union",
            Sport::RugbyLeague => "rugby_league",
            Sport::Golf => "golf",
            Sport::Mma => "mma",
        }
    }

    /// What a person calls it.
    pub fn label(self) -> &'static str {
        match self {
            Sport::Soccer => "Soccer",
            Sport::Basketball => "Basketball",
            Sport::RugbyUnion => "Rugby union",
            Sport::RugbyLeague => "Rugby league",
            Sport::Golf => "Golf",
            Sport::Mma => "MMA",
        }
    }
}

pub struct League {
    /// `[a-z0-9-]{2,24}`; the `league:<slug>` id and the prefix of every event id.
    pub slug: &'static str,
    pub name: &'static str,
    pub sport: Sport,
    /// `<sport>/<league>` in the source's URL space.
    pub espn: &'static str,
    /// Whether it is played between teams (so a team can be followed). Golf and MMA have
    /// players and fighters, not teams.
    pub teams: bool,
    /// The names release titles use for it. Nothing reads them yet: they are here so the
    /// release matcher of a later phase starts from a reviewed list.
    pub aliases: &'static [&'static str],
}

const fn league(
    slug: &'static str,
    name: &'static str,
    sport: Sport,
    espn: &'static str,
    teams: bool,
    aliases: &'static [&'static str],
) -> League {
    League {
        slug,
        name,
        sport,
        espn,
        teams,
        aliases,
    }
}

use Sport::*;

pub const LEAGUES: &[League] = &[
    league(
        "premier-league",
        "Premier League",
        Soccer,
        "soccer/eng.1",
        true,
        &["EPL", "English Premier League", "Premier League"],
    ),
    league(
        "a-league-men",
        "A-League Men",
        Soccer,
        "soccer/aus.1",
        true,
        &[
            "A-League",
            "A League",
            "A-League Men",
            "Australian A-League",
        ],
    ),
    league(
        "champions-league",
        "UEFA Champions League",
        Soccer,
        "soccer/uefa.champions",
        true,
        &["UEFA Champions League", "Champions League", "UCL"],
    ),
    league(
        "europa-league",
        "UEFA Europa League",
        Soccer,
        "soccer/uefa.europa",
        true,
        &["UEFA Europa League", "Europa League", "UEL"],
    ),
    league(
        "la-liga",
        "La Liga",
        Soccer,
        "soccer/esp.1",
        true,
        &["La Liga", "LaLiga", "Spanish La Liga"],
    ),
    league(
        "bundesliga",
        "Bundesliga",
        Soccer,
        "soccer/ger.1",
        true,
        &["Bundesliga", "German Bundesliga"],
    ),
    league(
        "serie-a",
        "Serie A",
        Soccer,
        "soccer/ita.1",
        true,
        &["Serie A", "Italian Serie A"],
    ),
    league(
        "ligue-1",
        "Ligue 1",
        Soccer,
        "soccer/fra.1",
        true,
        &["Ligue 1", "Ligue1", "French Ligue 1"],
    ),
    league(
        "fa-cup",
        "FA Cup",
        Soccer,
        "soccer/eng.fa",
        true,
        &["FA Cup", "English FA Cup", "Emirates FA Cup"],
    ),
    league(
        "mls",
        "MLS",
        Soccer,
        "soccer/usa.1",
        true,
        &["MLS", "Major League Soccer"],
    ),
    league(
        "nba",
        "NBA",
        Basketball,
        "basketball/nba",
        true,
        &["NBA", "National Basketball Association"],
    ),
    league(
        "nbl",
        "NBL",
        Basketball,
        "basketball/nbl",
        true,
        &["NBL", "National Basketball League", "NBL Australia"],
    ),
    league(
        "wnba",
        "WNBA",
        Basketball,
        "basketball/wnba",
        true,
        &["WNBA", "Women's National Basketball Association"],
    ),
    league(
        "nrl",
        "NRL",
        RugbyLeague,
        "rugby-league/3",
        true,
        &["NRL", "National Rugby League", "NRL Premiership"],
    ),
    league(
        "super-rugby-pacific",
        "Super Rugby Pacific",
        RugbyUnion,
        "rugby/242041",
        true,
        &["Super Rugby Pacific", "Super Rugby", "Super Rugby AU"],
    ),
    league(
        "urc",
        "United Rugby Championship",
        RugbyUnion,
        "rugby/270557",
        true,
        &["URC", "United Rugby Championship", "Guinness URC"],
    ),
    league(
        "gallagher-premiership",
        "Gallagher Premiership",
        RugbyUnion,
        "rugby/267979",
        true,
        &[
            "Gallagher Premiership",
            "Gallagher Prem",
            "Premiership Rugby",
            "English Premiership Rugby",
        ],
    ),
    league(
        "top-14",
        "Top 14",
        RugbyUnion,
        "rugby/270559",
        true,
        &["Top 14", "Top14", "French Top 14"],
    ),
    league(
        "champions-cup",
        "European Rugby Champions Cup",
        RugbyUnion,
        "rugby/271937",
        true,
        &[
            "Champions Cup",
            "European Rugby Champions Cup",
            "Heineken Champions Cup",
            "Investec Champions Cup",
        ],
    ),
    league(
        "six-nations",
        "Six Nations",
        RugbyUnion,
        "rugby/180659",
        true,
        &["Six Nations", "6 Nations", "Guinness Six Nations"],
    ),
    league(
        "rugby-championship",
        "The Rugby Championship",
        RugbyUnion,
        "rugby/244293",
        true,
        &[
            "The Rugby Championship",
            "Rugby Championship",
            "Tri Nations",
        ],
    ),
    league(
        "rugby-internationals",
        "Rugby internationals",
        RugbyUnion,
        "rugby/289234",
        true,
        &[
            "International Test Match",
            "Rugby Internationals",
            "Test Match",
            "Autumn Nations Series",
            "Autumn Internationals",
            "Summer Internationals",
        ],
    ),
    league(
        "rugby-world-cup",
        "Rugby World Cup",
        RugbyUnion,
        "rugby/164205",
        true,
        &["Rugby World Cup", "RWC"],
    ),
    league(
        "pga-tour",
        "PGA Tour",
        Golf,
        "golf/pga",
        false,
        // The four majors are events inside this feed, not separate leagues.
        &[
            "PGA Tour",
            "PGA",
            "The Masters",
            "Masters Tournament",
            "PGA Championship",
            "US Open",
            "U.S. Open",
            "The Open",
            "The Open Championship",
        ],
    ),
    league(
        "dp-world-tour",
        "DP World Tour",
        Golf,
        "golf/eur",
        false,
        &["DP World Tour", "European Tour", "DP World"],
    ),
    league(
        "liv-golf",
        "LIV Golf",
        Golf,
        "golf/liv",
        false,
        &["LIV Golf", "LIV"],
    ),
    league(
        "lpga-tour",
        "LPGA Tour",
        Golf,
        "golf/lpga",
        false,
        &["LPGA Tour", "LPGA"],
    ),
    league(
        "ufc",
        "UFC",
        Mma,
        "mma/ufc",
        false,
        &["UFC", "Ultimate Fighting Championship"],
    ),
    league(
        "pfl",
        "PFL",
        Mma,
        "mma/pfl",
        false,
        &["PFL", "Professional Fighters League"],
    ),
];

pub fn find(slug: &str) -> Option<&'static League> {
    LEAGUES.iter().find(|l| l.slug == slug)
}

impl League {
    pub fn id(&self) -> String {
        format!("league:{}", self.slug)
    }
}

/// Leagues that match a search, best first: an exact name or alias, then leagues whose name,
/// aliases, sport or slug start with every word typed ("prem" finds the Premier League,
/// "rugby" finds every rugby competition).
pub fn search(q: &str) -> Vec<&'static League> {
    let words = title::tokens(q);
    if words.is_empty() {
        return vec![];
    }
    let key = title::key(q);
    let mut hits: Vec<(u8, usize, &'static League)> = vec![];
    for (i, l) in LEAGUES.iter().enumerate() {
        let exact = std::iter::once(l.name)
            .chain(l.aliases.iter().copied())
            .chain(std::iter::once(l.slug))
            .any(|n| title::key(n) == key);
        if exact {
            hits.push((0, i, l));
            continue;
        }
        let hay: Vec<String> = std::iter::once(l.name)
            .chain(l.aliases.iter().copied())
            .chain([l.slug, l.sport.label()])
            .flat_map(title::tokens)
            .collect();
        if words.iter().all(|w| hay.iter().any(|h| h.starts_with(w))) {
            hits.push((1, i, l));
        }
    }
    hits.sort_by_key(|(rank, i, _)| (*rank, *i));
    hits.into_iter().map(|(_, _, l)| l).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_and_paths_are_unique_and_well_formed() {
        let mut slugs = std::collections::HashSet::new();
        let mut paths = std::collections::HashSet::new();
        for l in LEAGUES {
            assert!(slugs.insert(l.slug), "{} twice", l.slug);
            assert!(paths.insert(l.espn), "{} twice", l.espn);
            assert!(
                (2..=24).contains(&l.slug.len())
                    && l.slug
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "{}",
                l.slug
            );
            // A path is two plain segments; nothing a person typed ever reaches it.
            let parts: Vec<&str> = l.espn.split('/').collect();
            assert_eq!(parts.len(), 2, "{}", l.espn);
            assert!(
                parts.iter().all(|p| !p.is_empty()
                    && p.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')),
                "{}",
                l.espn
            );
            assert!(!l.aliases.is_empty());
            // Golf and fighting leagues have no teams.
            assert_eq!(l.teams, !matches!(l.sport, Golf | Mma), "{}", l.slug);
        }
    }

    #[test]
    fn a_league_is_found_by_its_alias_name_or_a_few_letters() {
        let first = |q: &str| search(q).first().map(|l| l.slug);
        assert_eq!(first("EPL"), Some("premier-league"));
        assert_eq!(first("english premier league"), Some("premier-league"));
        assert_eq!(first("prem"), Some("premier-league"));
        assert_eq!(first("nrl"), Some("nrl"));
        assert_eq!(first("super rugby"), Some("super-rugby-pacific"));
        assert_eq!(first("6 nations"), Some("six-nations"));
        assert_eq!(first("the masters"), Some("pga-tour"));
        assert_eq!(first("Ultimate Fighting Championship"), Some("ufc"));
        assert!(search("rugby").len() >= 8);
        assert!(search("zzzz").is_empty());
        assert!(search("  ").is_empty());
    }
}
