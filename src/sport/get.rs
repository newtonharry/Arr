//! Phase D: picking a copy of an event by hand, filing it, and telling Plex.
//!
//! An event joins the movie and series download machinery as `MediaId::Event` (the grab, its
//! target, the release link, the journal, the poller and the client are shared) and keeps out of
//! everything else: its files are `sport_files`, its names come from the stored event and never
//! from the torrent, and the scan, the catalogue and the lists never see it.

use super::Part;
use super::catalog::{self, Sport};
use super::data::{self, EventRow};
use super::find::Candidate;
use super::matcher::{Confidence, RefusalCode};
use crate::api::AppState;
use crate::downloads::{self, ReleaseLink};
use crate::ids::{EventRef, MediaId};
use crate::release::Revision;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::PathBuf;
use time::OffsetDateTime;

// ----------------------------------------------------------------------- ids and the copy ref

/// The media id of an event part, from a stored event.
pub fn target_of(ev: &EventRow, part: Part) -> Option<MediaId> {
    let mut parts = ev.id.split(':');
    let (Some("event"), Some(slug), Some(n), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let league = catalog::find(slug)?;
    Some(MediaId::Event(EventRef {
        league: league.slug,
        espn: n.parse().ok()?,
        part,
    }))
}

/// What a person calls a copy: built from the stored event and the part, never from the
/// release's own title (which can carry anything).
pub fn display_name(ev: &EventRow, part: Part) -> String {
    match part {
        Part::Whole => ev.name.clone(),
        other => format!("{} - {}", ev.name, other.label()),
    }
}

/// How long a listed copy can be picked.
pub const LISTED_FOR: time::Duration = time::Duration::days(7);

/// The release link of a listed copy. The title is the event's own name, so no release text
/// reaches the grab, its views or the history.
pub fn link_for(ev: &EventRow, c: &Candidate, now_target: MediaId) -> ReleaseLink {
    let part = match now_target {
        MediaId::Event(e) => e.part,
        MediaId::Movie { .. } | MediaId::Series(_) | MediaId::Season(..) | MediaId::Episode(..) => {
            Part::Whole
        }
    };
    ReleaseLink {
        release_ref: c.release_ref.clone(),
        target: now_target,
        media_id: now_target,
        info_hash: c
            .release
            .info_hash
            .as_deref()
            .map(|h| h.trim().to_ascii_lowercase()),
        magnet: c
            .release
            .magnet_url
            .as_ref()
            .map(|l| l.expose().to_string()),
        download_url: c
            .release
            .download_url
            .as_ref()
            .map(|l| l.expose().to_string()),
        title: display_name(ev, part).chars().take(300).collect(),
        size: c.release.size,
        indexer_id: c.release.indexer_id,
        quality: c.parsed.quality,
        revision: Revision::default(),
        full_season: false,
        covers: vec![now_target],
        replaces: vec![],
        supersedes: vec![],
        interactive: true,
    }
}

/// Saves the links of the copies a copies answer lists, and what each was judged to be.
/// A copy is picked by its `release_ref` (a one-way hash of the release's identity), and only
/// a copy saved here, for this event, with this part, can be picked: the reference cannot name
/// anything that was not listed. The rows live seven days from the last time they were listed.
pub fn save_listed(
    c: &Connection,
    ev: &EventRow,
    listed: &[Candidate],
    now: OffsetDateTime,
) -> Result<()> {
    let mut links = vec![];
    for cand in listed {
        let Some(target) = target_of(ev, cand.m.part) else {
            continue;
        };
        links.push(link_for(ev, cand, target));
        c.execute(
            "INSERT INTO sport_picks (release_ref, target, confidence, reason, seen_utc) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (release_ref, target) DO UPDATE SET confidence = excluded.confidence, reason = excluded.reason, seen_utc = excluded.seen_utc",
            params![
                cand.release_ref,
                target.to_string(),
                match cand.m.confidence {
                    Confidence::Sure => "sure",
                    Confidence::Likely => "likely",
                },
                cand.m.reason,
                crate::import::rfc3339(now)
            ],
        )?;
    }
    downloads::save_links(c, &links, now)
}

/// A copy that was listed for this event: its link and how sure it was.
pub struct Listed {
    pub link: ReleaseLink,
    pub sure: bool,
    pub reason: Option<String>,
}

/// The listed copy `release_ref` of the event `event_id` (any part), or none. A reference saved
/// for another event never answers, whatever it is.
///
/// A listing older than `LISTED_FOR` is not found: the lookup itself enforces the cutoff, and the
/// daily prune only reclaims the storage.
pub fn load_listed(
    c: &Connection,
    event_id: &str,
    release_ref: &str,
    now: OffsetDateTime,
) -> Result<Option<Listed>> {
    let like = format!("{event_id}:%");
    let cutoff = crate::import::rfc3339(now - LISTED_FOR);
    let target: Option<(String, String, Option<String>)> = c
        .query_row(
            "SELECT p.target, p.confidence, p.reason FROM sport_picks p
             JOIN release_links l ON l.release_ref = p.release_ref AND l.target = p.target
             WHERE p.release_ref = ?1 AND (p.target = ?2 OR p.target LIKE ?3) AND p.seen_utc >= ?4
             ORDER BY p.seen_utc DESC LIMIT 1",
            params![release_ref, event_id, like, cutoff],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((target, confidence, reason)) = target else {
        return Ok(None);
    };
    let Ok(t) = target.parse::<MediaId>() else {
        return Ok(None);
    };
    let fresh: bool = c
        .query_row(
            "SELECT 1 FROM release_links WHERE release_ref = ?1 AND target = ?2 AND seen_at >= ?3",
            params![release_ref, target, downloads::ts(now - LISTED_FOR)],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !fresh {
        return Ok(None);
    }
    let link = downloads::load_link(c, release_ref, Some(t))?.filter(|l| l.target == t);
    Ok(link.map(|link| Listed {
        link,
        sure: confidence == "sure",
        reason,
    }))
}

// -------------------------------------------------------------------------- gates

/// Whether an event can be picked now: it exists, is not cancelled, postponed or unlisted, and is
/// over by Arr's reckoning. The text is plain; `None` means it can.
pub fn gate_now(c: &Connection, e: EventRef, now: OffsetDateTime) -> Result<Option<String>> {
    let Some(ev) = data::event(c, &e.event_id())? else {
        return Ok(Some(
            "the event is no longer in Arr's schedule; look for it again".into(),
        ));
    };
    let Some(league) = catalog::find(&ev.league) else {
        return Ok(Some("the event's league is not one Arr knows".into()));
    };
    Ok(match super::find::availability(&ev, league.sport, now) {
        Ok(()) => None,
        Err("not_over_yet") => Some("the event is not over yet".into()),
        Err(why) => Some(format!("the event is {why}")),
    })
}

/// What a matcher refusal says about a payload's own name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// The name positively says it is not this event (or not this part): the download fails.
    Contradiction,
    /// The name does not say enough to tell (a terse file name): tolerated, search already
    /// judged the release.
    Missing,
}

/// Every refusal code, classified once. Exhaustive on purpose: a new code has to be put on one
/// side here before the crate builds.
pub fn evidence_of(code: RefusalCode) -> Evidence {
    match code {
        RefusalCode::NotAnEvent
        | RefusalCode::OtherLeague
        | RefusalCode::AmbiguousLeague
        | RefusalCode::QualifiedLeague
        | RefusalCode::OtherCompetition
        | RefusalCode::NotTheEvent
        | RefusalCode::WrongDate
        | RefusalCode::TeamMismatch
        | RefusalCode::RoundMismatch
        | RefusalCode::WrongSeason
        | RefusalCode::SeriesMismatch
        | RefusalCode::NumberMismatch
        | RefusalCode::FightersMismatch
        | RefusalCode::TournamentMismatch
        | RefusalCode::AnotherPart
        | RefusalCode::SingleBout => Evidence::Contradiction,
        RefusalCode::NoLeague
        | RefusalCode::DateUnreadable
        | RefusalCode::PublishedBeforeEvent
        | RefusalCode::NoTeams
        | RefusalCode::NoDate
        | RefusalCode::AmbiguousTeam
        | RefusalCode::NoEvidence
        | RefusalCode::OutsideWindow
        | RefusalCode::TitleTooLong => Evidence::Missing,
    }
}

pub use super::payload::{Known, PayloadCheck, judge_payload, known_for, payload_claims};

// ---------------------------------------------------------------- the payload, judged again

/// What a video of this event and part may plausibly be. The bounds are wide: they exist to turn
/// away a trailer, a sample, a promo reel or a mislabelled multi-day file, not to grade a
/// broadcast. Duration is what ffprobe measured; size is the file's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub min_secs: u32,
    pub max_secs: u32,
    pub min_bytes: u64,
    pub max_bytes: u64,
}

const MIN: u32 = 60;
const HOUR: u32 = 3600;
const GIB: u64 = 1 << 30;

/// The plausible duration and size per sport and part. A game: soccer 45 minutes to 4.5 hours,
/// basketball and rugby 60 minutes to 4.5 hours. A fight card: the whole card 90 minutes to 9
/// hours, the main card 45 minutes to 4.5 hours, prelims 30 minutes to 4 hours, early prelims 15
/// minutes to 3 hours. A golf round 30 minutes to 10 hours; a whole tournament 30 minutes to 48
/// hours. Size, for every sport and part: at least 100 MiB and at most 150 GiB.
pub fn bounds(sport: Sport, part: Part) -> Bounds {
    let (min_secs, max_secs) = match (sport, part) {
        (Sport::Soccer, _) => (45 * MIN, 9 * HOUR / 2),
        (Sport::Basketball | Sport::RugbyUnion | Sport::RugbyLeague, _) => (60 * MIN, 9 * HOUR / 2),
        (Sport::Mma, Part::Whole) => (90 * MIN, 9 * HOUR),
        (Sport::Mma, Part::Main) => (45 * MIN, 9 * HOUR / 2),
        (Sport::Mma, Part::Prelims) => (30 * MIN, 4 * HOUR),
        (Sport::Mma, Part::EarlyPrelims) => (15 * MIN, 3 * HOUR),
        (Sport::Mma, Part::Round(_)) => (30 * MIN, 4 * HOUR),
        (Sport::Golf, Part::Whole) => (30 * MIN, 48 * HOUR),
        (Sport::Golf, Part::Round(_)) => (30 * MIN, 10 * HOUR),
        (Sport::Golf, Part::Main | Part::Prelims | Part::EarlyPrelims) => (30 * MIN, 10 * HOUR),
    };
    Bounds {
        min_secs,
        max_secs,
        min_bytes: 100 << 20,
        max_bytes: 150 * GIB,
    }
}

/// Why a probed file is not plausibly this event and part.
pub fn implausible_reason(
    b: Bounds,
    size: u64,
    duration_secs: f64,
) -> Option<super::reason::Plausibility> {
    use super::reason::Plausibility as P;
    let mins = |s: f64| (s / 60.0).round().max(0.0) as u64;
    if duration_secs < f64::from(b.min_secs) {
        return Some(P::from_numbers(
            0,
            mins(duration_secs),
            u64::from(b.min_secs / 60),
        ));
    }
    if duration_secs > f64::from(b.max_secs) {
        return Some(P::from_numbers(
            1,
            mins(duration_secs),
            u64::from(b.max_secs / 60),
        ));
    }
    if size < b.min_bytes {
        return Some(P::from_numbers(2, size >> 20, 0));
    }
    if size > b.max_bytes {
        return Some(P::from_numbers(3, size >> 30, 0));
    }
    None
}

/// `implausible_reason` as one plain sentence.
pub fn implausible(b: Bounds, size: u64, duration_secs: f64) -> Option<String> {
    implausible_reason(b, size, duration_secs).map(|why| {
        super::reason::Reason::Implausible { why }
            .render_for("event")
            .trim_start_matches("this is not the event: ")
            .to_string()
    })
}

/// File extensions that are run or unpacked, never filed: a payload that holds one is refused.
pub const UNSAFE_EXTS: &[&str] = UNSAFE;
const UNSAFE: &[&str] = &[
    "exe", "bat", "cmd", "com", "scr", "msi", "msp", "ps1", "vbs", "vbe", "js", "jse", "wsf",
    "wsh", "jar", "apk", "sh", "run", "bin", "app", "dmg", "pkg", "deb", "rpm", "lnk", "dll", "so",
    "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst", "cab", "iso", "img", "arj", "lzh",
    "ace",
];

/// Whether `ext` is one of the fixed list of extensions that are never filed.
pub fn known_unsafe_ext(ext: &str) -> bool {
    let volume = ext.len() == 3
        && matches!(ext.as_bytes()[0], b'r' | b'z')
        && ext[1..].bytes().all(|b| b.is_ascii_digit());
    UNSAFE.contains(&ext) || volume
}

/// The first file of a payload that is an executable or an archive (including rar volumes
/// `.r00`, `.part1.rar` and split zips `.z01`).
pub fn unsafe_file<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    names.into_iter().find(|n| {
        let ext = n
            .rsplit('/')
            .next()
            .and_then(|b| b.rsplit_once('.'))
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        let volume = ext.len() == 3
            && matches!(ext.as_bytes()[0], b'r' | b'z')
            && ext[1..].bytes().all(|b| b.is_ascii_digit());
        UNSAFE.contains(&ext.as_str()) || volume
    })
}

// ------------------------------------------------------------------------ naming

/// The longest a file name may be, in bytes (the limit is 255; a margin is left for a name that
/// is later given a suffix by another tool).
const NAME_BYTES: usize = 240;

/// Names a Windows file system will not take, whatever the extension.
const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// One path component, safe on every file system Arr's folder may be shared with: no control
/// characters or separators, no trailing dots or spaces, not a reserved Windows name, not empty.
pub fn component(s: &str, replace_illegal: bool) -> String {
    let cleaned: String = crate::library::clean_name(s, replace_illegal)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    let stem = cleaned
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_lowercase();
    if cleaned.is_empty() {
        "_".into()
    } else if RESERVED.contains(&stem.as_str()) {
        format!("_{cleaned}")
    } else {
        cleaned
    }
}

/// A short, stable disambiguator of an event: the same event always gets the same one.
pub fn event_tag(event_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"unified-media/sport-name/v1\0");
    h.update(event_id.as_bytes());
    hex::encode(&h.finalize()[..3])
}

/// `s` cut to at most `max` bytes at a character boundary.
fn cut(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Where a copy goes below the sport root, built only from the stored event: the folder
/// `<League>/<Season>` and the file `<League> - <date> - <Event>[ - <Part>].<ext>`. Nothing of
/// the torrent's names, so a spoiler or junk name cannot reach Plex.
///
/// The part and the extension are reserved first and the event's own name is shortened (at a
/// character boundary, by bytes) to what is left, with a short tag of the event's id when it had
/// to be, so two parts of one long-named event never collide and no name passes 255 bytes.
/// `taken` is the lower-cased names already in the folder: a name that matches one of them
/// ignoring case (another event's) gets the tag too.
pub fn destination_among(
    ev: &EventRow,
    part: Part,
    ext: &str,
    replace_illegal: bool,
    taken: &[String],
) -> (PathBuf, String) {
    let league = catalog::find(&ev.league).map_or(ev.league.as_str(), |l| l.name);
    let start = OffsetDateTime::parse(
        &ev.starts_at,
        &time::format_description::well_known::Rfc3339,
    )
    .ok();
    let date = start.map_or_else(
        || "0000-00-00".to_string(),
        |t| format!("{:04}-{:02}-{:02}", t.year(), u8::from(t.month()), t.day()),
    );
    let season = ev
        .season
        .clone()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 12
                && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
        .or_else(|| start.map(|t| t.year().to_string()))
        .unwrap_or_else(|| "Unknown".into());
    let league_c = component(league, replace_illegal);
    let head = format!("{league_c} - {date} - ");
    let part_s = if part == Part::Whole {
        String::new()
    } else {
        format!(" - {}", component(&part.label(), replace_illegal))
    };
    let ext = component(ext, true);
    let tail = format!("{part_s}.{ext}");
    let event = component(&ev.name, replace_illegal);
    let tag = format!(" [{}]", event_tag(&ev.id));
    let fit = |event: &str, with_tag: bool| -> String {
        let room = NAME_BYTES
            .saturating_sub(head.len() + tail.len() + if with_tag { tag.len() } else { 0 });
        let shortened = cut(event, room).trim_end_matches(['.', ' ']);
        let mut name = format!("{head}{shortened}");
        if with_tag {
            name.push_str(&tag);
        }
        name.push_str(&tail);
        name
    };
    let mut name = fit(&event, false);
    let mut too_long = head.len() + event.len() + tail.len() > NAME_BYTES;
    if taken.iter().any(|t| *t == name.to_lowercase()) {
        too_long = true;
    }
    if too_long {
        name = fit(&event, true);
    }
    let dir: PathBuf = [component(league, replace_illegal), component(&season, true)]
        .iter()
        .collect();
    (dir, name)
}

/// `destination_among` with nothing else in the folder.
pub fn destination(
    ev: &EventRow,
    part: Part,
    ext: &str,
    replace_illegal: bool,
) -> (PathBuf, String) {
    destination_among(ev, part, ext, replace_illegal, &[])
}

// ---------------------------------------------------------------- names for views

/// Event names for the ids in a view, read from the sport store in one query (the catalogue
/// index knows nothing of events).
pub fn names(c: &Connection, ids: &[MediaId]) -> Result<HashMap<MediaId, String>> {
    let mut out = HashMap::new();
    for id in ids {
        if let MediaId::Event(e) = id
            && let Some(ev) = data::event(c, &e.event_id())?
        {
            out.insert(*id, display_name(&ev, e.part));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------- Plex

/// What the import says about Plex, for the grab's history: where the files are when Plex could
/// not be told.
pub fn plex_note(folder: &str) -> String {
    format!("Saved in {folder}; add it to Plex as an Other Videos library to see it there")
}

/// The key under which the Plex section Arr created or found for sport is remembered. The value
/// is `<machine identifier>|<Plex-side root>|<section key>`: a key means nothing on another
/// server or for another folder.
const SECTION_KEY: &str = "sport_plex_section";

/// The Plex-side sport root, and whether it was stated (`plex.sport_path`) or inferred. Inferred:
/// a folder beside the movies root as Plex sees it, named after the case of the movies folder's
/// own name. An inferred root is trusted only after Plex itself has been asked to show the file
/// just filed there (`established`).
pub fn plex_root(cfg: &crate::config::PlexConfig) -> Option<(String, bool)> {
    if let Some(p) = &cfg.sport_path {
        return Some((p.clone(), true));
    }
    let m = cfg.movies_path.as_deref()?.trim_end_matches('/');
    let (parent, last) = m.rsplit_once('/')?;
    Some((
        format!("{parent}/{}", crate::config::sport_folder_name(last)),
        false,
    ))
}

/// How a Plex library location stands to the Sport folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlexRel {
    /// The same folder.
    Same,
    /// A folder above it (the library holds the Sport folder as part of a larger location).
    Above,
    /// A folder inside it.
    Below,
    /// Equal, above or below only when case is ignored, and Arr cannot tell if case matters.
    Uncertain,
    Apart,
}

/// A Plex-side path as its components: both separators, no empty or trailing ones, the drive
/// letter in lower case; and whether it looks like a Windows path (a drive letter, a backslash or
/// a UNC prefix), in which case the components compare without regard to case.
fn plex_parts(p: &str) -> (Vec<String>, bool) {
    let windows = p.contains('\\')
        || p.starts_with("//")
        || p.as_bytes().get(1) == Some(&b':') && p.as_bytes()[0].is_ascii_alphabetic();
    let mut parts: Vec<String> = p
        .split(['/', '\\'])
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect();
    if let Some(first) = parts.first_mut()
        && first.len() == 2
        && first.ends_with(':')
    {
        *first = first.to_ascii_lowercase();
    }
    (parts, windows)
}

fn plex_rel(location: &str, sport: &str) -> PlexRel {
    // (a blank location is no location; the filesystem root, "/", is the empty list of
    // components and holds everything)
    if location.trim().is_empty() {
        return PlexRel::Apart;
    }
    let (l, lw) = plex_parts(location);
    let (s, sw) = plex_parts(sport);
    let by = |fold: bool| -> PlexRel {
        let norm = |v: &[String]| -> Vec<String> {
            v.iter()
                .map(|c| if fold { c.to_lowercase() } else { c.clone() })
                .collect()
        };
        let (l, s) = (norm(&l), norm(&s));
        if l == s {
            PlexRel::Same
        } else if s.starts_with(&l) {
            PlexRel::Above
        } else if l.starts_with(&s) {
            PlexRel::Below
        } else {
            PlexRel::Apart
        }
    };
    // A Windows path compares without regard to case. Any other path compares exactly, unless
    // the two are the same, above or below each other only when case is ignored: a macOS volume
    // is usually case-insensitive, so Arr cannot tell whether they are one folder, and fails
    // closed (`Uncertain` counts as held, never as a folder Arr may use or create over).
    if lw || sw {
        return by(true);
    }
    match (by(false), by(true)) {
        (PlexRel::Apart, PlexRel::Apart) => PlexRel::Apart,
        (PlexRel::Apart, _) => PlexRel::Uncertain,
        (exact, _) => exact,
    }
}

/// A Plex-side path as it is compared: backslashes as slashes, no doubled or trailing slash.
fn plex_norm(p: &str) -> String {
    let parts: Vec<&str> = p.split(['/', '\\']).filter(|c| !c.is_empty()).collect();
    format!("/{}", parts.join("/"))
}

/// Makes sure Plex has a library whose folder is the sport root, and returns its key.
///
/// Nothing is created unless the Sport-side mapping is established: the root was stated
/// (`plex.sport_path`), or Plex was asked to list the folder the file was just filed in (`GET
/// services/browse/<base64 path>?includeFiles=1`, as its folder picker does) and showed the
/// file by name. A remembered section is reused only for the same Plex server (machine
/// identifier) and root, and only while that section's folder is still the root. A section is
/// created once, by `POST library/sections` with the token header and these query parameters:
/// `name=Sport`, `type=movie`, `agent=com.plexapp.agents.none`, `scanner=Plex Video Files
/// Scanner`, `language=en-US`, `location=<root>`. None of the three Plex requests is verified
/// against a real Plex.
async fn section(
    st: &AppState,
    plex: &crate::plex::Plex,
    root: &str,
    stated: bool,
    filed_dir: &str,
    filed_name: &str,
) -> Result<u32, String> {
    let down =
        |e: crate::upstream::UpstreamError| format!("Plex did not answer ({})", e.describe());
    let root = root.trim_end_matches('/');
    if !stated {
        let folder = format!("{root}/{filed_dir}");
        // (a folder Plex cannot list answers an error: the same as not showing the file)
        let seen = plex.browse_files(&folder).await.unwrap_or_default();
        // Only a FILE entry whose full path is exactly where the filed video would be: not a
        // folder of that name, not the same name somewhere else.
        let expected = plex_norm(&format!("{folder}/{filed_name}"));
        if !seen.iter().any(|p| plex_norm(p) == expected) {
            return Err(
                "Plex could not see the filed video where Arr expected the Sport folder to be; set plex.sport_path to the folder as Plex sees it"
                    .into(),
            );
        }
    }
    let machine = plex
        .machine_id()
        .await
        .map_err(down)?
        .unwrap_or_else(|| "unknown".into());
    let sections = plex.sections().await.map_err(down)?;
    let cfg = plex.config();
    // How a library's folders stand to the Sport folder, by components and with the Plex-side
    // paths normalised (see `plex_rel`). A library holds the Sport folder if one of its locations
    // is that folder or a folder above it.
    let rel_of = |s: &crate::plex::Section| -> Vec<PlexRel> {
        s.locations.iter().map(|l| plex_rel(l, root)).collect()
    };
    let holds = |s: &crate::plex::Section| {
        rel_of(s)
            .iter()
            .any(|r| matches!(r, PlexRel::Same | PlexRel::Above | PlexRel::Uncertain))
    };
    let not_movie_or_tv = |s: &crate::plex::Section| {
        Some(s.key) != cfg.movie_section && Some(s.key) != cfg.tv_section
    };
    // What Arr reuses by itself: a verified plain video library of its own: not the configured
    // movie or TV library, type `movie`, the metadata agent exactly `com.plexapp.agents.none`
    // (what `create_section` makes), and the Sport folder as its only location.
    let plain = |s: &crate::plex::Section| {
        not_movie_or_tv(s)
            && s.kind == "movie"
            && s.agent == "com.plexapp.agents.none"
            && !s.locations.is_empty()
            && rel_of(s).iter().all(|r| *r == PlexRel::Same)
    };
    if let Some(k) = cfg.sport_section {
        // A stated section is the person's deliberate choice: any library that holds the folder
        // and is not the configured movie or TV one. One that does not is a configuration error,
        // said plainly: nothing falls back to another library.
        return match sections.iter().find(|s| s.key == k) {
            Some(s) if not_movie_or_tv(s) && holds(s) => Ok(k),
            _ => Err(format!(
                "plex.sport_section ({k}) is not a library that holds the Sport folder {root}, or it is the movie or TV library; fix it or remove it from config.toml"
            )),
        };
    }
    let remembered: Option<String> = st
        .store
        .with(|c| {
            Ok(c.query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [SECTION_KEY],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
        })
        .await
        .ok()
        .flatten();
    // (the same server, the same root, and the section is still a plain one over the folder)
    if let Some(r) = remembered
        && let Some((m, rest)) = r.split_once('|')
        && let Some((rt, key)) = rest.rsplit_once('|')
        && m == machine
        && rt == root
        && let Ok(key) = key.parse::<u32>()
        && sections.iter().any(|s| s.key == key && plain(s))
    {
        return Ok(key);
    }
    // First ask whether ANY library already has to do with the folder: a verified plain one is
    // used; any other is left untouched, nothing is created, and the note names it.
    if let Some(s) = sections.iter().find(|s| plain(s)) {
        remember(st, &machine, root, s.key).await;
        return Ok(s.key);
    }
    if let Some(s) = sections
        .iter()
        .find(|s| holds(s) || rel_of(s).contains(&PlexRel::Below))
    {
        let how = if holds(s) {
            "holds it"
        } else {
            "has a folder inside it"
        };
        return Err(format!(
            "Plex already has a library ({}) that {how} and is not a plain video library Arr can use by itself; nothing was created or scanned. To use it, set plex.sport_section = {} in config.toml",
            s.title, s.key
        ));
    }
    plex.create_section(
        "Sport",
        "movie",
        "com.plexapp.agents.none",
        "Plex Video Files Scanner",
        "en-US",
        root,
    )
    .await
    .map_err(|e| format!("Plex did not create the Sport library ({})", e.describe()))?;
    let after = plex.sections().await.map_err(down)?;
    let k = after
        .iter()
        .find(|s| plain(s))
        .map(|s| s.key)
        .ok_or("Plex made no Sport library")?;
    remember(st, &machine, root, k).await;
    let at = crate::import::rfc3339(OffsetDateTime::now_utc());
    let _ = st
        .store
        .with(move |c| {
            crate::library::event(
                c,
                &at,
                "sport_library_created",
                None,
                None,
                None,
                "Arr created a Sport library in Plex",
                None,
            )
        })
        .await;
    Ok(k)
}

async fn remember(st: &AppState, machine: &str, root: &str, key: u32) {
    let value = format!("{machine}|{root}|{key}");
    let _ = st
        .store
        .with(move |c| {
            c.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![SECTION_KEY, value],
            )?;
            Ok(())
        })
        .await;
}

/// The `plex_refresh` command for an event: finds or creates the Sport library the first time
/// and scans `<League>/<Season>`. Any failure is a note, never a failed import.
pub(crate) async fn refresh_plex(
    st: &AppState,
    plex: &crate::plex::Plex,
    e: EventRef,
) -> crate::worker::CommandOutcome {
    use crate::worker::CommandOutcome;
    let local = st
        .cfg
        .library
        .as_ref()
        .and_then(crate::ingest::sport_root)
        .map_or_else(|| "the Sport folder".into(), |p| p.display().to_string());
    let filed = {
        let id = e.event_id();
        let part = e.part.slug().unwrap_or_default();
        st.store
            .with(move |c| {
                Ok(super::files::for_event(c, &id)?
                    .into_iter()
                    .find(|f| f.part == part))
            })
            .await
    };
    let Ok(Some(filed)) = filed else {
        return CommandOutcome::Done(serde_json::json!({ "skipped": "nothing is filed for this" }));
    };
    let Some((root, stated)) = plex_root(plex.config()) else {
        return CommandOutcome::Done(serde_json::json!({
            "skipped": "Plex is told where Arr's sport folder is only when plex.sport_path (or movies_path) is set",
            "note": plex_note(&local),
        }));
    };
    let (dir, name) = match filed.path.rsplit_once('/') {
        Some((d, n)) => (d.to_string(), n.to_string()),
        None => (String::new(), filed.path.clone()),
    };
    let key = match section(st, plex, &root, stated, &dir, &name).await {
        Ok(k) => k,
        Err(why) => {
            return CommandOutcome::Done(serde_json::json!({
                "skipped": why,
                "note": plex_note(&local),
            }));
        }
    };
    let path = format!("{}/{dir}", root.trim_end_matches('/'));
    match plex.refresh_import(key, Some(&path)).await {
        Ok(folder) => CommandOutcome::Done(serde_json::json!({
            "section": key, "refreshed": true, "scope": if folder { "folder" } else { "section" }
        })),
        Err(e) => CommandOutcome::Retry(format!("plex {}", e.describe())),
    }
}

// ------------------------------------------------------------------- the text sink

/// A history message as a client may read it. For an event it is rendered from the typed record
/// the row holds and the stored event's own name, whatever else the row holds; other titles pass
/// unchanged. This is the one function every route that returns history for a sport grab uses.
pub fn history_message(c: &Connection, media_id: &str, stored: &str) -> String {
    let Ok(MediaId::Event(e)) = media_id.parse::<MediaId>() else {
        return stored.to_string();
    };
    let display = data::event(c, &e.event_id())
        .ok()
        .flatten()
        .map_or_else(|| "Sport event".to_string(), |ev| display_name(&ev, e.part));
    super::reason::render_history(&display, stored)
}

/// The name a grab of an event is shown under, from the stored event (never from a stored title).
pub fn grab_title(c: &Connection, media_id: &str) -> Option<String> {
    let MediaId::Event(e) = media_id.parse::<MediaId>().ok()? else {
        return None;
    };
    Some(
        data::event(c, &e.event_id())
            .ok()
            .flatten()
            .map_or_else(|| "Sport event".to_string(), |ev| display_name(&ev, e.part)),
    )
}

/// What the home feed needs to show sport downloads: the event names for every event that has a
/// grab, and for each grab that failed or is blocked one plain sentence about what went wrong.
pub struct FeedFacts {
    pub names: HashMap<MediaId, String>,
    pub why: HashMap<String, String>,
}

/// The sentence for a failure of a sport download: the stored reason rendered, or, when the row
/// holds none, the one its failure code stands for.
pub fn plain_failure(code: Option<&str>, stored: Option<&str>, display: &str) -> String {
    match stored.and_then(super::reason::Reason::decode) {
        Some(r) => r.render_for(display),
        None => super::reason::Reason::from_code(code.unwrap_or("")).render_for(display),
    }
}

pub fn feed_facts(c: &Connection) -> Result<FeedFacts> {
    let mut stmt = c.prepare(
        "SELECT id, media_id, state, failure, last_error FROM grabs WHERE media_id LIKE 'event:%' ORDER BY created_at DESC LIMIT 200",
    )?;
    type Row = (String, String, String, Option<String>, Option<String>);
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let ids: Vec<MediaId> = rows.iter().filter_map(|r| r.1.parse().ok()).collect();
    let names = names(c, &ids)?;
    let mut why = HashMap::new();
    for (id, media, state, failure, detail) in &rows {
        if state == "failed" || state == "import_blocked" {
            let display = media
                .parse::<MediaId>()
                .ok()
                .and_then(|m| names.get(&m).cloned())
                .unwrap_or_else(|| super::reason::THIS_EVENT.to_string());
            why.insert(
                id.clone(),
                plain_failure(failure.as_deref(), detail.as_deref(), &display),
            );
        }
    }
    Ok(FeedFacts { names, why })
}
