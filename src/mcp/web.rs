//! The tools for everything the web app can do beyond the original eight: sport, people,
//! download options, the title pages, the feed, and deleting files with its bin. Each tool is
//! one REST call (two for `requests`-style summaries in the parent module, none here), through
//! the same routes as the web app, so it gets the same checks and the same refusals. Results are
//! what the API returns: no field is added, hidden or reworded, so nothing the API keeps
//! back (scores and results for sport, release text, links, hashes) can come out of a tool.
//!
//! Adding a route to the API means adding its tool, or its reason, to the parity table in the
//! parent module (`AGENTS.md` explains).

use super::{
    ACQUIRABLE_ID, ANY_ID, Adapter, Effect, MAX_QUERY_CHARS, invalid, parse, respond, tool,
};
use crate::ids::MediaId;
use rmcp::model::{CallToolResult, Tool, ToolAnnotations};
use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;
use serde_json::{Value, json};

const SERIES_ID: &str = r"^series:(tvdb|tvmaze):[1-9][0-9]{0,9}$";
const MOVIE_OR_SERIES_ID: &str =
    r"^(movie:tmdb:[1-9][0-9]{0,9}|series:(tvdb|tvmaze):[1-9][0-9]{0,9})$";
const TITLE_PAGE_ID: &str = r"^(movie:tmdb:[1-9][0-9]{0,9}|series:(tvdb|tvmaze):[1-9][0-9]{0,9}|season:(tvdb|tvmaze):[1-9][0-9]{0,9}:[1-9][0-9]{0,2})$";
const LEAGUE: &str = r"^(league:)?[a-z0-9-]{2,24}$";
const EVENT_ID: &str = r"^(event:)?[a-z0-9-]{2,24}:[1-9][0-9]{0,17}(:[a-z0-9-]{1,20})?$";
const COPY_REF: &str = r"^rel_[0-9a-f]{24}$";
const GRAB_ID: &str = r"^grb_[0-9a-f]{24}$";
const DELETE_ID: &str = r"^[0-9a-f]{16}$";
const PERSON_ID: &str = r"^(plex|tvmaze):[A-Za-z0-9_.-]{1,64}$";
const TOKEN: &str = r"^[0-9a-f]{64}$";
const TIME: &str = r"^[0-9]{4}-[0-9]{2}-[0-9]{2}([T ][0-9:.+Zz-]{1,40})?$";

/// What the person must see and agree to before a copy with a caution is picked.
pub(super) const CONFIRM_ROUTE: &str = "This copy carries a caution. Show the person the caution note exactly as given, tell them what it means, and ask whether they still want this copy. Call sport_get again with confirm true only after they say yes.";

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
enum Level {
    Smaller,
    Standard,
    Best,
}

impl Level {
    fn as_str(&self) -> &'static str {
        match self {
            Level::Smaller => "smaller",
            Level::Standard => "standard",
            Level::Best => "best",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TextQuery {
    /// What to look for (1 to 100 printable characters).
    #[schemars(length(min = 1, max = 100))]
    q: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SportLeague {
    /// A league's slug from sport_search or sport_leagues, such as nba or league:nba.
    #[schemars(regex(pattern = LEAGUE))]
    league: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SportFollowArg {
    /// A league's slug from sport_search or sport_leagues.
    #[schemars(regex(pattern = LEAGUE))]
    league: String,
    /// A team's number in that league (from sport_league or sport_search). Leave out for the whole league.
    #[serde(default)]
    #[schemars(regex(pattern = r"^[0-9]{1,18}$"))]
    team_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SportEvents {
    /// First day (YYYY-MM-DD or an RFC 3339 time). Default: now.
    #[serde(default)]
    #[schemars(regex(pattern = TIME))]
    from: Option<String>,
    /// Last day, at most 92 days after from. Default: two weeks after from.
    #[serde(default)]
    #[schemars(regex(pattern = TIME))]
    to: Option<String>,
    /// Only this league.
    #[serde(default)]
    #[schemars(regex(pattern = LEAGUE))]
    league: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[schemars(inline)]
enum Part {
    Main,
    Prelims,
    EarlyPrelims,
    R1,
    R2,
    R3,
    R4,
}

impl Part {
    fn as_str(&self) -> &'static str {
        match self {
            Part::Main => "main",
            Part::Prelims => "prelims",
            Part::EarlyPrelims => "early-prelims",
            Part::R1 => "r1",
            Part::R2 => "r2",
            Part::R3 => "r3",
            Part::R4 => "r4",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SportCopies {
    /// An event's id from sport_events or sport_league, such as event:ufc:600061400.
    #[schemars(regex(pattern = EVENT_ID))]
    event_id: String,
    /// Only one part of the event (a card's main, prelims or early-prelims; a golf round r1 to r4).
    #[serde(default)]
    part: Option<Part>,
    /// Ask the indexers again instead of using a recent answer.
    #[serde(default)]
    fresh: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SportGet {
    /// The event the copy was listed for.
    #[schemars(regex(pattern = EVENT_ID))]
    event_id: String,
    /// The id of the one copy the person chose from the last sport_copies answer for this event.
    #[schemars(regex(pattern = COPY_REF))]
    id: String,
    /// True only after the person has seen the copy's caution and said they still want it.
    #[serde(default)]
    confirm: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PersonArg {
    /// A person's id from people_search, such as plex:1234 or tvmaze:5678.
    #[schemars(regex(pattern = PERSON_ID))]
    person_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetTitle {
    /// Canonical id of a movie, a show, a season or an episode.
    #[schemars(regex(pattern = ANY_ID))]
    media_id: String,
    /// The quality the person picked: smaller (720p), standard (1080p) or best (4K).
    #[serde(default)]
    level: Option<Level>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetQuality {
    /// A movie or a whole show Arr already knows.
    #[schemars(regex(pattern = MOVIE_OR_SERIES_ID))]
    media_id: String,
    /// smaller (720p), standard (1080p) or best (4K).
    level: Level,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct FollowSeries {
    /// A show: series:<tvdb|tvmaze>:<id>.
    #[schemars(regex(pattern = SERIES_ID))]
    media_id: String,
    /// True to get its new episodes as they air, false to stop.
    on: bool,
    /// The quality the person picked, when turning it on.
    #[serde(default)]
    level: Option<Level>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveTitle {
    /// A movie or a whole show.
    #[schemars(regex(pattern = MOVIE_OR_SERIES_ID))]
    media_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Copies {
    /// A movie, season or episode (canonical id).
    #[schemars(regex(pattern = ACQUIRABLE_ID))]
    media_id: String,
    /// For a title Arr does not have yet: the quality the person picked.
    #[serde(default)]
    level: Option<Level>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetCopy {
    /// The id the copy was listed for by copies.
    #[schemars(regex(pattern = ACQUIRABLE_ID))]
    media_id: String,
    /// The id of the one copy the person chose from the copies answer.
    #[schemars(regex(pattern = COPY_REF))]
    id: String,
    /// The quality the person picked, for a title Arr does not have yet.
    #[serde(default)]
    level: Option<Level>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RetryDownload {
    /// A download's id from requests or the feed (grb_ and 24 hex).
    #[schemars(regex(pattern = GRAB_ID))]
    grab_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HideIdea {
    /// A show from ideas.
    #[schemars(regex(pattern = SERIES_ID))]
    media_id: String,
    /// True (the default) hides it from Ideas for everyone using this Arr; false shows it again.
    #[serde(default)]
    hidden: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Calendar {
    /// First moment (RFC 3339). Default: a day ago.
    #[serde(default)]
    #[schemars(length(min = 20, max = 40))]
    start: Option<String>,
    /// Last moment (RFC 3339), at most 92 days after start. Default: two weeks ahead.
    #[serde(default)]
    #[schemars(length(min = 20, max = 40))]
    end: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Wanted {
    /// Only what is wanted for this canonical id.
    #[serde(default)]
    #[schemars(regex(pattern = ANY_ID))]
    media_id: Option<String>,
    /// Items per page (default 50).
    #[serde(default)]
    #[schemars(range(min = 1, max = 100))]
    limit: Option<u32>,
    /// Where the page starts.
    #[serde(default)]
    #[schemars(range(min = 0, max = 1_000_000))]
    offset: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct History {
    /// Only this canonical id.
    #[serde(default)]
    #[schemars(regex(pattern = ANY_ID))]
    media_id: Option<String>,
    /// Only one kind of event, such as grabbed or file_imported.
    #[serde(default)]
    #[schemars(regex(pattern = r"^[a-z_]{1,40}$"))]
    kind: Option<String>,
    /// Items per page (default 50).
    #[serde(default)]
    #[schemars(range(min = 1, max = 100))]
    limit: Option<u32>,
    /// Where the page starts.
    #[serde(default)]
    #[schemars(range(min = 0, max = 1_000_000))]
    offset: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TitleDetails {
    /// A movie, a show or one season of a show (canonical id).
    #[schemars(regex(pattern = TITLE_PAGE_ID))]
    media_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeletePreview {
    /// The movie, show, season or episode whose files might be deleted.
    #[schemars(regex(pattern = ANY_ID))]
    media_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeleteFiles {
    /// The same media_id the preview was made for.
    #[schemars(regex(pattern = ANY_ID))]
    media_id: String,
    /// The token the preview of exactly this media_id returned, unchanged.
    #[schemars(regex(pattern = TOKEN))]
    token: String,
    /// True to keep wanting the title, so Arr gets a new copy; the default stops wanting it.
    #[serde(default)]
    keep_wanted: Option<bool>,
    /// Must be true, and only after the person has seen this exact preview and said yes to deleting it.
    confirmed: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BinRestore {
    /// An entry's delete_id from bin or from a delete's answer.
    #[schemars(regex(pattern = DELETE_ID))]
    delete_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BinEmpty {
    /// The entry's delete_id from bin.
    #[schemars(regex(pattern = DELETE_ID))]
    delete_id: String,
    /// The entry's bytes exactly as bin shows them; the entry is only emptied while it still has that size.
    #[schemars(range(min = 0))]
    bytes: u64,
    /// Must be true, and only after the person has seen this entry and said yes to deleting it for good.
    confirmed: bool,
}

/// (tool, argument, pattern): arguments that become part of a request path.
const SHAPES: &[(&str, &str, &str)] = &[
    ("sport_league", "league", LEAGUE),
    ("sport_follow", "league", LEAGUE),
    ("sport_follow", "team_id", r"^[0-9]{1,18}$"),
    ("sport_unfollow", "league", LEAGUE),
    ("sport_unfollow", "team_id", r"^[0-9]{1,18}$"),
    ("sport_events", "league", LEAGUE),
    ("sport_events", "from", TIME),
    ("sport_events", "to", TIME),
    ("sport_copies", "event_id", EVENT_ID),
    ("sport_get", "event_id", EVENT_ID),
    ("sport_get", "id", COPY_REF),
    ("get_copy", "id", COPY_REF),
    ("person", "person_id", PERSON_ID),
    ("retry_download", "grab_id", GRAB_ID),
    ("bin_restore", "delete_id", DELETE_ID),
    ("bin_empty", "delete_id", DELETE_ID),
    ("delete_files", "token", TOKEN),
    ("download", "grab_id", GRAB_ID),
    ("pause_download", "grab_id", GRAB_ID),
    ("resume_download", "grab_id", GRAB_ID),
    ("cancel_download", "grab_id", GRAB_ID),
    ("command_status", "id", r"^cmd_[0-9a-f]{24}$"),
];

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DownloadArg {
    /// A download's id from requests or the feed (grb_ and 24 hex).
    #[schemars(regex(pattern = GRAB_ID))]
    grab_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BlocklistArg {
    /// Only this canonical id.
    #[serde(default)]
    #[schemars(regex(pattern = ANY_ID))]
    media_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Unblock {
    /// An entry's id from blocklist.
    #[schemars(range(min = 1, max = 9_007_199_254_740_991u64))]
    id: u64,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
enum CommandName {
    Rescan,
    SearchMissing,
    RssSync,
    RefreshMetadata,
    Search,
    PlexRefresh,
}

impl CommandName {
    fn as_str(&self) -> &'static str {
        match self {
            CommandName::Rescan => "rescan",
            CommandName::SearchMissing => "search_missing",
            CommandName::RssSync => "rss_sync",
            CommandName::RefreshMetadata => "refresh_metadata",
            CommandName::Search => "search",
            CommandName::PlexRefresh => "plex_refresh",
        }
    }

    fn needs_title(&self) -> bool {
        matches!(self, CommandName::Search | CommandName::PlexRefresh)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RunCommand {
    /// rescan (look through the library folders again), search_missing (look for everything missing), rss_sync (check the indexers' newest releases now), refresh_metadata (refresh titles' details), search (look for one title; needs media_id), plex_refresh (ask Plex to look again at one title; needs media_id).
    name: CommandName,
    /// The title for search and plex_refresh (a canonical id); not for the others.
    #[serde(default)]
    #[schemars(regex(pattern = ANY_ID))]
    media_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CommandStatus {
    /// A command's id from run_command (cmd_ and 24 hex). Leave out to list the latest commands.
    #[serde(default)]
    #[schemars(regex(pattern = r"^cmd_[0-9a-f]{24}$"))]
    id: Option<String>,
    /// Commands per page when listing (default 50).
    #[serde(default)]
    #[schemars(range(min = 1, max = 100))]
    limit: Option<u32>,
    /// Where the page starts when listing.
    #[serde(default)]
    #[schemars(range(min = 0, max = 1_000_000))]
    offset: Option<u32>,
}

pub(super) fn tool_list(read: &ToolAnnotations) -> Vec<Tool> {
    let write = ToolAnnotations::new()
        .read_only(false)
        .destructive(false)
        .idempotent(false)
        .open_world(true);
    let write_idem = write.clone().idempotent(true);
    let destructive = ToolAnnotations::new()
        .read_only(false)
        .destructive(true)
        .idempotent(false)
        .open_world(true);
    vec![
        tool::<TextQuery>(
            "sport_search",
            "Find sport leagues (and teams of leagues Arr has read) by name, for example \"premier league\" or \"lakers\". Results say whether each is followed. Sport is hand-picked only: nothing here downloads anything.",
            read.clone(),
        ),
        tool::<NoArgs>(
            "sport_leagues",
            "Every sport league Arr knows, with whether each is followed.",
            read.clone(),
        ),
        tool::<SportLeague>(
            "sport_league",
            "One league's page: its teams, upcoming events, recent events and whether it is followed. Results and scores are never shown.",
            read.clone(),
        ),
        tool::<SportFollowArg>(
            "sport_follow",
            "Follow a league, or one team in it, so its events appear in the schedule. Following starts now and downloads nothing; the person picks every copy by hand with sport_copies and sport_get.",
            write_idem.clone(),
        ),
        tool::<SportFollowArg>(
            "sport_unfollow",
            "Stop following a league or a team. What Arr already stored stays. Following again later starts from then.",
            write_idem.clone(),
        ),
        tool::<NoArgs>(
            "sport_following",
            "Everything followed, each with its next event.",
            read.clone(),
        ),
        tool::<SportEvents>(
            "sport_events",
            "The schedule: events of followed leagues and teams that are not over by `from` and start before `to`. Results and scores are never shown.",
            read.clone(),
        ),
        tool::<SportCopies>(
            "sport_copies",
            "The copies Arr found for one sport event after it ended, as the API lists them (part, resolution, source, size, language, speed, and a caution when Arr is not sure it is the right event). Nothing is recommended or picked for the person: show them the copies and let them choose. A copy with a caution must be shown with its note. This only looks; it downloads nothing.",
            read.clone(),
        ),
        tool::<SportGet>(
            "sport_get",
            "Get the ONE copy the person chose from sport_copies for this event. Never choose a copy for them. If the answer is confirmation_needed, the copy has a caution: show the person the caution note exactly as given and ask whether they still want it; call again with confirm true only after they say yes, never on your own. Repeating a pick does not create a duplicate (already_queued, already_have).",
            write.clone(),
        ),
        tool::<TextQuery>(
            "people_search",
            "Find an actor, director or other person by name. Results carry a person_id for the person tool.",
            read.clone(),
        ),
        tool::<PersonArg>(
            "person",
            "A person and what they made, in groups by job, each title with whether it is already here.",
            read.clone(),
        ),
        tool::<GetTitle>(
            "get_title",
            "What the web app's Get button does: add a movie, show, season or episode and have Arr find it, optionally at a quality the person picked (smaller 720p, standard 1080p, best 4K). A show is followed for new episodes and its latest season is fetched. add_media is the narrower tool for exactly one movie, season or episode without a quality; use this one when the person names a quality or wants a whole show. Repeating it creates no duplicate.",
            write_idem.clone(),
        ),
        tool::<SetQuality>(
            "set_quality",
            "Change the quality level (smaller, standard, best) of a movie or show Arr already knows. It changes what Arr looks for next; files already in Plex are never touched or replaced.",
            write_idem.clone(),
        ),
        tool::<FollowSeries>(
            "follow_series",
            "Turn on or off getting a show's new episodes as they air (what the web app's Follow does). Turning it off deletes nothing.",
            write_idem.clone(),
        ),
        tool::<RemoveTitle>(
            "remove_title",
            "Arr stops tracking a movie or a whole show: its unfinished downloads are cancelled and its requests end. Files already in Plex or the library are never touched (to delete files use delete_preview). Confirm with the person which title first.",
            destructive.clone(),
        ),
        tool::<Copies>(
            "copies",
            "The copies Arr found for a movie, season or episode, in plain words (resolution, source, size, speed), with the one Arr would pick marked `recommended` and counts of why others were skipped. Use it when the person wants to see choices; the choice is theirs. A copy that would replace a file already owned is never listed.",
            read.clone(),
        ),
        tool::<GetCopy>(
            "get_copy",
            "Download the one copy the person chose from copies, for the same canonical id. Never choose for them. A title Arr does not have yet is added first, without a search of its own. Repeating it creates no duplicate. (release_choices and start_download are the older, more detailed way to do the same.)",
            write.clone(),
        ),
        tool::<RetryDownload>(
            "retry_download",
            "Try a failed download again (what the web app's Try again does for a download that went wrong).",
            write.clone(),
        ),
        tool::<NoArgs>(
            "feed",
            "The home page: what is on its way, ready, needing attention and coming up (sport included, without results).",
            read.clone(),
        ),
        tool::<NoArgs>(
            "attention",
            "Items that need someone to look at them (failed or stuck downloads and the like).",
            read.clone(),
        ),
        tool::<NoArgs>(
            "ideas",
            "Popular shows with new episodes this week that nobody has hidden, each saying whether it is already here.",
            read.clone(),
        ),
        tool::<HideIdea>(
            "hide_idea",
            "\"Not interested\": hide a show from Ideas for everyone using this Arr (or show it again with hidden false). Changes nothing else: no download, file or library entry.",
            write_idem.clone(),
        ),
        tool::<NoArgs>(
            "storage",
            "Free and total space of the drive(s) holding the movies and TV.",
            read.clone(),
        ),
        tool::<Calendar>(
            "calendar",
            "What airs or releases in a window (default: yesterday to two weeks ahead), movies and episodes, with sport events listed without results.",
            read.clone(),
        ),
        tool::<Wanted>(
            "wanted",
            "What Arr is waiting for: monitored, aired, missing and not downloading, with paging.",
            read.clone(),
        ),
        tool::<History>(
            "history",
            "What Arr did (grabbed, imported, deleted, restored and so on), newest first, with paging.",
            read.clone(),
        ),
        tool::<TitleDetails>(
            "title_details",
            "The page of a movie, a show or one season: state, details, scores, cast, the quality level and what each level looks like, and for a season its episodes.",
            read.clone(),
        ),
        tool::<NoArgs>(
            "system_health",
            "How this Arr is doing: what is working, what needs the person who runs it.",
            read.clone(),
        ),
        tool::<DeletePreview>(
            "delete_preview",
            "Step one of deleting files: shows exactly what deleting a movie, show, season or episode would move to the bin (the files, their sizes, the downloads that only seed them, anything blocking) and returns a token. Deletes nothing. Show the person the whole preview.",
            ToolAnnotations::new()
                .read_only(true)
                .destructive(false)
                .open_world(true),
        ),
        tool::<DeleteFiles>(
            "delete_files",
            "DELETES FILES: moves a movie, show, season or episode out of the library into the bin, where it can be put back for 7 days (bin_restore), then it is gone for good. Only call it after delete_preview of exactly this media_id, after you have shown the person the preview (title, files, sizes) and they explicitly said yes to deleting exactly that; then pass the preview's token unchanged and confirmed true. Never infer consent, never delete a wider scope than the person named, and never reuse a token for another scope. If anything changed since the preview Arr refuses (changed): preview again and ask again. By default Arr stops wanting the title; keep_wanted true makes it fetch a new copy instead.",
            destructive.clone(),
        ),
        tool::<DownloadArg>(
            "download",
            "One download in detail: its state and progress, what Arr did with it (events) and how its import went. Take the grab_id from requests or the feed.",
            read.clone(),
        ),
        tool::<DownloadArg>(
            "pause_download",
            "Pause one unfinished download. Resume it with resume_download.",
            write_idem.clone(),
        ),
        tool::<DownloadArg>(
            "resume_download",
            "Resume a paused download.",
            write_idem.clone(),
        ),
        tool::<DownloadArg>(
            "cancel_download",
            "Cancel one download (its id is from requests); its data is kept and nothing already in Plex is touched. To stop getting a whole title use stop. Confirm with the person which download first.",
            destructive.clone(),
        ),
        tool::<BlocklistArg>(
            "blocklist",
            "Copies Arr will not try again because they failed before, newest first, optionally for one title. Sport entries show the event's name and a plain reason, never a release's own text.",
            read.clone(),
        ),
        tool::<Unblock>(
            "unblock",
            "Take one entry off the blocklist (its id is from blocklist) so Arr may try that copy again. Forgiving a copy deletes nothing.",
            write_idem.clone(),
        ),
        tool::<RunCommand>(
            "run_command",
            "Ask Arr to do a background job now: rescan the library folders, look for everything that is missing, check the indexers' newest releases, refresh details, search for one title, or ask Plex to look again at one title. It is queued, not finished: read the result with command_status. Asking for one that is already pending does not queue another. Only with execution enabled.",
            write_idem.clone(),
        ),
        tool::<CommandStatus>(
            "command_status",
            "The state and result of one background job (by id from run_command), or the latest jobs when no id is given.",
            read.clone(),
        ),
        tool::<NoArgs>(
            "library_folders",
            "The library folders Arr watches and the status of their last rescan (ok, unavailable, incomplete, empty_suspect); only ok scans change what is owned.",
            read.clone(),
        ),
        tool::<NoArgs>(
            "bin",
            "What was deleted and can still be put back: each entry's delete_id, title, files, size, when it was deleted and when it goes for good.",
            read.clone(),
        ),
        tool::<BinRestore>(
            "bin_restore",
            "Put a deleted entry's files back where they were (within 7 days of deleting it). A file whose place was taken stays in the bin and is reported. Nothing is wanted again by this.",
            write.clone(),
        ),
        tool::<BinEmpty>(
            "bin_empty",
            "PERMANENTLY deletes one bin entry now; it cannot be put back and its space comes back. Only after showing the person that entry (from bin) and getting their explicit yes to deleting it for good; pass its delete_id, its bytes as bin shows them and confirmed true. Refused if the entry's size no longer matches what the person saw.",
            destructive,
        ),
    ]
}

fn text(q: &str) -> Result<String, CallToolResult> {
    let q = q.trim();
    if q.is_empty() || q.chars().count() > MAX_QUERY_CHARS || q.chars().any(char::is_control) {
        return Err(invalid(&format!(
            "q must be 1 to {MAX_QUERY_CHARS} printable characters"
        )));
    }
    Ok(q.to_string())
}

/// A canonical id of one of the allowed kinds, or the refusal.
fn id_of(raw: &str, allowed: fn(&MediaId) -> bool, what: &str) -> Result<MediaId, CallToolResult> {
    match raw.parse::<MediaId>() {
        Ok(id) if allowed(&id) && id.to_string() == raw => Ok(id),
        Ok(MediaId::Event(_)) => Err(invalid(
            "sport events are not titles: use the sport_* tools for them",
        )),
        _ => Err(invalid(&format!(
            "media_id must be a canonical id of {what}"
        ))),
    }
}

fn any_title(id: &MediaId) -> bool {
    !matches!(id, MediaId::Event(_))
}

fn unit(id: &MediaId) -> bool {
    matches!(
        id,
        MediaId::Movie { .. } | MediaId::Season(..) | MediaId::Episode(..)
    )
}

fn movie_or_series(id: &MediaId) -> bool {
    matches!(id, MediaId::Movie { .. } | MediaId::Series(_))
}

fn series(id: &MediaId) -> bool {
    matches!(id, MediaId::Series(_))
}

fn page_query(limit: Option<u32>, offset: Option<u32>) -> Vec<(&'static str, String)> {
    let mut q = vec![];
    if let Some(l) = limit {
        q.push(("limit", l.to_string()));
    }
    if let Some(o) = offset {
        q.push(("offset", o.to_string()));
    }
    q
}

fn level_query(level: &Option<Level>) -> Vec<(&'static str, String)> {
    level
        .iter()
        .map(|l| ("level", l.as_str().to_string()))
        .collect()
}

impl Adapter {
    async fn read(&self, segments: &[&str], query: &[(&str, String)]) -> CallToolResult {
        respond(self.call(Effect::Read, segments, query, None).await)
    }

    async fn write(
        &self,
        effect: Effect,
        segments: &[&str],
        body: Option<Value>,
    ) -> CallToolResult {
        respond(self.call(effect, segments, &[], body).await)
    }

    /// The tool called `name` with its arguments, or `None` when there is no such tool.
    pub(super) async fn dispatch(
        &self,
        name: &str,
        args: serde_json::Map<String, Value>,
    ) -> Option<CallToolResult> {
        // What goes into a URL path (or is checked by pattern in the schema) is checked here
        // too: a client that ignores the schema cannot make a request the schema forbids.
        for (tool, field, pattern) in SHAPES {
            if *tool == name
                && let Some(v) = args.get(*field).and_then(Value::as_str)
                && !regex::Regex::new(pattern).is_ok_and(|re| re.is_match(v))
            {
                return Some(invalid(&format!(
                    "{field} is not in the form this tool takes (see its schema)"
                )));
            }
        }
        macro_rules! run {
            ($t:ty, |$a:ident| $body:expr) => {{
                match parse::<$t>(args) {
                    Ok($a) => $body.await,
                    Err(r) => r,
                }
            }};
        }
        Some(match name {
            "sport_search" => run!(TextQuery, |a| async {
                match text(&a.q) {
                    Ok(q) => {
                        self.read(&["api", "v1", "sport", "search"], &[("q", q)])
                            .await
                    }
                    Err(r) => r,
                }
            }),
            "sport_leagues" => run!(NoArgs, |_a| self
                .read(&["api", "v1", "sport", "leagues"], &[])),
            "sport_league" => run!(SportLeague, |a| self
                .read(&["api", "v1", "sport", "league", &a.league], &[])),
            "sport_follow" => run!(SportFollowArg, |a| self.write(
                Effect::Change,
                &["api", "v1", "sport", "follow"],
                Some(sport_follow_body(&a))
            )),
            "sport_unfollow" => run!(SportFollowArg, |a| self.write(
                Effect::Change,
                &["api", "v1", "sport", "unfollow"],
                Some(sport_follow_body(&a))
            )),
            "sport_following" => run!(NoArgs, |_a| self
                .read(&["api", "v1", "sport", "following"], &[])),
            "sport_events" => run!(SportEvents, |a| async {
                let mut q = vec![];
                for (k, v) in [("from", &a.from), ("to", &a.to), ("league", &a.league)] {
                    if let Some(v) = v {
                        q.push((k, v.clone()));
                    }
                }
                self.read(&["api", "v1", "sport", "events"], &q).await
            }),
            "sport_copies" => run!(SportCopies, |a| async {
                let mut q = vec![];
                if let Some(p) = &a.part {
                    q.push(("part", p.as_str().to_string()));
                }
                if let Some(f) = a.fresh {
                    q.push(("fresh", f.to_string()));
                }
                self.read(&["api", "v1", "sport", "event", &a.event_id, "copies"], &q)
                    .await
            }),
            "sport_get" => run!(SportGet, |a| self.write(
                Effect::SportGet,
                &["api", "v1", "sport", "event", &a.event_id, "get"],
                Some(json!({ "id": a.id, "confirm": a.confirm.unwrap_or(false) }))
            )),
            "people_search" => run!(TextQuery, |a| async {
                match text(&a.q) {
                    Ok(q) => self.read(&["api", "v1", "people"], &[("q", q)]).await,
                    Err(r) => r,
                }
            }),
            "person" => run!(PersonArg, |a| self
                .read(&["api", "v1", "people", &a.person_id], &[])),
            "get_title" => run!(GetTitle, |a| async {
                match id_of(&a.media_id, any_title, "a movie, show, season or episode") {
                    Ok(id) => {
                        let mut body = json!({ "media_id": id.to_string() });
                        if let Some(l) = &a.level {
                            body["level"] = json!(l.as_str());
                        }
                        self.write(Effect::GetTitle, &["api", "v1", "quick-add"], Some(body))
                            .await
                    }
                    Err(r) => r,
                }
            }),
            "set_quality" => run!(SetQuality, |a| async {
                match id_of(&a.media_id, movie_or_series, "a movie or a show") {
                    Ok(id) => {
                        self.write(
                            Effect::Change,
                            &["api", "v1", "quality"],
                            Some(json!({ "media_id": id.to_string(), "level": a.level.as_str() })),
                        )
                        .await
                    }
                    Err(r) => r,
                }
            }),
            "follow_series" => run!(FollowSeries, |a| async {
                match id_of(&a.media_id, series, "a show") {
                    Ok(id) => {
                        let mut body = json!({ "media_id": id.to_string(), "on": a.on });
                        if let Some(l) = &a.level {
                            body["level"] = json!(l.as_str());
                        }
                        self.write(Effect::Change, &["api", "v1", "follow"], Some(body))
                            .await
                    }
                    Err(r) => r,
                }
            }),
            "remove_title" => run!(RemoveTitle, |a| async {
                match id_of(&a.media_id, movie_or_series, "a movie or a show") {
                    Ok(id) => {
                        self.write(
                            Effect::Change,
                            &["api", "v1", "remove"],
                            Some(json!({ "media_id": id.to_string() })),
                        )
                        .await
                    }
                    Err(r) => r,
                }
            }),
            "copies" => run!(Copies, |a| async {
                match id_of(&a.media_id, unit, "a movie, season or episode") {
                    Ok(id) => {
                        self.read(
                            &["api", "v1", "copies", &id.to_string()],
                            &level_query(&a.level),
                        )
                        .await
                    }
                    Err(r) => r,
                }
            }),
            "get_copy" => run!(GetCopy, |a| async {
                match id_of(&a.media_id, unit, "a movie, season or episode") {
                    Ok(id) => {
                        let mut body = json!({ "id": a.id });
                        if let Some(l) = &a.level {
                            body["level"] = json!(l.as_str());
                        }
                        self.write(
                            Effect::CopyGet,
                            &["api", "v1", "copies", &id.to_string(), "get"],
                            Some(body),
                        )
                        .await
                    }
                    Err(r) => r,
                }
            }),
            "retry_download" => run!(RetryDownload, |a| self.write(
                Effect::Change,
                &["api", "v1", "queue", &a.grab_id, "retry"],
                None
            )),
            "feed" => run!(NoArgs, |_a| self.read(&["api", "v1", "feed"], &[])),
            "attention" => run!(NoArgs, |_a| self.read(&["api", "v1", "attention"], &[])),
            "ideas" => run!(NoArgs, |_a| self.read(&["api", "v1", "ideas"], &[])),
            "hide_idea" => run!(HideIdea, |a| async {
                match id_of(&a.media_id, series, "a show") {
                    Ok(id) => {
                        let id = id.to_string();
                        if a.hidden.unwrap_or(true) {
                            self.write(
                                Effect::Change,
                                &["api", "v1", "ideas", "hide"],
                                Some(json!({ "media_id": id })),
                            )
                            .await
                        } else {
                            self.write(
                                Effect::ChangeDelete,
                                &["api", "v1", "ideas", "hide", &id],
                                None,
                            )
                            .await
                        }
                    }
                    Err(r) => r,
                }
            }),
            "storage" => run!(NoArgs, |_a| self.read(&["api", "v1", "storage"], &[])),
            "calendar" => run!(Calendar, |a| async {
                let mut q = vec![];
                for (k, v) in [("start", &a.start), ("end", &a.end)] {
                    if let Some(v) = v {
                        q.push((k, v.clone()));
                    }
                }
                self.read(&["api", "v1", "calendar"], &q).await
            }),
            "wanted" => run!(Wanted, |a| async {
                let mut q = page_query(a.limit, a.offset);
                if let Some(m) = &a.media_id {
                    match id_of(m, any_title, "a title") {
                        Ok(id) => q.push(("media_id", id.to_string())),
                        Err(r) => return r,
                    }
                }
                self.read(&["api", "v1", "wanted"], &q).await
            }),
            "history" => run!(History, |a| async {
                let mut q = page_query(a.limit, a.offset);
                if let Some(m) = &a.media_id {
                    match id_of(m, any_title, "a title") {
                        Ok(id) => q.push(("media_id", id.to_string())),
                        Err(r) => return r,
                    }
                }
                if let Some(k) = &a.kind {
                    q.push(("kind", k.clone()));
                }
                self.read(&["api", "v1", "history"], &q).await
            }),
            "title_details" => run!(TitleDetails, |a| async {
                match id_of(
                    &a.media_id,
                    |i| !matches!(i, MediaId::Event(_) | MediaId::Episode(..)),
                    "a movie, show or season",
                ) {
                    Ok(MediaId::Movie { tmdb }) => {
                        self.read(&["api", "v1", "movie", &format!("movie:tmdb:{tmdb}")], &[])
                            .await
                    }
                    Ok(MediaId::Season(k, n)) => {
                        self.read(
                            &[
                                "api",
                                "v1",
                                "show",
                                &MediaId::Series(k).to_string(),
                                "season",
                                &n.to_string(),
                            ],
                            &[],
                        )
                        .await
                    }
                    Ok(id) => {
                        self.read(&["api", "v1", "show", &id.to_string()], &[])
                            .await
                    }
                    Err(r) => r,
                }
            }),
            "system_health" => run!(NoArgs, |_a| self
                .read(&["api", "v1", "system", "health"], &[])),
            "delete_preview" => run!(DeletePreview, |a| async {
                match id_of(&a.media_id, any_title, "a movie, show, season or episode") {
                    Ok(id) => {
                        self.write(
                            Effect::Preview,
                            &["api", "v1", "delete", "preview"],
                            Some(json!({ "media_id": id.to_string() })),
                        )
                        .await
                    }
                    Err(r) => r,
                }
            }),
            "delete_files" => run!(DeleteFiles, |a| self.delete_files(a)),
            "download" => run!(DownloadArg, |a| self
                .read(&["api", "v1", "queue", &a.grab_id], &[])),
            "pause_download" => run!(DownloadArg, |a| self.write(
                Effect::Download,
                &["api", "v1", "queue", &a.grab_id, "pause"],
                None
            )),
            "resume_download" => run!(DownloadArg, |a| self.write(
                Effect::Download,
                &["api", "v1", "queue", &a.grab_id, "resume"],
                None
            )),
            "cancel_download" => run!(DownloadArg, |a| self.write(
                Effect::Download,
                &["api", "v1", "queue", &a.grab_id, "cancel"],
                None
            )),
            "blocklist" => run!(BlocklistArg, |a| async {
                let mut q = vec![];
                if let Some(m) = &a.media_id {
                    match id_of(m, any_title, "a title") {
                        Ok(id) => q.push(("media_id", id.to_string())),
                        Err(r) => return r,
                    }
                }
                self.read(&["api", "v1", "blocklist"], &q).await
            }),
            "unblock" => run!(Unblock, |a| self.write(
                Effect::ChangeDelete,
                &["api", "v1", "blocklist", &a.id.to_string()],
                None
            )),
            "run_command" => run!(RunCommand, |a| async {
                let media = match (&a.media_id, a.name.needs_title()) {
                    (Some(m), true) => match id_of(m, any_title, "a title") {
                        Ok(id) => Some(id.to_string()),
                        Err(r) => return r,
                    },
                    (None, true) => {
                        return invalid("this command needs a media_id");
                    }
                    (Some(_), false) => {
                        return invalid("this command takes no media_id");
                    }
                    (None, false) => None,
                };
                let mut body = json!({ "name": a.name.as_str() });
                if let Some(m) = media {
                    body["media_id"] = json!(m);
                }
                self.write(Effect::Command, &["api", "v1", "commands"], Some(body))
                    .await
            }),
            "command_status" => run!(CommandStatus, |a| async {
                match &a.id {
                    Some(id) => self.read(&["api", "v1", "commands", id], &[]).await,
                    None => {
                        self.read(&["api", "v1", "commands"], &page_query(a.limit, a.offset))
                            .await
                    }
                }
            }),
            "library_folders" => run!(NoArgs, |_a| self
                .read(&["api", "v1", "library", "roots"], &[])),
            "bin" => run!(NoArgs, |_a| self.read(&["api", "v1", "bin"], &[])),
            "bin_restore" => run!(BinRestore, |a| self.write(
                Effect::Restore,
                &["api", "v1", "bin", &a.delete_id, "restore"],
                None
            )),
            "bin_empty" => run!(BinEmpty, |a| self.bin_empty(a)),
            _ => return None,
        })
    }

    /// Deletes what the person was shown and said yes to, and nothing else. The scope is
    /// previewed again here and the token must be the one that preview gives (a token binds
    /// the media id and every file with its size and time), so a token for another scope, a
    /// stale one or an invented one is refused before anything is sent to the delete. The
    /// API checks the same token again before it moves a file.
    async fn delete_files(&self, a: DeleteFiles) -> CallToolResult {
        if !a.confirmed {
            return invalid(
                "confirmed must be true, and only after the person has seen the preview of exactly this media_id and said yes to deleting it",
            );
        }
        let id = match id_of(&a.media_id, any_title, "a movie, show, season or episode") {
            Ok(id) => id.to_string(),
            Err(r) => return r,
        };
        let keep_wanted = a.keep_wanted.unwrap_or(false);
        let now = match self
            .call(
                Effect::Preview,
                &["api", "v1", "delete", "preview"],
                &[],
                Some(json!({ "media_id": id })),
            )
            .await
        {
            Ok(v) => v,
            Err(r) => return r,
        };
        let refused = |code: &str, message: &str| {
            CallToolResult::structured_error(json!({
                "error": { "code": code, "message": message },
                "outcome": "not_applied",
            }))
        };
        if now["token"].as_str() != Some(a.token.as_str()) {
            return refused(
                "changed",
                "this is not the preview of what is there now (a different scope, or the files changed since). Preview again, show the person, and ask again.",
            );
        }
        if now["files"].as_array().is_none_or(Vec::is_empty) {
            return refused(
                "delete_refused",
                "There are no files Arr knows of to delete.",
            );
        }
        if let Some(why) = now["blocked"].as_str()
            && !keep_wanted
        {
            return refused("delete_refused", why);
        }
        self.write(
            Effect::Delete,
            &["api", "v1", "delete"],
            Some(json!({ "media_id": id, "token": a.token, "keep_wanted": keep_wanted })),
        )
        .await
    }

    /// Empties one bin entry for good, but only while it is the entry the person saw: the
    /// entry is read first and its size must still be the one the person was shown.
    async fn bin_empty(&self, a: BinEmpty) -> CallToolResult {
        if !a.confirmed {
            return invalid(
                "confirmed must be true, and only after the person has seen this bin entry and said yes to deleting it for good",
            );
        }
        let bin = match self
            .call(Effect::Read, &["api", "v1", "bin"], &[], None)
            .await
        {
            Ok(v) => v,
            Err(r) => return r,
        };
        let entry = bin["entries"]
            .as_array()
            .and_then(|e| e.iter().find(|e| e["delete_id"] == a.delete_id.as_str()));
        match entry {
            None => CallToolResult::structured_error(json!({
                "error": { "code": "not_found", "message": "nothing in the bin has that id" },
                "outcome": "not_applied",
            })),
            Some(e) if e["bytes"].as_u64() != Some(a.bytes) => {
                CallToolResult::structured_error(json!({
                    "error": {
                        "code": "changed",
                        "message": "this bin entry is not the one the person was shown (its size differs). Look at the bin again and ask again.",
                    },
                    "outcome": "not_applied",
                }))
            }
            Some(_) => {
                self.write(Effect::Purge, &["api", "v1", "bin", &a.delete_id], None)
                    .await
            }
        }
    }
}

fn sport_follow_body(a: &SportFollowArg) -> Value {
    let mut b = json!({ "league": a.league });
    if let Some(t) = &a.team_id {
        b["team_id"] = json!(t);
    }
    b
}
