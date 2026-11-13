//! Why a sport download is held up, failed or blocked, as a typed value, and the lines of its
//! history.
//!
//! Every reason a person can be shown for an event's grab is a value of `Reason`, and every line
//! of its history a `HistoryEvent`. Neither has a free string field: each field is a number with
//! a range (checked when a stored value is read), a bool, or a value of a closed enum (a claim,
//! a part, a kind of file operation, an I/O error kind, ...). The event's own name, its league
//! and its part are never stored in a record: they are filled in when the sentence is rendered,
//! from the stored event the row belongs to. A stored value that does not decode and validate (an
//! unknown variant, an extra field, a wrong type, an out-of-range number, an overlong or
//! truncated value, anything nested) renders as one generic sentence, so what a row holds can
//! never put text into a response, whoever wrote the row.

use super::Part;
use serde::{Deserialize, Serialize};

/// Marks a stored value as an encoded `Reason`.
const MARK: &str = "\u{1}sport:";

/// Marks a stored history message as an encoded `HistoryEvent`.
const EVENT_MARK: &str = "\u{1}sportev:";

/// The longest stored value that is decoded at all.
const MAX_STORED: usize = 512;

/// What the sink shows when nothing better is known.
pub const GENERIC: &str = "Arr hit a problem with this download; its log has the details";

/// What stands for the event's name when a sentence is rendered without the stored event.
pub const THIS_EVENT: &str = "this event";

/// What a name or a title claims that does not fit the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Claim {
    Show,
    NonEvent,
    Episode,
    OneFight,
    TwoParts,
    TwoRounds,
    AnotherRound,
    AnotherDay,
    AnotherCompetition,
    AnotherSport,
    AnotherTournament,
    AnotherCardNumber,
    TwoCardNumbers,
    AnotherTeam,
    OtherFighters,
}

impl Claim {
    fn words(self) -> &'static str {
        match self {
            Claim::Show => "a show about it",
            Claim::NonEvent => "not sport at all",
            Claim::Episode => "an episode of a series",
            Claim::OneFight => "one fight, not the card",
            Claim::TwoParts => "two different parts",
            Claim::TwoRounds => "two different rounds",
            Claim::AnotherRound => "another round",
            Claim::AnotherDay => "another day",
            Claim::AnotherCompetition => "another competition",
            Claim::AnotherSport => "another sport",
            Claim::AnotherTournament => "another tournament",
            Claim::AnotherCardNumber => "another card number",
            Claim::TwoCardNumbers => "two different card numbers",
            Claim::AnotherTeam => "another team",
            Claim::OtherFighters => "other fighters",
        }
    }
}

/// A part of an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartKind {
    Whole,
    Main,
    Prelims,
    EarlyPrelims,
    Round(u8),
}

impl From<Part> for PartKind {
    fn from(p: Part) -> Self {
        match p {
            Part::Whole => PartKind::Whole,
            Part::Main => PartKind::Main,
            Part::Prelims => PartKind::Prelims,
            Part::EarlyPrelims => PartKind::EarlyPrelims,
            Part::Round(n) => PartKind::Round(n),
        }
    }
}

impl PartKind {
    fn valid(self) -> bool {
        !matches!(self, PartKind::Round(n) if !(1..=20).contains(&n))
    }

    /// "the main card", "round 3".
    fn said(self) -> String {
        match self {
            PartKind::Whole => "the full event".into(),
            PartKind::Main => "the main card".into(),
            PartKind::Prelims => "the prelims".into(),
            PartKind::EarlyPrelims => "the early prelims".into(),
            PartKind::Round(n) => format!("round {n}"),
        }
    }
}

/// The kind of an I/O error, from a closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoKind {
    NotFound,
    PermissionDenied,
    Other,
}

impl From<std::io::ErrorKind> for IoKind {
    fn from(k: std::io::ErrorKind) -> Self {
        match k {
            std::io::ErrorKind::NotFound => IoKind::NotFound,
            std::io::ErrorKind::PermissionDenied => IoKind::PermissionDenied,
            _ => IoKind::Other,
        }
    }
}

impl IoKind {
    fn words(self) -> &'static str {
        match self {
            IoKind::NotFound => "not found",
            IoKind::PermissionDenied => "permission denied",
            IoKind::Other => "an I/O error",
        }
    }
}

/// What the import was doing to a file when it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOp {
    StatSource,
    NotRegular,
    SizeDiffers,
    StatDestination,
    EscapedRoot,
    LinkOrCopy,
}

impl FileOp {
    fn words(self) -> &'static str {
        match self {
            FileOp::StatSource => "cannot stat the source",
            FileOp::NotRegular => "is not a regular file",
            FileOp::SizeDiffers => "size on disk differs from the torrent",
            FileOp::StatDestination => "cannot stat the destination",
            FileOp::EscapedRoot => "destination escaped the root",
            FileOp::LinkOrCopy => "cannot link or copy it into the Sport folder",
        }
    }
}

/// Where a filed video turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Landed {
    NotSportFolder,
    InsideDownloads,
}

/// What is wrong with a video's length or size, in numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case", deny_unknown_fields)]
pub enum Plausibility {
    TooShort { mins: u32, min_mins: u32 },
    TooLong { mins: u32, max_mins: u32 },
    TooSmall { mib: u32 },
    TooLarge { gib: u32 },
}

impl Plausibility {
    fn valid(self) -> bool {
        const M: u32 = 10_000_000;
        match self {
            Plausibility::TooShort { mins, min_mins } => mins <= M && min_mins <= M,
            Plausibility::TooLong { mins, max_mins } => mins <= M && max_mins <= M,
            Plausibility::TooSmall { mib } => mib <= M,
            Plausibility::TooLarge { gib } => gib <= M,
        }
    }

    /// Numbers that fit a record: values beyond the range are held at its end.
    pub fn from_numbers(kind: u8, a: u64, b: u64) -> Plausibility {
        let n = |x: u64| u32::try_from(x).unwrap_or(10_000_000).min(10_000_000);
        match kind {
            0 => Plausibility::TooShort {
                mins: n(a),
                min_mins: n(b),
            },
            1 => Plausibility::TooLong {
                mins: n(a),
                max_mins: n(b),
            },
            2 => Plausibility::TooSmall { mib: n(a) },
            _ => Plausibility::TooLarge { gib: n(a) },
        }
    }
}

/// A reason, with everything it says as numbers and closed values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reason {
    // ---- the payload
    AlreadyHave,
    SeveralVideos,
    NoVideo,
    /// An executable or an archive: the index of its extension in `get::UNSAFE_EXTS`, or the
    /// length of that list for an archive volume.
    UnsafeFile {
        ext: u8,
    },
    NamesNotEvent {
        claim: Claim,
    },
    WrongPart {
        said: PartKind,
        expected: PartKind,
    },
    NotPlayable,
    WrongQuality,
    Implausible {
        why: Plausibility,
    },
    Unreadable {
        kind: IoKind,
    },
    CannotUse,
    // ---- the folder and the file operations
    SportFolderApart,
    SportFolderUnusable,
    CannotCreateFolder {
        kind: IoKind,
    },
    DestinationTaken,
    NoSpace {
        free_mib: u64,
        needed_mib: u64,
        min_mib: u64,
    },
    FileOp {
        what: FileOp,
        kind: Option<IoKind>,
    },
    CopyProbeFailed,
    TakenBack {
        landed: Landed,
    },
    PublishFailed {
        kind: IoKind,
    },
    RolledBackAtStartup,
    EventGone,
    LeagueUnknown,
    Internal,
    // ---- the download client and the engine
    ClientSignIn,
    ClientUnreachable,
    ClientRefused,
    ClientNoSpace,
    ClientProblem,
    ForeignTorrent,
    NotStarted,
    // ---- clean-up
    CleanupSeeding,
    CleanupOff {
        completed: bool,
    },
    CleanupPending,
    ImportedKept,
    Reevaluating,
    StillSeeding,
    FilesLeft,
    RemovalSkipped,
    // ---- nothing better is known
    Generic,
}

impl Reason {
    /// A reason for an unsafe file's extension (`exe`, `zip`, ... or an archive volume).
    pub fn unsafe_file(ext: &str) -> Reason {
        let list = super::get::UNSAFE_EXTS;
        let i = list.iter().position(|e| *e == ext).unwrap_or(list.len());
        Reason::UnsafeFile { ext: i as u8 }
    }

    /// Whether every number is in its range.
    fn valid(&self) -> bool {
        const BIG: u64 = 1 << 40;
        match self {
            Reason::UnsafeFile { ext } => usize::from(*ext) <= super::get::UNSAFE_EXTS.len(),
            Reason::WrongPart { said, expected } => said.valid() && expected.valid(),
            Reason::Implausible { why } => why.valid(),
            Reason::NoSpace {
                free_mib,
                needed_mib,
                min_mib,
            } => *free_mib <= BIG && *needed_mib <= BIG && *min_mib <= BIG,
            _ => true,
        }
    }

    /// The stored form.
    pub fn encode(&self) -> String {
        format!("{MARK}{}", serde_json::to_string(self).unwrap_or_default())
    }

    /// The reason a stored value holds, if it holds exactly one: it must be short, a single
    /// well-formed record of a known kind with no extra field, and every number in range.
    pub fn decode(stored: &str) -> Option<Reason> {
        if stored.len() > MAX_STORED {
            return None;
        }
        let r: Reason = serde_json::from_str(stored.strip_prefix(MARK)?).ok()?;
        // Only the canonical form this code writes: an extra field a unit variant would ignore,
        // other spacing, a number written another way, all fail this.
        (r.valid() && r.encode() == stored).then_some(r)
    }

    /// The sentence a person reads, with `display` (the stored event's own name) where it names
    /// the event.
    pub fn render_for(&self, display: &str) -> String {
        use Reason::*;
        match self {
            AlreadyHave => "Arr already has a file for this part of the event; nothing is overwritten".into(),
            SeveralVideos => "more than one video file; this copy needs picking apart by hand".into(),
            NoVideo => "no importable video file in the download".into(),
            UnsafeFile { ext } => {
                let name = super::get::UNSAFE_EXTS.get(usize::from(*ext)).copied().unwrap_or("archive");
                format!("the download holds a .{name} file, which is never filed: nothing from it was added")
            }
            NamesNotEvent { claim } => format!("the download's own names are not this event ({})", claim.words()),
            WrongPart { said, expected } => format!("the download says it is {}, not {}", said.said(), expected.said()),
            NotPlayable => format!("no playable video for {display}"),
            WrongQuality => format!("the video for {display} is not the quality it was listed as"),
            Implausible { why } => format!(
                "this is not the {display}: {}",
                match why {
                    Plausibility::TooShort { mins, min_mins } => format!("the video is {mins} minutes long, shorter than this can be ({min_mins} minutes)"),
                    Plausibility::TooLong { mins, max_mins } => format!("the video is {mins} minutes long, longer than this can be ({max_mins} minutes)"),
                    Plausibility::TooSmall { mib } => format!("the video is only {mib} MiB, too small to be this"),
                    Plausibility::TooLarge { gib } => format!("the video is {gib} GiB, too large"),
                }
            ),
            Unreadable { kind } => format!("cannot read the downloaded video of {display} ({}); give arr read access to the download directory", kind.words()),
            CannotUse => format!("the downloaded video of {display} cannot be used where it is"),
            SportFolderApart => "the Sport folder is inside or around the downloads or another library folder; nothing was filed".into(),
            SportFolderUnusable => "the Sport folder cannot be used; Arr's log says why".into(),
            CannotCreateFolder { kind } => format!("cannot create the Sport folder: {}", kind.words()),
            DestinationTaken => "the destination already exists in the library; nothing is overwritten".into(),
            NoSpace { free_mib, needed_mib, min_mib } => format!(
                "not enough free space in the library ({free_mib} MiB free; {needed_mib} MiB needed plus the {min_mib} MiB minimum)"
            ),
            FileOp { what, kind: None } => format!("the video: {}", what.words()),
            FileOp { what, kind: Some(k) } => format!("the video: {}: {}", what.words(), k.words()),
            CopyProbeFailed => "the copied file failed the media probe".into(),
            TakenBack { landed: Landed::NotSportFolder } => "the filed video is not where the Sport folder is; it was taken back".into(),
            TakenBack { landed: Landed::InsideDownloads } => "the filed video landed inside the downloads or another library; it was taken back".into(),
            PublishFailed { kind } => format!("cannot publish the video: {}", kind.words()),
            RolledBackAtStartup => "interrupted; rolled back at startup".into(),
            EventGone => "the event is no longer in Arr's schedule".into(),
            LeagueUnknown => "the event's league is not one Arr knows".into(),
            Internal => "Arr hit a problem while filing this; its log has the details".into(),
            ClientSignIn => "Arr could not sign in to the download client; check its user name and password in Arr's settings".into(),
            ClientUnreachable => "Arr could not reach the download client; it will try again".into(),
            ClientRefused => "the download client refused this torrent".into(),
            ClientNoSpace => "the download client has no space left in its download folder".into(),
            ClientProblem => "the download client reported a problem; Arr's log has the details".into(),
            ForeignTorrent => "a torrent with this hash is already in the download client under another category; it was left untouched".into(),
            NotStarted => "the download didn't start moving".into(),
            CleanupSeeding => "removeCompletedDownloads: kept seeding until the client stops it at a met share limit".into(),
            CleanupOff { completed: true } => "removeCompletedDownloads is off; the download keeps seeding".into(),
            CleanupOff { completed: false } => "removeFailedDownloads is off; the download is kept".into(),
            CleanupPending => "removeFailedDownloads: removal pending the ownership and seeding checks".into(),
            ImportedKept => "files from this download were imported; its data is kept".into(),
            Reevaluating => "clean-up will be looked at again".into(),
            StillSeeding => "still seeding (the client has not stopped it at a share limit)".into(),
            FilesLeft => "the download's own files are left where they are".into(),
            RemovalSkipped => "a file of the download could not be checked for removal; it is left alone".into(),
            Generic => GENERIC.into(),
        }
    }

    /// The sentence without the stored event (the event is "this event").
    pub fn render(&self) -> String {
        self.render_for(THIS_EVENT)
    }

    /// The reason for a failure code the shared download code raises (the import raises its own
    /// typed ones).
    pub fn from_code(code: &str) -> Reason {
        match code {
            "invalid_torrent" | "submission_failed" => Reason::ClientRefused,
            "foreign_torrent" => Reason::ForeignTorrent,
            "no_metadata" | "stalled" => Reason::NotStarted,
            _ => Reason::Generic,
        }
    }

    /// The reason for an error from the download client or the engine. Its text is never kept.
    pub fn from_upstream(e: &crate::upstream::UpstreamError) -> Reason {
        use crate::upstream::UpstreamError as U;
        match e {
            U::Auth { .. } => Reason::ClientSignIn,
            U::Timeout | U::RateLimited | U::NotFound | U::Throttled { .. } => {
                Reason::ClientUnreachable
            }
            U::Unavailable { reason } if reason.to_ascii_lowercase().contains("space") => {
                Reason::ClientNoSpace
            }
            U::Unavailable { .. } => Reason::ClientUnreachable,
            U::BadResponse { reason } if reason.to_ascii_lowercase().contains("invalid") => {
                Reason::ClientRefused
            }
            U::BadResponse { .. } => Reason::ClientProblem,
        }
    }
}

/// One line of a sport download's history, as a typed record: what happened, with a `Reason`
/// where something went wrong. A history row for an event stores this, never text, and it is
/// rendered from the stored event at read time (`render`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoryEvent {
    Picked,
    Accepted,
    Completed,
    Imported,
    FileImported,
    Failed {
        reason: Reason,
    },
    Blocked {
        reason: Reason,
    },
    Rejected {
        reason: Reason,
    },
    Cancelled,
    Released,
    Removed,
    Retried,
    Recovered,
    PlexRefreshed,
    LibraryCreated,
    /// Something else happened (its text is not kept).
    Other,
}

impl HistoryEvent {
    /// The record for a history line of kind `kind`. Only the kind decides what it is; a reason
    /// is taken from `text` only when `text` is an encoded `Reason` (anything else becomes the
    /// generic reason). Free text is never kept.
    pub fn from_kind(kind: &str, text: &str) -> HistoryEvent {
        let reason = || Reason::decode(&classify(text)).unwrap_or(Reason::Generic);
        match kind {
            "grabbed" => HistoryEvent::Picked,
            "download_started" => HistoryEvent::Accepted,
            "download_completed" => HistoryEvent::Completed,
            "import_completed" => HistoryEvent::Imported,
            "file_imported" | "file_replaced" => HistoryEvent::FileImported,
            "download_failed" => HistoryEvent::Failed { reason: reason() },
            "import_blocked" => HistoryEvent::Blocked { reason: reason() },
            "release_rejected" | "wrong_payload" => HistoryEvent::Rejected { reason: reason() },
            "download_cancelled" => HistoryEvent::Cancelled,
            "download_released" => HistoryEvent::Released,
            "download_removed" => HistoryEvent::Removed,
            "download_retry" => HistoryEvent::Retried,
            "recovered" => HistoryEvent::Recovered,
            "plex_refresh" => HistoryEvent::PlexRefreshed,
            "sport_library_created" => HistoryEvent::LibraryCreated,
            _ => HistoryEvent::Other,
        }
    }

    pub fn encode(&self) -> String {
        format!(
            "{EVENT_MARK}{}",
            serde_json::to_string(self).unwrap_or_default()
        )
    }

    /// The record a stored message holds, under the same rules as `Reason::decode`.
    pub fn decode(stored: &str) -> Option<HistoryEvent> {
        if stored.len() > MAX_STORED {
            return None;
        }
        let h: HistoryEvent = serde_json::from_str(stored.strip_prefix(EVENT_MARK)?).ok()?;
        if h.encode() != stored {
            return None;
        }
        let ok = match h {
            HistoryEvent::Failed { reason }
            | HistoryEvent::Blocked { reason }
            | HistoryEvent::Rejected { reason } => reason.valid(),
            _ => true,
        };
        ok.then_some(h)
    }

    /// The sentence, after the event's own name.
    pub fn render(&self, display: &str) -> String {
        let said = match self {
            HistoryEvent::Picked => "a copy was picked".to_string(),
            HistoryEvent::Accepted => "the download client has it".to_string(),
            HistoryEvent::Completed => "the download finished".to_string(),
            HistoryEvent::Imported => "filed in the Sport folder".to_string(),
            HistoryEvent::FileImported => "the video was put in place".to_string(),
            HistoryEvent::Failed { reason } => {
                format!("the download failed: {}", reason.render_for(display))
            }
            HistoryEvent::Blocked { reason } => format!("waiting: {}", reason.render_for(display)),
            HistoryEvent::Rejected { reason } => {
                format!("not taken: {}", reason.render_for(display))
            }
            HistoryEvent::Cancelled => "the download was cancelled".to_string(),
            HistoryEvent::Released => "Arr let it go".to_string(),
            HistoryEvent::Removed => "the download was removed from the client".to_string(),
            HistoryEvent::Retried => "trying again".to_string(),
            HistoryEvent::Recovered => "picked up again after a restart".to_string(),
            HistoryEvent::PlexRefreshed => "Plex was asked to look".to_string(),
            HistoryEvent::LibraryCreated => "a Sport library was made in Plex".to_string(),
            HistoryEvent::Other => "something happened".to_string(),
        };
        format!("{display}: {said}")
    }
}

/// The sentence for a stored history message of an event: a typed record renders from `display`,
/// anything else is one generic sentence. No prefix or substring of stored text is trusted.
pub fn render_history(display: &str, stored: &str) -> String {
    match HistoryEvent::decode(stored) {
        Some(h) => h.render(display),
        None => format!("{display}: something happened"),
    }
}

/// Turns whatever a writer tried to store for an event grab into the stored form of a reason:
/// an encoded reason is kept (only if it decodes and validates); a fixed sentence the shared
/// download code writes (clean-up) is recognised by what it is about and replaced by its typed
/// reason; anything else becomes `Generic`. The text itself is never kept.
pub fn classify(text: &str) -> String {
    if Reason::decode(text).is_some() {
        return text.to_string();
    }
    let t = text;
    let r = if t.starts_with(MARK) {
        // (looks like a record and is not one)
        Reason::Generic
    } else if t.starts_with("interrupted; rolled back") {
        Reason::RolledBackAtStartup
    } else if t.contains("re-evaluation") {
        Reason::Reevaluating
    } else if t.starts_with("still seeding") {
        Reason::StillSeeding
    } else if t.starts_with("removeCompletedDownloads is off") {
        Reason::CleanupOff { completed: true }
    } else if t.starts_with("removeFailedDownloads is off") {
        Reason::CleanupOff { completed: false }
    } else if t.starts_with("removeCompletedDownloads: kept seeding") {
        Reason::CleanupSeeding
    } else if t.starts_with("removeFailedDownloads: removal pending") {
        Reason::CleanupPending
    } else if t.starts_with("files from this download were imported") {
        Reason::ImportedKept
    } else if t.starts_with("a file of the download could not be checked") {
        Reason::RemovalSkipped
    } else if t.starts_with("the download's own files are left") {
        Reason::FilesLeft
    } else {
        tracing::warn!("text withheld from clients for a sport download: {t}");
        Reason::Generic
    };
    r.encode()
}

/// The sentence for a stored value of an event grab, with the event's own name where the sentence
/// names it: an encoded reason renders, anything else (a row written some other way) is the
/// generic sentence.
pub fn render_stored_for(stored: &str, display: &str) -> String {
    Reason::decode(stored).map_or_else(|| GENERIC.to_string(), |r| r.render_for(display))
}

/// `render_stored_for` without the stored event.
pub fn render_stored(stored: &str) -> String {
    render_stored_for(stored, THIS_EVENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One of every variant. The `match` forces this list to be updated with the type: a new
    /// variant does not compile until it is handled here.
    fn all() -> Vec<Reason> {
        use Plausibility::*;
        let list = vec![
            Reason::AlreadyHave,
            Reason::SeveralVideos,
            Reason::NoVideo,
            Reason::UnsafeFile { ext: 0 },
            Reason::NamesNotEvent {
                claim: Claim::AnotherDay,
            },
            Reason::WrongPart {
                said: PartKind::Prelims,
                expected: PartKind::Main,
            },
            Reason::WrongPart {
                said: PartKind::Round(2),
                expected: PartKind::Round(3),
            },
            Reason::NotPlayable,
            Reason::WrongQuality,
            Reason::Implausible {
                why: TooShort {
                    mins: 3,
                    min_mins: 45,
                },
            },
            Reason::Implausible {
                why: TooLong {
                    mins: 900,
                    max_mins: 270,
                },
            },
            Reason::Implausible {
                why: TooSmall { mib: 12 },
            },
            Reason::Implausible {
                why: TooLarge { gib: 200 },
            },
            Reason::Unreadable {
                kind: IoKind::PermissionDenied,
            },
            Reason::CannotUse,
            Reason::SportFolderApart,
            Reason::SportFolderUnusable,
            Reason::CannotCreateFolder {
                kind: IoKind::NotFound,
            },
            Reason::DestinationTaken,
            Reason::NoSpace {
                free_mib: 1,
                needed_mib: 2,
                min_mib: 3,
            },
            Reason::FileOp {
                what: FileOp::StatSource,
                kind: Some(IoKind::NotFound),
            },
            Reason::FileOp {
                what: FileOp::NotRegular,
                kind: None,
            },
            Reason::CopyProbeFailed,
            Reason::TakenBack {
                landed: Landed::InsideDownloads,
            },
            Reason::TakenBack {
                landed: Landed::NotSportFolder,
            },
            Reason::PublishFailed {
                kind: IoKind::Other,
            },
            Reason::RolledBackAtStartup,
            Reason::EventGone,
            Reason::LeagueUnknown,
            Reason::Internal,
            Reason::ClientSignIn,
            Reason::ClientUnreachable,
            Reason::ClientRefused,
            Reason::ClientNoSpace,
            Reason::ClientProblem,
            Reason::ForeignTorrent,
            Reason::NotStarted,
            Reason::CleanupSeeding,
            Reason::CleanupOff { completed: true },
            Reason::CleanupOff { completed: false },
            Reason::CleanupPending,
            Reason::ImportedKept,
            Reason::Reevaluating,
            Reason::StillSeeding,
            Reason::FilesLeft,
            Reason::RemovalSkipped,
            Reason::Generic,
        ];
        for r in &list {
            // (exhaustive: no wildcard arm)
            match r {
                Reason::AlreadyHave
                | Reason::SeveralVideos
                | Reason::NoVideo
                | Reason::UnsafeFile { .. }
                | Reason::NamesNotEvent { .. }
                | Reason::WrongPart { .. }
                | Reason::NotPlayable
                | Reason::WrongQuality
                | Reason::Implausible { .. }
                | Reason::Unreadable { .. }
                | Reason::CannotUse
                | Reason::SportFolderApart
                | Reason::SportFolderUnusable
                | Reason::CannotCreateFolder { .. }
                | Reason::DestinationTaken
                | Reason::NoSpace { .. }
                | Reason::FileOp { .. }
                | Reason::CopyProbeFailed
                | Reason::TakenBack { .. }
                | Reason::PublishFailed { .. }
                | Reason::RolledBackAtStartup
                | Reason::EventGone
                | Reason::LeagueUnknown
                | Reason::Internal
                | Reason::ClientSignIn
                | Reason::ClientUnreachable
                | Reason::ClientRefused
                | Reason::ClientNoSpace
                | Reason::ClientProblem
                | Reason::ForeignTorrent
                | Reason::NotStarted
                | Reason::CleanupSeeding
                | Reason::CleanupOff { .. }
                | Reason::CleanupPending
                | Reason::ImportedKept
                | Reason::Reevaluating
                | Reason::StillSeeding
                | Reason::FilesLeft
                | Reason::RemovalSkipped
                | Reason::Generic => {}
            }
        }
        list
    }

    fn history() -> Vec<HistoryEvent> {
        let r = Reason::NamesNotEvent { claim: Claim::Show };
        let list = vec![
            HistoryEvent::Picked,
            HistoryEvent::Accepted,
            HistoryEvent::Completed,
            HistoryEvent::Imported,
            HistoryEvent::FileImported,
            HistoryEvent::Failed { reason: r },
            HistoryEvent::Blocked { reason: r },
            HistoryEvent::Rejected { reason: r },
            HistoryEvent::Cancelled,
            HistoryEvent::Released,
            HistoryEvent::Removed,
            HistoryEvent::Retried,
            HistoryEvent::Recovered,
            HistoryEvent::PlexRefreshed,
            HistoryEvent::LibraryCreated,
            HistoryEvent::Other,
        ];
        for h in &list {
            match h {
                HistoryEvent::Picked
                | HistoryEvent::Accepted
                | HistoryEvent::Completed
                | HistoryEvent::Imported
                | HistoryEvent::FileImported
                | HistoryEvent::Failed { .. }
                | HistoryEvent::Blocked { .. }
                | HistoryEvent::Rejected { .. }
                | HistoryEvent::Cancelled
                | HistoryEvent::Released
                | HistoryEvent::Removed
                | HistoryEvent::Retried
                | HistoryEvent::Recovered
                | HistoryEvent::PlexRefreshed
                | HistoryEvent::LibraryCreated
                | HistoryEvent::Other => {}
            }
        }
        list
    }

    const HOSTILE: &str = "Gane-wins-by-KO";

    /// Every string leaf of a JSON value, with the path to it.
    fn strings(v: &serde_json::Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        match v {
            serde_json::Value::String(_) => out.push(path.clone()),
            serde_json::Value::Object(m) => {
                for (k, x) in m {
                    path.push(k.clone());
                    strings(x, path, out);
                    path.pop();
                }
            }
            serde_json::Value::Array(a) => {
                for (i, x) in a.iter().enumerate() {
                    path.push(i.to_string());
                    strings(x, path, out);
                    path.pop();
                }
            }
            _ => {}
        }
    }

    fn set(v: &mut serde_json::Value, path: &[String], to: serde_json::Value) {
        let mut cur = v;
        for p in &path[..path.len() - 1] {
            cur = match cur {
                serde_json::Value::Object(m) => m.get_mut(p).unwrap(),
                serde_json::Value::Array(a) => a.get_mut(p.parse::<usize>().unwrap()).unwrap(),
                _ => unreachable!(),
            };
        }
        if let serde_json::Value::Object(m) = cur {
            m.insert(path[path.len() - 1].clone(), to);
        }
    }

    #[test]
    fn every_reason_renders_and_round_trips() {
        for r in all() {
            let s = r.render();
            assert!(!s.is_empty() && !s.contains('\u{1}'), "{r:?}");
            assert_eq!(Reason::decode(&r.encode()), Some(r), "{r:?}");
            assert_eq!(render_stored(&r.encode()), s);
            assert!(
                r.render_for("UFC 334: Gane vs. Hokit - Main card").len()
                    >= s.len() - THIS_EVENT.len()
            );
        }
        for h in history() {
            assert_eq!(HistoryEvent::decode(&h.encode()), Some(h), "{h:?}");
            assert!(h.render("X").starts_with("X: "));
        }
    }

    /// No variant has a free string field: wherever a string appears in a record, replacing it by
    /// hostile text makes the whole record fail to decode (a closed enum accepts only its own
    /// names). A `String` field anywhere would accept it and fail this test.
    #[test]
    fn no_variant_has_a_free_string_field() {
        let mut leaves = 0;
        for r in all() {
            let base: serde_json::Value =
                serde_json::from_str(r.encode().strip_prefix(MARK).unwrap()).unwrap();
            let mut paths = vec![];
            strings(&base, &mut vec![], &mut paths);
            for p in paths {
                // (the tag naming the variant is a string: it must name a variant, nothing else)
                leaves += 1;
                let mut v = base.clone();
                set(&mut v, &p, serde_json::Value::String(HOSTILE.into()));
                let stored = format!("{MARK}{v}");
                assert!(
                    Reason::decode(&stored).is_none(),
                    "{r:?} at {p:?} took hostile text"
                );
            }
        }
        for h in history() {
            let base: serde_json::Value =
                serde_json::from_str(h.encode().strip_prefix(EVENT_MARK).unwrap()).unwrap();
            let mut paths = vec![];
            strings(&base, &mut vec![], &mut paths);
            for p in paths {
                leaves += 1;
                let mut v = base.clone();
                set(&mut v, &p, serde_json::Value::String(HOSTILE.into()));
                let stored = format!("{EVENT_MARK}{v}");
                assert!(
                    HistoryEvent::decode(&stored).is_none(),
                    "{h:?} at {p:?} took hostile text"
                );
            }
        }
        assert!(leaves > 50, "{leaves}");
    }

    #[test]
    fn a_crafted_value_never_renders_foreign_text() {
        let hostile = |s: &str| {
            let r = render_stored(s);
            assert!(!r.contains(HOSTILE), "{s}: {r}");
            assert_eq!(r, GENERIC, "{s}");
            let h = render_history("EVENT", s);
            assert!(!h.contains(HOSTILE), "{s}: {h}");
        };
        for stored in [
            // a free field that used to exist
            format!("{MARK}{{\"k\":\"names_not_event\",\"what\":\"{HOSTILE}\"}}"),
            format!("{MARK}{{\"k\":\"names_not_event\",\"claim\":\"{HOSTILE}\"}}"),
            format!("{MARK}{{\"k\":\"not_playable\",\"name\":\"{HOSTILE}\"}}"),
            format!("{MARK}{{\"k\":\"wrong_part\",\"said\":\"{HOSTILE}\",\"expected\":\"main\"}}"),
            format!("{MARK}{{\"k\":\"unsafe_file\",\"ext\":\"{HOSTILE}\"}}"),
            // an unknown variant, extra fields, a wrong type
            format!("{MARK}{{\"k\":\"{HOSTILE}\"}}"),
            format!("{MARK}{{\"k\":\"already_have\",\"note\":\"{HOSTILE}\"}}"),
            format!("{MARK}{{\"k\":\"unsafe_file\",\"ext\":true}}"),
            // a huge or negative or fractional number
            format!("{MARK}{{\"k\":\"unsafe_file\",\"ext\":255}}"),
            format!("{MARK}{{\"k\":\"unsafe_file\",\"ext\":99999999999999999999}}"),
            format!(
                "{MARK}{{\"k\":\"no_space\",\"free_mib\":99999999999999,\"needed_mib\":1,\"min_mib\":1}}"
            ),
            format!(
                "{MARK}{{\"k\":\"implausible\",\"why\":{{\"k\":\"too_small\",\"mib\":4294967295}}}}"
            ),
            format!(
                "{MARK}{{\"k\":\"wrong_part\",\"said\":{{\"round\":200}},\"expected\":\"main\"}}"
            ),
            format!("{MARK}{{\"k\":\"unsafe_file\",\"ext\":-1}}"),
            // truncated, empty, repeated or nested prefixes
            format!("{MARK}{{\"k\":\"already_have\""),
            MARK.to_string(),
            format!("{MARK}{MARK}{{\"k\":\"already_have\"}}"),
            format!("{MARK}{{\"k\":\"names_not_event\",\"claim\":\"{MARK}\"}}"),
            format!("{MARK}{{\"k\":\"already_have\"}}{HOSTILE}"),
            format!("{MARK}{{\"k\":\"already_have\"}} {}", "x".repeat(600)),
            format!("{}{MARK}{{\"k\":\"already_have\"}}", HOSTILE),
            HOSTILE.to_string(),
        ] {
            hostile(&stored);
            assert_eq!(render_stored(&classify(&stored)), GENERIC, "{stored}");
        }
        // The same for history records.
        for stored in [
            format!(
                "{EVENT_MARK}{{\"k\":\"failed\",\"reason\":{{\"k\":\"names_not_event\",\"what\":\"{HOSTILE}\"}}}}"
            ),
            format!("{EVENT_MARK}{{\"k\":\"failed\",\"reason\":\"{HOSTILE}\"}}"),
            format!("{EVENT_MARK}{{\"k\":\"{HOSTILE}\"}}"),
            format!("{EVENT_MARK}{{\"k\":\"picked\",\"note\":\"{HOSTILE}\"}}"),
            format!(
                "{EVENT_MARK}{{\"k\":\"failed\",\"reason\":{{\"k\":\"unsafe_file\",\"ext\":250}}}}"
            ),
            format!("{EVENT_MARK}{EVENT_MARK}{{\"k\":\"picked\"}}"),
            format!("{EVENT_MARK}{{\"k\":\"picked\""),
            format!("{MARK}{{\"k\":\"already_have\"}}"),
        ] {
            let r = render_history("EVENT", &stored);
            assert!(!r.contains(HOSTILE), "{stored}: {r}");
            assert_eq!(r, "EVENT: something happened", "{stored}");
        }
    }

    #[test]
    fn foreign_text_has_no_way_in() {
        for t in [
            "Gane-wins.mkv: corrupt",
            "unsafe torrent name \"../x\"",
            "/srv/dl/x.mkv",
            "\u{1}sport:{\"k\":\"nonsense\"}",
        ] {
            assert_eq!(render_stored(&classify(t)), GENERIC, "{t}");
            assert_eq!(render_stored(t), GENERIC, "{t}");
        }
    }

    #[test]
    fn the_shared_cleanup_sentences_keep_their_meaning() {
        for (text, want) in [
            (
                "removeCompletedDownloads: kept seeding until the client stops it at a met share limit",
                Reason::CleanupSeeding,
            ),
            (
                "removeCompletedDownloads is off (or [execution] download_cleanup = false); the download keeps seeding",
                Reason::CleanupOff { completed: true },
            ),
            (
                "removeFailedDownloads is off (or [execution] download_cleanup = false); the download is kept",
                Reason::CleanupOff { completed: false },
            ),
            (
                "removeFailedDownloads: removal pending the ownership and seeding checks",
                Reason::CleanupPending,
            ),
            (
                "files from this download were imported; its data is kept",
                Reason::ImportedKept,
            ),
            ("cleanup re-evaluation requested", Reason::Reevaluating),
            (
                "still seeding (the client has not stopped it at a share limit)",
                Reason::StillSeeding,
            ),
        ] {
            assert_eq!(Reason::decode(&classify(text)), Some(want), "{text}");
        }
    }

    #[test]
    fn client_errors_become_typed_reasons() {
        use crate::upstream::UpstreamError as U;
        let case = |e: U| Reason::from_upstream(&e);
        assert_eq!(case(U::Auth { status: 403 }), Reason::ClientSignIn);
        assert_eq!(case(U::Timeout), Reason::ClientUnreachable);
        assert_eq!(
            case(U::Unavailable {
                reason: "HTTP 503".into()
            }),
            Reason::ClientUnreachable
        );
        assert_eq!(
            case(U::Unavailable {
                reason: "no space left on device".into()
            }),
            Reason::ClientNoSpace
        );
        assert_eq!(
            case(U::BadResponse {
                reason: "invalid torrent \"../Gane\"".into()
            }),
            Reason::ClientRefused
        );
        assert_eq!(
            case(U::BadResponse {
                reason: "garbled".into()
            }),
            Reason::ClientProblem
        );
    }
}
