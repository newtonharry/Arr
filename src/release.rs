//! Release-title parsing. Deliberately conservative: anything that cannot be read
//! unambiguously is reported as such so the decision layer can reject it.

use crate::quality::Quality;
use crate::title;
use regex::Regex;
use serde::Serialize;
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Numbering {
    /// No TV numbering found.
    None,
    /// Full single-season pack, e.g. `S01`, `Season 1`, `Series 1 Complete`.
    Season { season: u16 },
    /// Pack spanning several seasons, e.g. `S01-S04`.
    Seasons { seasons: Vec<u16> },
    /// Part of a season, e.g. `S01 Part 1`, `S02 Vol 2`.
    PartialSeason { season: u16 },
    /// One or more episodes of one season, e.g. `S01E03`, `S01E01-E03`, `1x02`.
    Episodes { season: u16, episodes: Vec<u16> },
    /// Whole-series pack without season numbers, e.g. `Complete Series`.
    CompleteSeries,
    /// Date-numbered (daily) release.
    Daily,
    /// Conflicting or unreadable numbering.
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityBasis {
    /// Source and resolution both named in the title.
    Parsed,
    /// Only a resolution was named; source assumed HDTV (native Sonarr/Radarr convention).
    InferredFromResolution,
    /// Contradictory source markers (e.g. WEBRip and BluRay).
    Conflicting,
    /// Nothing usable.
    Unknown,
}

/// Release revision with Sonarr v4 / Radarr v5 semantics: `real` compares first, then
/// `version` (1 = original; PROPER/REPACK/RERIP = 2, or N+1 after a `vN`/`repackN`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct Revision {
    pub version: u8,
    pub real: u8,
}

impl Default for Revision {
    fn default() -> Self {
        Revision {
            version: 1,
            real: 0,
        }
    }
}

impl Ord for Revision {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.real, self.version).cmp(&(other.real, other.version))
    }
}

impl PartialOrd for Revision {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<'de> serde::Deserialize<'de> for Revision {
    /// Accepts the object form and the bare version number stored by earlier builds.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Legacy(u8),
            Full {
                version: u8,
                #[serde(default)]
                real: u8,
            },
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Legacy(version) => Revision { version, real: 0 },
            Repr::Full { version, real } => Revision { version, real },
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ParsedRelease {
    /// Folded title tokens before the year/numbering/quality markers.
    pub title_tokens: Vec<String>,
    pub year: Option<u16>,
    pub numbering: Numbering,
    pub quality: Quality,
    pub quality_basis: QualityBasis,
    /// Native revision: PROPER/REPACK/RERIP/`vN` version and REAL count.
    pub revision: Revision,
    /// Audio languages named in the title (folded English names).
    pub languages: Vec<&'static str>,
    pub multi_language: bool,
    pub dubbed: bool,
    pub three_d: bool,
    pub hardcoded_subs: bool,
    /// Sample/trailer/extras keywords found anywhere (matched against the target title later).
    pub extra_markers: Vec<String>,
    /// Collection/trilogy keywords or year ranges found anywhere.
    pub bundle_markers: Vec<String>,
    pub dangerous_file_type: bool,
}

#[derive(Debug, Clone)]
struct Tok {
    text: String,
    sep: String,
}

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).expect("valid regex"));
    };
}

re!(SXXEYY, r"^s(\d{1,3})((?:e\d{1,4})+)$");
re!(SXX, r"^s(\d{1,3})$");
re!(SXX_MULTI, r"^s\d{1,3}(?:s\d{1,3})+$");
re!(EP_CONT, r"^e?(\d{1,4})$");
re!(EP_NEXT, r"^e(\d{1,4})$");
re!(NXM, r"^(\d{1,2})x(\d{1,3})$");
re!(NXM_CONT, r"^(?:\d{1,2}x)?(\d{1,3})$");
re!(SEASON_NUM, r"^s?(\d{1,3})$");
re!(NUM, r"^(\d{1,3})$");
re!(YEAR, r"^(?:19|20)\d{2}$");
re!(TWO_DIGITS, r"^\d{2}$");
re!(RES, r"^(2160|1080|720|576|540|480|360)[pi]$");
re!(
    DIMENSIONS,
    r"^(?:3840|4096)x2160$|^1920x10(?:80|88)$|^1280x720$"
);
re!(PART, r"^(?:part|pt|vol|volume|disc|disk|cd)(\d{1,2})?$");
re!(PROPER_RE, r"\bproper\b");
re!(REPACK_RE, r"\b(?:repack\d?|rerip\d?)\b");
re!(
    VERSION_RE,
    r"\d[-._ ]?v(\d)[-._ ]|\[v(\d)\]|repack(\d)|rerip(\d)|(?:480|576|720|1080|2160)p[._ ]v(\d)"
);
re!(REAL_RE, r"\bREAL\b");
re!(
    REAL_AFTER,
    r"(?i)\b(?:s\d{1,3}(?:e\d{1,4})*|\d{1,2}x\d{2,3}|(?:19|20)\d{2}|(?:480|576|720|1080|2160)[pi])\b"
);

/// Native revision parsing on the whole name. `REAL` (case-sensitive, as natively) is only
/// counted after the first numbering/year/resolution marker, so a title such as "REAL STEEL"
/// is never mistaken for a revision. That is stricter than native; it can only suppress
/// replacements.
pub fn parse_revision(raw: &str) -> Revision {
    let norm = raw.replace('_', " ").trim().to_lowercase();
    let version_match: Option<u8> = VERSION_RE.captures(&norm).and_then(|c| {
        c.iter()
            .skip(1)
            .flatten()
            .next()
            .and_then(|m| m.as_str().parse().ok())
    });
    let mut version = version_match.unwrap_or(1);
    if PROPER_RE.is_match(&norm) || REPACK_RE.is_match(&norm) {
        version = version_match.map_or(2, |v| v.saturating_add(1));
    }
    let real = REAL_AFTER
        .find(raw)
        .map(|m| REAL_RE.find_iter(&raw[m.end()..]).count())
        .unwrap_or(0);
    Revision {
        version,
        real: u8::try_from(real).unwrap_or(u8::MAX),
    }
}

const STRONG_MARKERS: &[&str] = &[
    "2160p", "1080p", "1080i", "720p", "576p", "480p", "4k", "webdl", "webrip", "bluray", "bdrip",
    "brrip", "bdremux", "remux", "hdtv", "pdtv", "hdtvrip", "dvdrip", "dvdscr", "x264", "x265",
    "h264", "h265", "hevc", "xvid", "divx", "av1", "hdcam", "camrip", "hdts", "telesync",
    "telecine", "hdtc", "bd25", "bd50", "bdmv", "rawhd", "brdisk", "proper", "repack", "rerip",
];

const EXTRA_KEYWORDS: &[&str] = &[
    "sample",
    "trailer",
    "teaser",
    "featurette",
    "featurettes",
    "extras",
    "bonus",
    "soundtrack",
    "ost",
    "interview",
    "interviews",
];
const EXTRA_PHRASES: &[&[&str]] = &[
    &["behind", "the", "scenes"],
    &["deleted", "scenes"],
    &["making", "of"],
    &["special", "features"],
];
const BUNDLE_KEYWORDS: &[&str] = &[
    "trilogy",
    "duology",
    "quadrilogy",
    "pentalogy",
    "hexalogy",
    "collection",
    "anthology",
    "boxset",
    "saga",
];

fn tokenize(input: &str) -> Vec<Tok> {
    let folded = title::fold_chars(input);
    let mut toks = Vec::new();
    let mut sep = String::new();
    let mut cur = String::new();
    for c in folded.chars() {
        if c.is_alphanumeric() {
            cur.push(c);
        } else {
            if !cur.is_empty() {
                toks.push(Tok {
                    text: std::mem::take(&mut cur),
                    sep: std::mem::take(&mut sep),
                });
            }
            sep.push(c);
        }
    }
    if !cur.is_empty() {
        toks.push(Tok { text: cur, sep });
    }
    toks
}

/// Drops a leading `[site/group]` tag or `www.site.tld - ` prefix.
fn strip_prefix(title: &str) -> &str {
    static SITE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^\s*(?:\[\s*)?www\.[^\s\]]+\s*\]?\s*[-–]?\s*|^\s*[a-z0-9-]+\.(?:com|org|net|to|xyz|info)\s+[-–]\s+")
            .expect("valid regex")
    });
    let t = title.trim();
    if let Some(m) = SITE.find(t) {
        return &t[m.end()..];
    }
    if let Some(rest) = t.strip_prefix('[')
        && let Some(end) = rest.find(']')
        && !rest[end + 1..].trim().is_empty()
    {
        return rest[end + 1..].trim_start();
    }
    t
}

fn num<T: std::str::FromStr>(s: &str) -> Option<T> {
    s.parse().ok()
}

fn has_dash(sep: &str) -> bool {
    sep.contains('-') || sep.contains('–')
}

const MAX_RANGE: u16 = 100;

/// Extends an episode list from `toks[i..]`; returns the next unconsumed index.
fn extend_episodes(toks: &[Tok], mut i: usize, eps: &mut Vec<u16>, cont: &Regex) -> Option<usize> {
    while i < toks.len() {
        let t = &toks[i];
        let last = *eps.last()?;
        if has_dash(&t.sep) {
            let Some(c) = cont.captures(&t.text) else {
                break;
            };
            let end: u16 = num(&c[1])?;
            if end <= last || end - last > MAX_RANGE {
                return None;
            }
            eps.extend(last + 1..=end);
        } else if let Some(c) = EP_NEXT.captures(&t.text) {
            eps.push(num(&c[1])?);
        } else {
            break;
        }
        i += 1;
    }
    Some(i)
}

/// After a season marker ending just before `toks[i]`, decides pack/multi/partial/episodes.
/// Returns the numbering (None = unreadable) and the next unconsumed index.
fn after_season(
    toks: &[Tok],
    i: usize,
    season: u16,
    explicit_word: bool,
) -> (Option<Numbering>, usize) {
    let Some(next) = toks.get(i) else {
        return (Some(Numbering::Season { season }), i);
    };
    let range_word = matches!(next.text.as_str(), "to" | "and");
    if has_dash(&next.sep) || range_word {
        let j = if range_word { i + 1 } else { i };
        if let Some(c) = toks.get(j).and_then(|t| SEASON_NUM.captures(&t.text)) {
            let seasons = num::<u16>(&c[1])
                .filter(|&end| end > season && end - season <= MAX_RANGE)
                .map(|end| Numbering::Seasons {
                    seasons: (season..=end).collect(),
                });
            return (seasons, j + 1);
        }
    }
    if let Some(c) = EP_NEXT.captures(&next.text) {
        let mut eps: Vec<u16> = num(&c[1]).into_iter().collect();
        return match extend_episodes(toks, i + 1, &mut eps, &EP_CONT) {
            Some(n) => (
                Some(Numbering::Episodes {
                    season,
                    episodes: eps,
                }),
                n,
            ),
            None => (None, i + 1),
        };
    }
    if explicit_word && matches!(next.text.as_str(), "episode" | "episodes" | "ep") {
        let Some(c) = toks.get(i + 1).and_then(|t| NUM.captures(&t.text)) else {
            return (None, i + 1);
        };
        let mut eps: Vec<u16> = num(&c[1]).into_iter().collect();
        return match extend_episodes(toks, i + 2, &mut eps, &EP_CONT) {
            Some(n) => (
                Some(Numbering::Episodes {
                    season,
                    episodes: eps,
                }),
                n,
            ),
            None => (None, i + 2),
        };
    }
    if SXX.is_match(&next.text) && !has_dash(&next.sep) {
        let mut seasons = vec![season];
        let mut j = i;
        while let Some(c) = toks.get(j).and_then(|t| SXX.captures(&t.text)) {
            seasons.extend(num::<u16>(&c[1]));
            j += 1;
        }
        return (Some(Numbering::Seasons { seasons }), j);
    }
    if let Some(c) = PART.captures(&next.text) {
        let numbered = c.get(1).is_some() || toks.get(i + 1).is_some_and(|t| NUM.is_match(&t.text));
        if numbered {
            return (Some(Numbering::PartialSeason { season }), i + 1);
        }
    }
    (Some(Numbering::Season { season }), i)
}

/// Finds every TV numbering marker. Returns the merged numbering and the index of the first.
fn numbering(toks: &[Tok]) -> (Numbering, Option<usize>) {
    let mut found: Vec<(usize, Option<Numbering>)> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i].text.as_str();
        let start = i;
        let (parsed, next) = if let Some(c) = SXXEYY.captures(t) {
            let mut eps: Vec<u16> = c[2]
                .split('e')
                .filter(|s| !s.is_empty())
                .filter_map(num)
                .collect();
            match (
                num::<u16>(&c[1]),
                extend_episodes(toks, i + 1, &mut eps, &EP_CONT),
            ) {
                (Some(season), Some(n)) => (
                    Some(Numbering::Episodes {
                        season,
                        episodes: eps,
                    }),
                    n,
                ),
                _ => (None, i + 1),
            }
        } else if SXX_MULTI.is_match(t) {
            let seasons = t
                .split('s')
                .filter(|s| !s.is_empty())
                .filter_map(num)
                .collect();
            (Some(Numbering::Seasons { seasons }), i + 1)
        } else if let Some(c) = SXX.captures(t) {
            match num::<u16>(&c[1]) {
                Some(s) => after_season(toks, i + 1, s, false),
                None => (None, i + 1),
            }
        } else if let Some(c) = NXM.captures(t) {
            let mut eps: Vec<u16> = num(&c[2]).into_iter().collect();
            match (
                num::<u16>(&c[1]),
                extend_episodes(toks, i + 1, &mut eps, &NXM_CONT),
            ) {
                (Some(season), Some(n)) => (
                    Some(Numbering::Episodes {
                        season,
                        episodes: eps,
                    }),
                    n,
                ),
                _ => (None, i + 1),
            }
        } else if matches!(t, "season" | "seasons" | "series")
            && let Some(c) = toks.get(i + 1).and_then(|n| NUM.captures(&n.text))
        {
            match num::<u16>(&c[1]) {
                Some(s) => after_season(toks, i + 2, s, true),
                None => (None, i + 2),
            }
        } else if t == "complete"
            && toks
                .get(i + 1)
                .is_some_and(|n| matches!(n.text.as_str(), "series" | "seasons" | "box" | "boxset"))
            && !toks.get(i + 2).is_some_and(|n| NUM.is_match(&n.text))
        {
            (Some(Numbering::CompleteSeries), i + 2)
        } else if YEAR.is_match(t)
            && i > 0
            && toks
                .get(i + 1)
                .is_some_and(|m| TWO_DIGITS.is_match(&m.text) && !m.sep.is_empty())
            && toks
                .get(i + 2)
                .is_some_and(|d| TWO_DIGITS.is_match(&d.text) && !d.sep.is_empty())
            && num::<u8>(&toks[i + 1].text).is_some_and(|m| (1..=12).contains(&m))
            && num::<u8>(&toks[i + 2].text).is_some_and(|d| (1..=31).contains(&d))
        {
            (Some(Numbering::Daily), i + 3)
        } else {
            i += 1;
            continue;
        };
        found.push((start, parsed));
        i = next.max(start + 1);
    }
    let Some(first) = found.first().map(|(idx, _)| *idx) else {
        return (Numbering::None, None);
    };
    let mut merged: Option<Numbering> = None;
    for (_, n) in found {
        let Some(mut n) = n else {
            return (Numbering::Ambiguous, Some(first));
        };
        if let Numbering::Episodes { episodes, .. } = &mut n {
            episodes.sort_unstable();
            episodes.dedup();
            if episodes.is_empty()
                || episodes.contains(&0)
                || episodes.len() > usize::from(MAX_RANGE)
            {
                return (Numbering::Ambiguous, Some(first));
            }
        }
        if let Numbering::Seasons { seasons } = &mut n {
            seasons.sort_unstable();
            seasons.dedup();
            match seasons.as_slice() {
                [] => return (Numbering::Ambiguous, Some(first)),
                [only] => n = Numbering::Season { season: *only },
                _ => {}
            }
        }
        match &merged {
            None => merged = Some(n),
            Some(m) if *m == n => {}
            Some(_) => return (Numbering::Ambiguous, Some(first)),
        }
    }
    let merged = merged.unwrap_or(Numbering::Ambiguous);
    let zero_season = match &merged {
        Numbering::Season { season }
        | Numbering::PartialSeason { season }
        | Numbering::Episodes { season, .. } => *season == 0,
        Numbering::Seasons { seasons } => seasons.contains(&0),
        _ => false,
    };
    if zero_season {
        return (Numbering::Ambiguous, Some(first));
    }
    (merged, Some(first))
}

fn adjacent(toks: &[Tok], a: &str, b: &str) -> bool {
    phrase(toks, &[a, b])
}

fn phrase(toks: &[Tok], words: &[&str]) -> bool {
    toks.windows(words.len())
        .any(|w| w.iter().zip(words).all(|(t, word)| t.text == *word))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    Bluray,
    Webdl,
    Webrip,
    Tv,
    Dvd,
    Dvdr,
    Cam,
    Telesync,
    Telecine,
    Workprint,
    Screener,
    Regional,
}

fn quality(post: &[Tok]) -> (Quality, QualityBasis) {
    use Quality::*;
    let has = |t: &str| post.iter().any(|x| x.text == t);
    let any = |ts: &[&str]| post.iter().any(|x| ts.contains(&x.text.as_str()));

    let res = post.iter().find_map(|t| {
        if let Some(c) = RES.captures(&t.text) {
            return num::<u16>(&c[1]);
        }
        if DIMENSIONS.is_match(&t.text) {
            return t.text.split('x').nth(1).and_then(num::<u16>);
        }
        match t.text.as_str() {
            "4k" | "uhd" => Some(2160),
            _ => None,
        }
    });
    let res = res.map(|r| match r {
        540 | 360 => 480,
        r => r,
    });

    if any(&["brdisk", "bd25", "bd50", "bdmv", "bdiso"]) || adjacent(post, "complete", "bluray") {
        return (Brdisk, QualityBasis::Parsed);
    }
    if has("rawhd") || adjacent(post, "raw", "hd") {
        return (Rawhd, QualityBasis::Parsed);
    }

    let mut sources: Vec<Source> = Vec::new();
    let mut add = |s: Source| {
        if !sources.contains(&s) {
            sources.push(s);
        }
    };
    if any(&["bluray", "bdrip", "brrip", "bdremux", "uhdbluray"]) || adjacent(post, "blu", "ray") {
        add(Source::Bluray);
    }
    if any(&["webdl", "webhd"]) || adjacent(post, "web", "dl") {
        add(Source::Webdl);
    }
    if any(&["webrip", "webhdrip"]) || adjacent(post, "web", "rip") {
        add(Source::Webrip);
    }
    if has("web") && !adjacent(post, "web", "dl") && !adjacent(post, "web", "rip") {
        add(Source::Webdl);
    }
    if any(&[
        "hdtv", "pdtv", "sdtv", "dsr", "dsrip", "tvrip", "hdtvrip", "satrip",
    ]) {
        add(Source::Tv);
    }
    if any(&["dvdr", "dvd9", "dvd5"]) || adjacent(post, "dvd", "r") {
        add(Source::Dvdr);
    } else if any(&["dvdrip", "dvd"]) {
        add(Source::Dvd);
    }
    if any(&["cam", "camrip", "hdcam", "hqcam"]) {
        add(Source::Cam);
    }
    if any(&["ts", "telesync", "hdts", "tsrip", "hdtsrip", "pdvd"]) {
        add(Source::Telesync);
    }
    if any(&["tc", "telecine", "hdtc"]) {
        add(Source::Telecine);
    }
    if any(&["workprint", "wp"]) {
        add(Source::Workprint);
    }
    if any(&[
        "dvdscr",
        "dvdscreener",
        "screener",
        "scr",
        "bdscr",
        "webscr",
    ]) {
        add(Source::Screener);
    }
    if any(&["r5", "r6", "regional"]) {
        add(Source::Regional);
    }
    let remux = any(&["remux", "bdremux"]);

    // Pre-release sources dominate anything else they appear with (e.g. "HDCAM 720p WEB").
    for (src, q) in [
        (Source::Cam, Cam),
        (Source::Telesync, Telesync),
        (Source::Telecine, Telecine),
        (Source::Workprint, Workprint),
        (Source::Screener, Dvdscr),
        (Source::Regional, Regional),
    ] {
        if sources.contains(&src) {
            return (q, QualityBasis::Parsed);
        }
    }
    if sources.len() > 1 {
        return (Unknown, QualityBasis::Conflicting);
    }
    let parsed = |q| (q, QualityBasis::Parsed);
    match (sources.first(), res) {
        (Some(Source::Bluray), r) if remux => match r {
            Some(2160) => parsed(Remux2160),
            Some(1080) | None => parsed(Remux1080),
            _ => (Unknown, QualityBasis::Unknown),
        },
        (None, r) if remux => match r {
            Some(2160) => parsed(Remux2160),
            Some(1080) => parsed(Remux1080),
            _ => (Unknown, QualityBasis::Unknown),
        },
        (Some(Source::Bluray), Some(2160)) => parsed(Bluray2160),
        (Some(Source::Bluray), Some(1080)) => parsed(Bluray1080),
        (Some(Source::Bluray), Some(720)) => parsed(Bluray720),
        (Some(Source::Bluray), Some(576)) => parsed(Bluray576),
        (Some(Source::Bluray), Some(480)) => parsed(Bluray480),
        (Some(Source::Webdl), Some(2160)) => parsed(Webdl2160),
        (Some(Source::Webdl), Some(1080)) => parsed(Webdl1080),
        (Some(Source::Webdl), Some(720)) => parsed(Webdl720),
        (Some(Source::Webdl), Some(480 | 576)) => parsed(Webdl480),
        (Some(Source::Webrip), Some(2160)) => parsed(Webrip2160),
        (Some(Source::Webrip), Some(1080)) => parsed(Webrip1080),
        (Some(Source::Webrip), Some(720)) => parsed(Webrip720),
        (Some(Source::Webrip), Some(480 | 576)) => parsed(Webrip480),
        (Some(Source::Tv), Some(2160)) => parsed(Hdtv2160),
        (Some(Source::Tv), Some(1080)) => parsed(Hdtv1080),
        (Some(Source::Tv), Some(720)) => parsed(Hdtv720),
        (Some(Source::Tv), _) => parsed(Sdtv),
        (Some(Source::Dvdr), _) => parsed(Dvdr),
        (Some(Source::Dvd), _) => parsed(Dvd),
        (None, Some(2160)) => (Hdtv2160, QualityBasis::InferredFromResolution),
        (None, Some(1080)) => (Hdtv1080, QualityBasis::InferredFromResolution),
        (None, Some(720)) => (Hdtv720, QualityBasis::InferredFromResolution),
        (None, Some(_)) => (Sdtv, QualityBasis::InferredFromResolution),
        _ => (Unknown, QualityBasis::Unknown),
    }
}

const LANGUAGE_TOKENS: &[(&str, &str)] = &[
    ("english", "english"),
    ("eng", "english"),
    ("french", "french"),
    ("truefrench", "french"),
    ("vff", "french"),
    ("vfq", "french"),
    ("vf", "french"),
    ("vf2", "french"),
    ("vfi", "french"),
    ("vostfr", "french"),
    ("subfrench", "french"),
    ("german", "german"),
    ("deutsch", "german"),
    ("ger", "german"),
    ("spanish", "spanish"),
    ("castellano", "spanish"),
    ("espanol", "spanish"),
    ("esp", "spanish"),
    ("latino", "spanish"),
    ("italian", "italian"),
    ("ita", "italian"),
    ("russian", "russian"),
    ("rus", "russian"),
    ("ukrainian", "ukrainian"),
    ("ukr", "ukrainian"),
    ("polish", "polish"),
    ("pldub", "polish"),
    ("portuguese", "portuguese"),
    ("dublado", "portuguese"),
    ("dutch", "dutch"),
    ("flemish", "flemish"),
    ("swedish", "swedish"),
    ("swe", "swedish"),
    ("norwegian", "norwegian"),
    ("danish", "danish"),
    ("finnish", "finnish"),
    ("icelandic", "icelandic"),
    ("czech", "czech"),
    ("hungarian", "hungarian"),
    ("hun", "hungarian"),
    ("greek", "greek"),
    ("turkish", "turkish"),
    ("arabic", "arabic"),
    ("hebrew", "hebrew"),
    ("heb", "hebrew"),
    ("persian", "persian"),
    ("farsi", "persian"),
    ("hindi", "hindi"),
    ("tamil", "tamil"),
    ("telugu", "telugu"),
    ("malayalam", "malayalam"),
    ("kannada", "kannada"),
    ("bengali", "bengali"),
    ("marathi", "marathi"),
    ("punjabi", "punjabi"),
    ("japanese", "japanese"),
    ("jap", "japanese"),
    ("korean", "korean"),
    ("kor", "korean"),
    ("chinese", "chinese"),
    ("mandarin", "chinese"),
    ("cantonese", "chinese"),
    ("thai", "thai"),
    ("vietnamese", "vietnamese"),
    ("romanian", "romanian"),
    ("bulgarian", "bulgarian"),
    ("lithuanian", "lithuanian"),
    ("latvian", "latvian"),
    ("estonian", "estonian"),
    ("slovak", "slovak"),
    ("slovenian", "slovenian"),
    ("serbian", "serbian"),
    ("croatian", "croatian"),
];

fn is_hardcoded_sub_token(t: &str) -> bool {
    if matches!(t, "hc" | "subbed" | "hardcoded" | "hardsub" | "hardsubs") {
        return true;
    }
    // Radarr treats `<word>SUB(S)` (KORSUB, ESub, NORDICSUBS) as hardcoded, except SOFT/MULTI.
    let stem = t.strip_suffix("subs").or_else(|| t.strip_suffix("sub"));
    matches!(stem, Some(s) if !s.is_empty() && s != "soft" && s != "multi" && s.chars().all(char::is_alphabetic))
}

pub fn parse(raw_title: &str) -> ParsedRelease {
    let lowered = raw_title.trim().to_ascii_lowercase();
    let dangerous_file_type = [
        ".exe", ".msi", ".bat", ".cmd", ".lnk", ".scr", ".vbs", ".apk", ".js", ".zipx",
    ]
    .iter()
    .any(|ext| lowered.ends_with(ext));
    let toks = tokenize(strip_prefix(raw_title));
    let (numbering, first_numbering) = numbering(&toks);

    let first_strong = toks.iter().position(|t| {
        STRONG_MARKERS.contains(&t.text.as_str())
            || RES.is_match(&t.text)
            || DIMENSIONS.is_match(&t.text)
    });
    let first_complete = toks.windows(2).position(|w| {
        w[0].text == "complete"
            && matches!(
                w[1].text.as_str(),
                "series" | "season" | "seasons" | "bluray" | "uhd" | "box" | "boxset"
            )
    });
    let boundary = [first_numbering, first_strong, first_complete]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(toks.len());

    // The release year is the last year-like token in the title region (never the first token,
    // so "1923 S01" and "2001 A Space Odyssey 1968" keep their numeric titles).
    let year_idx = (1..boundary).rev().find(|&i| YEAR.is_match(&toks[i].text));
    let (title_end, year, post_start) = match year_idx {
        Some(i) => (i, num::<u16>(&toks[i].text), i + 1),
        None => (boundary, None, boundary),
    };
    let title_tokens: Vec<String> = toks[..title_end].iter().map(|t| t.text.clone()).collect();
    let post = &toks[post_start.min(toks.len())..];

    let (quality, quality_basis) = quality(post);
    let revision = parse_revision(raw_title);

    let mut languages: Vec<&'static str> = Vec::new();
    for (i, t) in post.iter().enumerate() {
        // "ENG SUBS" names a subtitle language, not the audio.
        let subtitle = post.get(i + 1).is_some_and(|n| {
            matches!(
                n.text.as_str(),
                "sub" | "subs" | "subbed" | "subtitle" | "subtitles" | "srt"
            )
        });
        if let Some((_, lang)) = LANGUAGE_TOKENS.iter().find(|(tok, _)| *tok == t.text)
            && !subtitle
            && !languages.contains(lang)
        {
            languages.push(lang);
        }
    }
    let multi_language = post.iter().any(|t| {
        matches!(
            t.text.as_str(),
            "multi" | "multilang" | "multilanguage" | "nordic" | "dual"
        )
    });
    let dubbed = post
        .iter()
        .any(|t| matches!(t.text.as_str(), "dubbed" | "dub"));
    let three_d = post
        .iter()
        .any(|t| matches!(t.text.as_str(), "3d" | "hsbs" | "sbs" | "hou" | "htab"));
    let hardcoded_subs = post.iter().any(|t| is_hardcoded_sub_token(&t.text));

    let mut extra_markers: Vec<String> = toks
        .iter()
        .filter(|t| EXTRA_KEYWORDS.contains(&t.text.as_str()))
        .map(|t| t.text.clone())
        .collect();
    for words in EXTRA_PHRASES {
        if phrase(&toks, words) {
            extra_markers.push(words.join(" "));
        }
    }
    let mut bundle_markers: Vec<String> = toks
        .iter()
        .filter(|t| BUNDLE_KEYWORDS.contains(&t.text.as_str()))
        .map(|t| t.text.clone())
        .collect();
    if toks.windows(2).any(|w| {
        YEAR.is_match(&w[0].text)
            && YEAR.is_match(&w[1].text)
            && has_dash(&w[1].sep)
            && w[0].text != w[1].text
    }) {
        bundle_markers.push("year range".into());
    }
    if phrase(&toks, &["double", "feature"]) {
        bundle_markers.push("double feature".into());
    }

    ParsedRelease {
        title_tokens,
        year,
        numbering,
        quality,
        quality_basis,
        revision,
        languages,
        multi_language,
        dubbed,
        three_d,
        hardcoded_subs,
        extra_markers,
        bundle_markers,
        dangerous_file_type,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Numbering::*;

    fn n(t: &str) -> Numbering {
        parse(t).numbering
    }

    #[test]
    fn season_and_episode_forms() {
        assert_eq!(
            n("Slow.Horses.S01.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb"),
            Season { season: 1 }
        );
        assert_eq!(
            n("Slow Horses Season 1 Complete 720p"),
            Season { season: 1 }
        );
        assert_eq!(n("Slow Horses Series 1 1080p"), Season { season: 1 });
        assert_eq!(
            n("Slow Horses (2022) S01 COMPLETE 1080p"),
            Season { season: 1 }
        );
        assert_eq!(
            n("Slow.Horses.S01E03.1080p.WEB.h264-GROUP"),
            Episodes {
                season: 1,
                episodes: vec![3]
            }
        );
        assert_eq!(
            n("Slow.Horses.S01E01E02.1080p.WEB"),
            Episodes {
                season: 1,
                episodes: vec![1, 2]
            }
        );
        assert_eq!(
            n("Slow.Horses.S01E01-E03.1080p.WEB"),
            Episodes {
                season: 1,
                episodes: vec![1, 2, 3]
            }
        );
        assert_eq!(
            n("Slow Horses S01E02-03 720p"),
            Episodes {
                season: 1,
                episodes: vec![2, 3]
            }
        );
        assert_eq!(
            n("Slow Horses S01 E04 720p"),
            Episodes {
                season: 1,
                episodes: vec![4]
            }
        );
        assert_eq!(
            n("Fargo 5x03 HDTV"),
            Episodes {
                season: 5,
                episodes: vec![3]
            }
        );
        assert_eq!(
            n("Fargo 5x01-5x03 HDTV"),
            Episodes {
                season: 5,
                episodes: vec![1, 2, 3]
            }
        );
        assert_eq!(
            n("Fargo Season 5 Episode 1-3 1080p"),
            Episodes {
                season: 5,
                episodes: vec![1, 2, 3]
            }
        );
    }

    #[test]
    fn multi_season_and_partial_packs() {
        assert_eq!(
            n("Slow.Horses.S01-S04.1080p.WEB"),
            Seasons {
                seasons: vec![1, 2, 3, 4]
            }
        );
        assert_eq!(
            n("Slow Horses S01-04 COMPLETE 1080p"),
            Seasons {
                seasons: vec![1, 2, 3, 4]
            }
        );
        assert_eq!(
            n("Slow Horses Seasons 1 to 4 1080p"),
            Seasons {
                seasons: vec![1, 2, 3, 4]
            }
        );
        assert_eq!(
            n("Slow Horses Season 1-4 720p"),
            Seasons {
                seasons: vec![1, 2, 3, 4]
            }
        );
        assert_eq!(
            n("Slow.Horses.S01S02.1080p"),
            Seasons {
                seasons: vec![1, 2]
            }
        );
        assert_eq!(
            n("Slow Horses S01 S02 S03 1080p"),
            Seasons {
                seasons: vec![1, 2, 3]
            }
        );
        assert_eq!(n("Fargo S05 Part 1 1080p"), PartialSeason { season: 5 });
        assert_eq!(n("Fargo.S05.Vol2.1080p"), PartialSeason { season: 5 });
        assert_eq!(n("Fargo Complete Series 1080p BluRay"), CompleteSeries);
    }

    #[test]
    fn ambiguous_and_other_numbering() {
        assert_eq!(n("Show S01E01 S02E05 720p"), Ambiguous);
        assert_eq!(n("Show Season 1 S01E02 720p"), Ambiguous);
        assert_eq!(n("Show S01E03-E01 720p"), Ambiguous);
        assert_eq!(n("Show S00E01 720p"), Ambiguous);
        assert_eq!(n("The Daily Show 2024.01.15 720p"), Daily);
        assert_eq!(n("The.Matrix.1999.1080p.BluRay.x264"), None);
        // "Complete" inside a movie title is just a word.
        assert_eq!(n("A Complete Unknown 2024 1080p WEB-DL"), None);
    }

    #[test]
    fn title_and_year_split() {
        let p = parse("The.Matrix.1999.1080p.BluRay.x264-GROUP");
        assert_eq!(p.title_tokens, ["the", "matrix"]);
        assert_eq!(p.year, Some(1999));
        let p = parse("2001.A.Space.Odyssey.1968.1080p.BluRay");
        assert_eq!(p.title_tokens, ["2001", "a", "space", "odyssey"]);
        assert_eq!(p.year, Some(1968));
        let p = parse("1923.S01E01.1080p.WEB");
        assert_eq!(p.title_tokens, ["1923"]);
        assert_eq!(p.year, Option::None);
        let p = parse("WAR.2026.S01E01.1080p.WEB");
        assert_eq!(p.title_tokens, ["war"]);
        assert_eq!(p.year, Some(2026));
        let p = parse("A Complete Unknown 2024 1080p WEB-DL");
        assert_eq!(p.title_tokens, ["a", "complete", "unknown"]);
        let p = parse("[ www.Torrenting.com ] - Slow.Horses.S01E02.720p.WEB");
        assert_eq!(p.title_tokens, ["slow", "horses"]);
        let p = parse("[TGx] Leviticus (2025) 1080p WEBRip");
        assert_eq!(p.title_tokens, ["leviticus"]);
        assert_eq!(p.year, Some(2025));
        let p = parse("Amélie.2001.1080p.BluRay");
        assert_eq!(p.title_tokens, ["amelie"]);
    }

    #[test]
    fn quality_detection() {
        let q = |t: &str| {
            let p = parse(t);
            (p.quality, p.quality_basis)
        };
        use Quality::*;
        assert_eq!(
            q("Show.S01E01.1080p.WEB-DL.x264"),
            (Webdl1080, QualityBasis::Parsed)
        );
        assert_eq!(
            q("Show.S01E01.1080p.WEB.h264"),
            (Webdl1080, QualityBasis::Parsed)
        );
        assert_eq!(
            q("Show.S01E01.720p.WEBRip.x264"),
            (Webrip720, QualityBasis::Parsed)
        );
        assert_eq!(
            q("Show.S01E01.720p.HDTV.x264"),
            (Hdtv720, QualityBasis::Parsed)
        );
        assert_eq!(q("Show.S01E01.HDTV.x264"), (Sdtv, QualityBasis::Parsed));
        assert_eq!(
            q("Movie.2020.2160p.UHD.BluRay.REMUX.HEVC"),
            (Remux2160, QualityBasis::Parsed)
        );
        assert_eq!(
            q("Movie.2020.1080p.BluRay.x264"),
            (Bluray1080, QualityBasis::Parsed)
        );
        assert_eq!(
            q("Movie.2020.1080p.BRRip.x264"),
            (Bluray1080, QualityBasis::Parsed)
        );
        assert_eq!(
            q("Movie.2020.COMPLETE.BLURAY-GRP"),
            (Brdisk, QualityBasis::Parsed)
        );
        assert_eq!(q("Movie.2020.HDCAM.x264"), (Cam, QualityBasis::Parsed));
        assert_eq!(
            q("Movie 2020 720p HDTS x264"),
            (Telesync, QualityBasis::Parsed)
        );
        assert_eq!(
            q("Movie 2020 1080p TS WEB"),
            (Telesync, QualityBasis::Parsed)
        );
        assert_eq!(
            q("Show.S01E01.1080p.x265"),
            (Hdtv1080, QualityBasis::InferredFromResolution)
        );
        assert_eq!(q("Show.S01E01.WEB-DL"), (Unknown, QualityBasis::Unknown));
        assert_eq!(
            q("Show.S01E01.1080p.WEBRip.BluRay"),
            (Unknown, QualityBasis::Conflicting)
        );
        assert_eq!(q("Show S01E01"), (Unknown, QualityBasis::Unknown));
        // A title word must not decide quality.
        assert_eq!(
            q("Cam.2018.1080p.WEB-DL.x264"),
            (Webdl1080, QualityBasis::Parsed)
        );
    }

    #[test]
    fn revision_languages_and_flags() {
        let p = parse("Show.S01E01.PROPER.REPACK.1080p.WEB");
        assert_eq!(
            p.revision,
            Revision {
                version: 2,
                real: 0
            },
            "native: proper and repack both set version 2"
        );
        assert_eq!(
            parse_revision("Show.S01E01.1080p.WEB-DL"),
            Revision::default()
        );
        assert_eq!(
            parse_revision("Show.S01E01.REPACK2.1080p.WEB-DL").version,
            3
        );
        assert_eq!(parse_revision("Show.S01E01v2.1080p.WEB-DL").version, 2);
        assert_eq!(
            parse_revision("Show.S01E01.REAL.PROPER.1080p.WEB"),
            Revision {
                version: 2,
                real: 1
            }
        );
        assert_eq!(
            parse_revision("REAL.STEEL.2011.1080p.BluRay").real,
            0,
            "title word is not a revision"
        );
        assert_eq!(
            parse_revision("Show.S01E01.real.1080p.WEB").real,
            0,
            "REAL is case-sensitive"
        );
        assert!(
            Revision {
                version: 1,
                real: 1
            } > Revision {
                version: 3,
                real: 0
            },
            "real compares first"
        );
        let p = parse("Movie.2020.FRENCH.1080p.WEB-DL");
        assert_eq!(p.languages, ["french"]);
        let p = parse("Movie.2020.MULTi.1080p.BluRay");
        assert!(p.multi_language && p.languages.is_empty());
        let p = parse("Movie.2020.iTA-ENG.1080p.BluRay");
        assert_eq!(p.languages, ["italian", "english"]);
        let p = parse("Movie.2020.1080p.WEB-DL.ESub");
        assert!(p.hardcoded_subs);
        let p = parse("Movie.2020.KORSUB.1080p.HDRip");
        assert!(p.hardcoded_subs);
        let p = parse("Movie.2020.1080p.BluRay.MultiSubs");
        assert!(!p.hardcoded_subs);
        let p = parse("Movie.2020.1080p.BluRay.Half-SBS.3D");
        assert!(p.three_d);
        let p = parse("Movie.2020.1080p.BluRay.Sample");
        assert_eq!(p.extra_markers, ["sample"]);
        let p = parse("Movie 2020 Behind the Scenes 1080p");
        assert_eq!(p.extra_markers, ["behind the scenes"]);
        let p = parse("Movie.2020.FRENCH.1080p.WEB-DL.ENG.SUBS");
        assert_eq!(p.languages, ["french"]);
        let p = parse("The.Matrix.Trilogy.1999-2003.1080p.BluRay");
        assert!(p.bundle_markers.contains(&"trilogy".to_string()));
        assert!(p.bundle_markers.contains(&"year range".to_string()));
        let p = parse("The Matrix 1999 1080p WEB-DL.exe");
        assert!(p.dangerous_file_type);
    }
}
