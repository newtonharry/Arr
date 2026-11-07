# Execution sandbox guide

This guide explains how to run the execution paths against an **isolated sandbox**: a
dedicated qBittorrent, dedicated download and library directories, and mock or isolated
Prowlarr, TVmaze, TMDB and Plex services. Execution is disabled by default
(`[execution] enabled = false`). It must never point at production clients, categories,
paths or libraries.

Everything here is also exercised by the test suite against loopback mocks with generated,
real, playable media (`tests/execution.rs`, `ingest.rs`, `cleanup.rs`, `rss.rs`). The
`#[ignore]` harness `tests/sandbox.rs` runs the same flow against a real qBittorrent; see
the end of this guide.

## 1. Requirements

| component | requirement |
|---|---|
| qBittorrent | 4.3.9 (WebAPI 2.8.2) up to 5.x (2.11+). WebUI enabled, its own user/password, its own save directory. |
| download directory | One directory, visible to qBittorrent as `save_path` and to arr as `local_path`. arr needs read access to import and **write** access to finish a journaled cleanup. |
| library roots | One movie root and one TV root, used only by the sandbox. Put them on the **same filesystem** as the download directory if hardlinks should be used (native `copyUsingHardlinks`); otherwise files are copied. Roots and the download directory must not contain each other. |
| ffprobe | `/usr/bin/ffprobe` (Debian package `ffmpeg`; the Docker runtime image installs it). Override with `[library] ffprobe`. |
| Prowlarr | The configured `[prowlarr] base_url`, either a sandbox Prowlarr or a mock implementing the contract in section 6. |
| metadata | `[tvmaze]` for series outside the snapshot (a mock is fine). `[tmdb]` is optional; without it, movies outside the snapshot need explicit metadata. |
| Plex | Optional `[plex]` (a mock is fine). |

## 2. Configuration

Use a fresh state directory (`arr init`), then import a snapshot (sanitized or
sandbox). Each secret goes in its own owner-only file (mode 0600, no other content).

```toml
[server]
listen = "127.0.0.1:8787"
api_token_file = "secrets/api_token"

[prowlarr]
base_url = "http://127.0.0.1:19696"       # sandbox Prowlarr or mock
api_key_file = "secrets/prowlarr_api_key"

[tvmaze]
enabled = true
base_url = "http://127.0.0.1:19001"       # mock or the public API

[execution]
enabled = true                            # sandbox only
download_cleanup = true                   # follow imported removeCompleted/FailedDownloads

[qbittorrent]
base_url = "http://127.0.0.1:18080"
username_file = "secrets/qbittorrent_username"
password_file = "secrets/qbittorrent_password"
save_path = "/downloads"                  # as qBittorrent sees it
local_path = "/srv/sandbox/downloads"     # the same directory as arr sees it
movie_category = "um-sandbox-movies"      # required in coexistence: no native app uses it
tv_category = "um-sandbox-tv"

[migration]
mode = "coexistence"                      # required with execution: coexistence or legacy

[library]
movies = "/srv/sandbox/library/movies"    # stands for the imported Radarr root
tv = "/srv/sandbox/library/tv"            # stands for the imported Sonarr root
# movies_native_root = "/movies"          # needed only if the import has several roots
# tv_native_root = "/tvshows"
# sample_detection = true                 # see section 5
# probe_timeout_secs = 600

[automation]
# rss_interval_minutes = 15               # default: the imported rssSyncInterval
poll_seconds = 15

[plex]                                    # optional
base_url = "http://127.0.0.1:19032"
token_file = "secrets/plex_token"
movie_section = 1
tv_section = 2
```

`arr check --config STATE/config.toml` validates the file modes and paths without
touching the network. With `[execution] enabled = true` the server also needs
`[qbittorrent]`, `[library]`, `[prowlarr]` and an explicit `[migration] mode`, or startup
fails. In coexistence mode a native title is acquired only after `POST /api/v1/library`
with `native_handoff_confirmed: true` (see [MIGRATION.md](MIGRATION.md#coexistence-with-the-running-native-apps)). `serve` then starts the
background worker. You can record verification evidence for the capability matrix in
`STATE/verification.toml`:

```toml
[verified]
download_submission = "sandbox"     # levels: sandbox | production (operator-written only)
completed_download_import = "sandbox"
```

## 3. Ownership, paths and what is never touched

- Every submitted torrent gets the configured category and the tag `unified-media`. Its
  info-hash is stored before the add call. A torrent is **ours** only if its hash belongs to
  one of our grabs, its category matches and it has the tag. A torrent with the same hash but
  another category or no tag is **foreign**. It is never paused, resumed, removed or deleted,
  and its grab fails with `foreign_torrent`.
- The add sends only parameters 4.3.9 knows: `torrents` or `urls`, `savepath`, `category`,
  `tags`, `autoTMM=false`, `paused` (`stopped` on 5.x), `sequentialDownload`,
  `firstLastPiecePrio`, `ratioLimit` and `seedingTimeLimit` (from the indexer seed criteria),
  and `contentLayout` only when the imported client sets one. 4.x uses `pause`/`resume`, 5.x
  `stop`/`start`.
- Downloads are mapped from the reported `save_path` into `local_path`. A save path outside
  the mapped directory blocks import and cleanup.
- Library imports never overwrite. Destination folders are `Movie Title (Year)/` and
  `Series Title/Season N/`, the folders recorded in the import when known. With rename off,
  file names follow native rules: a single-file or single-episode download takes the
  download title plus the file's extension, and a season pack keeps the original file names.
- Library files are never deleted. The only exception is a native revision upgrade, which
  replaces the managed file of the same item: the old file is moved to a sidecar backup until
  the new file is published and committed. Removing an item from management keeps its files.
  Cancelling keeps the torrent and its data, and stops an incomplete torrent.

## 4. API flow (sandbox walkthrough)

Use `Authorization: Bearer $(cat STATE/secrets/api_token)` on every request. Bodies are JSON.

1. **Health and gates:** `GET /api/v1/readiness` shows `mode: execution` and one gate per
   media kind (`execution_radarr`, `execution_sonarr`). `GET /api/v1/diagnostics` makes a
   live qBittorrent call and reports the WebAPI version, the 4.x/5.x naming, categories,
   global share limits and warnings, plus ffprobe, library roots and their free space,
   execution blockers, download counts and RSS state. `GET /api/v1/capabilities` gives the
   implemented/configured/verified matrix.
2. **Add an item (one call; search is the default):**
   - `POST /api/v1/library {"media_id":"season:tvdb:N:1"}` adds the series if needed and
     monitors and searches **only season 1**. Other seasons, and seasons that appear later,
     stay unmonitored on a new series. Use `episode:tvdb:N:S:E` for one episode,
     `series:tvdb:N` for everything (TVmaze needed for series outside the snapshot).
   - `POST /api/v1/library {"media_id":"movie:tmdb:N"}` uses TMDB when configured (title and
     release dates). Without TMDB, pass explicit metadata, including a release date or the
     movie stays not available:
     `{"media_id":"movie:tmdb:N","metadata":{"title":"Parity Feature","year":2026,"runtime_minutes":2,"digital_release":"2026-01-01"}}`.
   - The item's folder is **scanned first**: files already there are owned and never grabbed.
     The response's `scan` says what was found (`ok`, `no_folder`, `unavailable`, ...).
   - `"search": false` only adds and monitors. The profile and root folder are chosen
     automatically when there is exactly one candidate; otherwise pass `quality_profile`
     or `root_folder`.
   - A movie, season or episode returns its executed plan (`download_queued` until
     qBittorrent confirms, then `download_started`). A whole series returns a queued
     `search` command.
3. **Interactive grab:** `GET /api/v1/media/{id}/releases`, then
   `POST /api/v1/grabs {"media_id":"…","release_ref":"rel_…"}` with an **approved**
   `release_ref` (link material is kept privately for 7 days).
4. **Automatic plan:** `POST /api/v1/plans {"media_id":"season:tvdb:N:1","execute":true}`.
   To execute a shadow plan later, use `POST /api/v1/plans/{id}/execute`. The plan records
   `execution.grabs`, `execution.refused` and `execution.outcome`: `queued`,
   `partially_queued`, or `nothing_queued` (the plan stays `shadow_planned`/`shadow_partial`).
   After a refusal, the request stays active while a refused unit is not covered by an
   active download. Its state moves through `download_queued`
   (in the outbox; `submitted: false`) and `download_started` (qBittorrent confirmed;
   `submitted: true`, `confirmed_at`) to `imported`. During a client outage it stays
   `download_queued`.
5. **Queue:**
   - `GET /api/v1/queue` lists active downloads and those still tracked for cleanup.
   - `GET /api/v1/queue?all=true` adds anything updated in the last 7 days.
   - `GET /api/v1/queue/{grab_id}` shows one download with its import journal and events.
   - Each item has `state` (outbox state), `status` (user-facing: `pending_submission`,
     `submitting`, `downloading`, `paused`, `stalled`, `fetching_metadata`, `client_error`,
     `missing_files`, `checking`, `import_pending`, `importing`, `import_blocked`,
     `imported`, `seeding`, `cleanup_pending`, `cleanup_blocked`, `cleaned`, `failed`,
     `cancelled` or `released`) and `cleanup.state`/`cleanup.detail`.
6. **Actions:**
   - `POST /api/v1/queue/{id}/pause` and `/resume` act on our torrent only.
   - `POST /api/v1/queue/{id}/cancel` takes `{"blocklist":false,"remove":"keep|torrent|torrent_and_data","ignore_seed_criteria":false,"research":false}`.
     Without `research`, the cancelled targets wait 24 h before being searched again.
     `torrent_and_data` removes only our torrent and its tracked files. While the seed goal is
     unmet it answers `409 seed_criteria_unmet` unless `ignore_seed_criteria` is set.
   - `POST /api/v1/queue/{id}/retry` answers with `action`:
     - `reimport` for `import_blocked`;
     - `resubmit` for a pending submission;
     - `cleanup` to re-evaluate a blocked cleanup;
     - `search` to find an alternative for a failed download.
7. **History and blocklist:** `GET /api/v1/history?media_id=…&kind=…`,
   `GET /api/v1/blocklist`, `DELETE /api/v1/blocklist/{entry_id}`.
8. **Commands:** `POST /api/v1/commands {"name":…,"media_id":…}` answers `202 Accepted`, or
   `200` when an identical command is already pending. Commands:
   - `search` (movie, series, season or episode),
   - `search_missing` (the backlog, in batches of `searches_per_run` per pass),
   - `rss_sync`, `refresh_metadata`, `rescan`, `poll_downloads`, `cleanup_downloads`,
   - `plex_refresh` (needs a media_id).

   Follow progress with `GET /api/v1/commands/{id}`. `GET /api/v1/system/tasks` shows the
   scheduled tasks; `GET /api/v1/requests` shows explicit requests being retried.
9. **RSS:** `GET /api/v1/rss` shows the feeds (RSS-enabled imported indexers), the
   per-indexer state (last success, newest publish date, failures, backoff, `gap_possible`)
   and the last 200 items seen, with their status (`grabbed`, `rejected`, `deferred` or
   `ignored`) and the reason. It never shows links.

## 5. Import, identity, quality, sample detection and media validation

**Payload identity.** A release is judged by its own names as well as its search title:

- the `.torrent` name and video file paths, before submission;
- the torrent name, folders and file names, at import;
- file names, in scans.

A name that clearly belongs to something else is a contradiction: a different title with
season/episode numbering (TV), or a different title or year (movies). A contradiction is
never overridden by an approved search title. The release is blocklisted (`wrong_payload`)
and a valid alternative is searched automatically. Obfuscated names without such markers
(`a1b2c3.mkv`, `S01E02.mkv`) are judged by the validated grab context. A scan never
attributes a file that names another title to the folder's item.

**Title keys.** Decisions, payload identity, scans and RSS share one set of title keys
(`catalog::title_keys`): the title and each alias, with and without a `(YYYY)`
disambiguator. One derived form is added, taken only from the item's own metadata. When the
canonical title is a prefix plus a known alias (for example a franchise name plus the bare
title), any longer alias ending with that bare title is also known with the prefix in front.
So `Nebula Arin: The Last Keeper` with aliases `Arin: The Last Keeper` and
`The Legend of Arin: The Last Keeper` also matches `Nebula The Legend of Arin The Last
Keeper`. Nothing is stripped from release names and no substring is matched. A prefix the
metadata doesn't contain, another year, or TV numbering on a movie still contradicts.

**Payload language.** Explicit language markers in the payload's names (file name first,
then download name) are checked against the profile, like release titles. A RUSSIAN payload
for an English movie profile fails (`wrong_language`), is blocklisted and replaced. The TV
profile (any language) accepts it. Unmarked names rely on the approved release.

**Effective quality.** The file's quality is its source with the resolution measured by
ffprobe. The source is the one the payload names (file name, then download name), otherwise
the one the release advertised. The native MediaInfo thresholds use width or height, so cropped
1920x800 stays 1080p. The imported profile applies to this effective quality: a "1080p"
release that is really 2160p fails with `wrong_quality`, is blocklisted and replaced. A
revision upgrade must have exactly the owned file's quality, so a lower-resolution PROPER
fails. The recorded file quality is the effective one; scans correct it after probing.

Completed torrents are imported on the next worker pass:

1. Video files are selected. Extras and sample folders, sample names, files not wanted in the
   client, and incomplete files are skipped.
2. Each file is mapped to the grab's exact targets (episode numbering per file;
   multi-episode files own every episode they name).
3. Each file is probed with ffprobe, which must read every packet of a real video stream.
4. The file is hardlinked or copied into staging. A copy is probed again.
5. It is published without overwriting anything, then committed.

**Native sample detection is on by default.** The minimum duration depends on the item
runtime: ≤3 min → 15 s, ≤10 min → 90 s, ≤30 min → 300 s, otherwise 600 s. A **60-second
fixture is a sample for a 45-minute episode**. For short authored fixtures, either give the
mock metadata a runtime of at most 3 minutes, or set `[library] sample_detection = false`.
Failed media (corrupt, truncated, audio-only, samples) fails the download with
`invalid_media`, blocklists the release, and leaves the item not owned.

`import_blocked` (retried with backoff, or by `retry`) covers:

- an existing destination file;
- missing free space (`minimumFreeSpaceWhenImporting`);
- symlinks in the download path;
- several video files for one movie;
- an unmapped root;
- import-blocking imported settings.

## 6. Mock Prowlarr contract

arr only calls the configured Prowlarr origin:

| call | use |
|---|---|
| `GET {base}api/v1/search?query&type=search&indexerIds&categories&limit&offset` (`X-Api-Key`) | searches; JSON array of `{guid,title,size,indexerId,indexer,seeders,leechers,protocol,publishDate,infoHash,downloadUrl,magnetUrl,categories:[{id}]}` |
| `GET {base}{indexerId}/download?…` or `{base}api/v1/indexer/{id}/download?…` (`X-Api-Key`) | the `.torrent` for a `downloadUrl`, or for an HTTP `magnetUrl` (Prowlarr may put the proxy there with no `downloadUrl`). Bounded at 10 MiB; one same-origin redirect or a redirect to a valid `magnet:` is followed; no URL credentials. A literal magnet is used without a fetch, and an unusable field never hides a usable one |
| `GET {base}{indexerId}/api?t=search&cat=…&limit=100&apikey=…` (also `X-Api-Key`) | the Torznab RSS feed of each indexer imported with `enableRss`, every RSS interval |

Feed items need `title` and an `enclosure url` or `link` on the Prowlarr origin (other origins
are dropped). They should also carry `pubDate`, `size`, `guid` and Torznab attributes
`seeders`, `peers`, `infohash` and `category`. A magnet can be given as `magneturl`. The
parser refuses DTDs and caps items (500) and XML nodes. A Torznab `<error code>` document is
reported by code only.

RSS matches only managed, monitored items, by title or alias and year, with season/episode
numbering checked against the item's episode list. Untracked, ambiguous, unmonitored and
wrong-category items are ignored. Matched items go through the normal decisions (profile,
size, seeders, delay, ownership, queued, blocklist, air date, revision upgrades). A grabbed
item is never grabbed again, after a replay or a restart. There is no automatic backlog
search: monitored, missing old episodes are searched only by `search_missing`.

## 7. Cleanup (native `removeCompletedDownloads` / `removeFailedDownloads`)

After a committed import, a download whose imported client has `removeCompletedDownloads`
is tracked as `seeding`. It is removed with its data only when **all** of these hold:

- it is still ours, at the recorded save path, with no symlinked component;
- the library files from it are committed, present and the same size;
- qBittorrent has stopped it (`pausedUP`/`stoppedUP`; a stop alone is not enough);
- the applicable share limit is met: the per-torrent `ratio_limit`/`seeding_time_limit`,
  or the global `max_ratio`/`max_seeding_time` when the torrent uses `-2`. With no limit,
  it is never removed automatically;
- the indexer seed goal recorded at grab time (`seedCriteria.seedRatio`/seed time) is met.

The removal is journaled before the client is asked (`cleanup: removing` with the file list,
sizes and inodes). qBittorrent removes the torrent with `deleteFiles=true`. A restart at any
point finishes the removal: our own deletion only touches journaled files whose inode and
size are unchanged. It never marks the item failed or re-downloads it.

Other cleanup states:

- `blocked`: wrong category or tag, moved, symlinked, or library file changed. The download
  is kept; `retry` re-evaluates.
- `client_removed`: the torrent vanished from the client. Data is left alone.
- `disabled`: the imported flag is off, or `[execution] download_cleanup = false`.
- `kept`: cancelled, or a failed download whose files were imported.

Failed incomplete downloads are removed when `removeFailedDownloads` is set; complete ones
must meet the same seed criteria. The blocklist entry and the retry choice survive cleanup.
The retry choice is `autoRedownloadFailed` (`retry: research` queues a `search` command).

## 8. Restart and ambiguity checks

- **Ambiguous add:** a proxy accepts the add upstream, then withholds the answer. The grab
  stays `unconfirmed`. The next pass, or the first pass after a restart, looks the hash up
  first and confirms it without a second add. Expected: exactly one torrent, and the grab is
  `downloading`.
- **Crash during import:** uncommitted journal rows are rolled back at startup. Our staged or
  published inode is removed, and a moved-aside original is restored. The grab returns to
  `import_pending` and is imported again.
- **Crash during cleanup:** `removing` is finished at startup.

## 9. Generated fixtures

`arr fixture --out DIR --name NAME [--file REL]... [--seconds 60]` writes real
playable 1920x1080 MPEG-4/Matroska files (1 fps solid colour; honest HD for the quality
checks) under `DIR/NAME/`. A single file without
`--file` is written directly in `DIR`. It also writes `DIR/NAME.torrent` with correct piece
hashes, and prints the info-hash, magnet, torrent path and data root as JSON. Copy the data
into the qBittorrent save directory (as mapped by `local_path`) and serve the `.torrent`
from the mock Prowlarr download endpoint. qBittorrent then verifies the pieces and seeds
without any tracker or peer. Nothing is fetched from the internet.

## 10. Real-client harness (`tests/sandbox.rs`)

```bash
UM_SANDBOX_QB_URL=http://127.0.0.1:18080/ \
UM_SANDBOX_QB_USER_FILE=/private/qb_user UM_SANDBOX_QB_PASS_FILE=/private/qb_pass \
UM_SANDBOX_SAVE_PATH=/downloads UM_SANDBOX_LOCAL_DOWNLOADS=/srv/sandbox/downloads \
UM_SANDBOX_LIBRARY=/srv/sandbox/library \
cargo test --locked --test sandbox -- --ignored --nocapture
```

The harness starts an in-process mock Prowlarr and imports the synthetic snapshot. It
generates a movie fixture, places its data in the sandbox download directory, and then:

1. grabs the movie through the API (category `um-sandbox-movies`, tag `unified-media`);
2. waits for the real qBittorrent to verify and seed the torrent;
3. checks the ffprobe-validated import and the hardlink;
4. checks that the source is retained while seeding (no share limit reached);
5. removes the download explicitly with `torrent_and_data` and `ignore_seed_criteria`;
6. checks that the library file is still playable.

It only touches its own category, tag, hash and directories.
