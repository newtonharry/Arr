# Architecture and contracts

This document is the contract for operators and API clients. The machine-readable
API schema is served at `GET /api/v1/openapi.json`. Endpoint details are in
[`docs/API.md`](docs/API.md) and the sandbox procedure in [`docs/EXECUTION-SANDBOX.md`](docs/EXECUTION-SANDBOX.md).

## Boundaries

- The project is one Rust binary, `arr`. It serves the JSON API, a small web app for Plex
  users (sign in with Plex; `webapp.rs`) and remote MCP; it needs no other application but Plex.
  The web app is Preact components written with `h` (no JSX, no build step); Preact is vendored
  in `src/webapp/vendor/` and served by the binary, so nothing is fetched from a CDN.
- **Modes.** With `[execution] enabled = false` (the config default; the installers and the
  wizard turn it on) it runs in **shadow mode**: the
  outbound calls are read-only searches and metadata lookups. Plans are shadow plans, and no
  background worker runs. With `enabled = true` the server also:
  - submits torrents to the configured download client: qBittorrent, or the embedded engine
    (`[embedded]`, librqbit inside this process; [`docs/EMBEDDED-DOWNLOADER.md`](docs/EMBEDDED-DOWNLOADER.md)).
    Exactly one is configured; a state directory is bound to the first one it runs with;
  - imports completed downloads into the configured `[library]` roots;
  - follows the download cleanup policy of the managed settings;
  - reads Torznab RSS feeds and refreshes metadata;
  - scans the library roots and refreshes Plex.

  Enabling execution requires a download client (`[qbittorrent]` or `[embedded]`),
  `[library]` and a search backend (native `[[indexer]]` entries or `[prowlarr]`), or the
  configuration is rejected. Keep it in an isolated sandbox until you have verified it.
- **Settings come from managed setup** (`arr setup` / `POST /api/v1/setup`): written as an
  immutable, digest-verified import under `imports/` (`source-setup.json`, an empty catalog)
  and activated by `active-import.json`. Managed setup is activated in the running process
  (an `Arc` swap under a setup lock; every operation uses one snapshot); it never replaces
  anything the catalog or active downloads use. A state folder whose active import came from
  the removed Sonarr/Radarr snapshot importer (`source-native.json`) is refused at startup and
  left untouched; its stored fields stay readable so a rolled-back older release still works.
- **Search backend: exactly one.** Native indexers (`[[indexer]]`: The Pirate Bay's JSON API, Knaben's JSON API,
  generic Torznab) or Prowlarr; configuring both is refused and nothing falls back.
- Outbound requests go only to base URLs from `config.toml`. API callers cannot supply a URL
  or a credential (managed setup carries policy only).
  - **Native indexers:** TPB `q.php` (raw query, `%20` spaces) and
    `precompiled/data_top100_recent.json`; Torznab `t=caps`/`t=search` with the key as
    `apikey` to its own origin only, and `.torrent` links on that origin or explicitly
    configured `download_origins` (no key there). Per-indexer gate: provider backoff, durable
    query limit, concurrency, spacing; redirects handled manually (one hop).
  - **Prowlarr:** `GET /api/v1/search`; `GET {id}/download` for a result's own link; and
    `GET {id}/api?t=search` (Torznab RSS) for RSS-enabled indexers. The key
    goes in `X-Api-Key`, and also in the Torznab query of a URL that is never logged.
  - **qBittorrent WebUI API v2:** WebAPI 2.8.2 (4.3.9) to 2.11+ (5.x).
  - **TVmaze/TMDB:** when enabled.
  - **Sport schedules:** ESPN's public scoreboard feed (`GET {base}/{sport}/{league}/scoreboard?dates=YYYYMM&limit=400`
    and `.../teams`), unauthenticated, with a plain `Arr` user agent, one request at a time.
    On by default: schedules are asked for only when a league is opened or followed, but the worker
    also reads the team list of every catalogue league that has teams (two a tick, after the
    followed schedules, each again after a week) so that team search works with no follows;
    `[sports] enabled = false` turns it all off.
  - **Plex:** a section or folder refresh (`?path=` from `movies_path`/`tv_path`); read-only
    `GET /library/sections`, paginated `/library/sections/{id}/all?includeGuids=1` and
    `/library/metadata/{key}/allLeaves` for discovery and the library bootstrap.
- **The runtime never contacts Sonarr or Radarr** and imports nothing from them. Titles they
  manage are unknown to Arr until added or read from Plex.
- The server listens on loopback by default. A non-loopback bind requires
  `server.allow_non_loopback = true`.
- `arr mcp` is a separate stdio process: an MCP server (rmcp, the official Rust
  SDK) that is a client of this REST API only, with no listener of its own. With `[mcp]
  enabled`, `serve` also mounts remote MCP at `/mcp` (rmcp `StreamableHttpService`, stateless
  JSON) behind Host/Origin/bearer/body/concurrency checks, calling the API router
  in-process ([`docs/MCP.md`](docs/MCP.md)).

## Module map

| module | role |
|---|---|
| `ids`, `title`, `quality`, `release` | typed media IDs; title folding/keys; qualities; the release parser (numbering, quality, native revision `version`/`real`, languages, flags) |
| `import`, `settings`, `catalog` | the versioned import store (immutable settings/catalog/report directories and the active pointer; the name is historical), normalised settings, catalog lookup index |
| `config`, `fsutil`, `paths` | runtime config validation; owner-only files; client-to-local path mapping, safe relative names, symlink-free checks, free space |
| `client`, `embedded` | the download client: qBittorrent or the embedded librqbit engine (own records, persisted lifetime counters, share limits, piece verification, confinement checks) behind one normalised view |
| `upstream`, `prowlarr`, `metadata`, `qbit`, `plex` | bounded HTTP with classified errors (`Retry-After`); Prowlarr search, download fetch and Torznab feed; TVmaze/TMDB (incl. TMDB find by IMDB id); qBittorrent client (cookie session, 4.x/5.x naming); Plex refresh, sections and paginated library enumeration |
| `indexer`, `tpb`, `knaben`, `torznab` | native indexers: the registry and provider contract (gate, health, mapping of imported ids, seed goals, `Backend` = native or Prowlarr); The Pirate Bay's JSON API; Knaben's JSON search API (an aggregate of public trackers); Torznab caps/search/RSS, the shared hardened RSS parser, credential confinement and guarded downloads |
| `setup`, `bootstrap` | managed setup documents normalised into `Settings` with an empty catalog, replacement guards, Plex mapping checks; the read-only, resumable Plex library bootstrap into owned, unmonitored catalog entries |
| `torrent`, `fixture` | magnet/`.torrent` info-hash identity; authored fixtures (ffmpeg media, `.torrent` with real piece hashes) |
| `library` | the managed catalog: insert-only seeding from imports, add/monitor/remove, episode upserts, wanted, calendar, history events |
| `trash` | the bin behind "Delete files": checked renames into `<root>/.arr-bin/<delete_id>/`, restore, the 7-day purge (the `bin` task), the `bin_entries` table |
| `target`, `decision`, `search`, `plan`, `store` | target resolution from the managed index; release decisions (including native revision upgrades, queued coverage, blocklist); search and ranking; plans; SQLite (schema 3) |
| `downloads` | the grab outbox, submission/reconciliation, polling, failure classification, user actions, completed/failed download cleanup |
| `ingest`, `probe` | completed-download import with a journal and crash recovery; the bounded ffprobe media probe |
| `scan` | per-root library reconciliation |
| `rss` | Torznab parsing, matching and RSS sync |
| `sport` | sport schedules, follows and release matching, a module of its own beside movies and series (`catalog`: the built-in leagues and their names; `espn`: the schedule client and the fixtures-only parser; `data`: the `sport_*` tables; `refresh`: keeping followed leagues current, and the worker's `sports` task; `release`: the release-name parser; `names`: team and fighter name rules; `matcher`: whether a release is a stored event; `find`: the search texts and the judging of an answer). The HTTP handlers are `api/sport.rs` |
| `worker` | background loop: commands (leased), downloads pass, scheduled tasks, request retries, metadata refresh, startup recovery |
| `api`, `openapi`, `error` | axum router, auth, limits, typed errors, OpenAPI document |
| `mcp` | the MCP adapter: six tools over the REST API (stdio over HTTP, or remote over the in-process router), closed input schemas, typed errors with their outcome, no fallback; the remote `/mcp` mount |

## Private state directory

```
STATE/                         0700
  config.toml                  0600  runtime settings (secrets referenced by path only)
  verification.toml            0600  optional, operator-written verification levels
  secrets/                     0700  api_token, mcp_token, indexer API keys, tmdb_credential,
                                     plex_token, prowlarr_api_key, qbittorrent_username/password
                                     (each 0600; any names, referenced by config paths)
  imports/imp-<digest16>-s1/   0700  immutable imports (source-native.json or source-setup.json,
                                     settings, catalog, report, manifest)
  active-import.json           0600  {import_id, manifest_sha256}
  state.sqlite (+ -wal, -shm)  0600  plans and keys, managed catalog, file evidence, grabs
                                     (outbox), import journal, blocklist, events, tasks,
                                     commands, roots, RSS state/seen items, requests,
                                     indexer_usage (query limits), bootstrap_runs (cursor)
```

Startup refuses wrong modes, symlinked private directories or a tampered active import.
Schema migrations are in place and transactional: 1 → 3 and 2 → 3 keep every plan and every
idempotency key. Secret values never appear in logs, API responses, settings, reports, the
schema or fixtures. Private link material (download URLs, magnets, payloads) is kept only in
`state.sqlite` and is never serialised.

## CLI

```
arr init      --state-dir STATE [--listen 127.0.0.1:8787]
arr setup     --config STATE/config.toml [--settings setup.toml] [--dry-run]
arr bootstrap --config STATE/config.toml [--media movies|tv|all] [--restart] [--dry-run] [--batch N]
arr check   --config STATE/config.toml
arr serve   --config STATE/config.toml
arr fixture --out DIR --name NAME [--file REL]... [--seconds N] [--ffmpeg PATH]
arr mcp     --api-url URL --token-file FILE [--timeout-secs N]
```

`init` writes the standalone template (native indexers, embedded engine, execution off).
`setup` validates the config without its
secret files (listing the missing ones), writes and activates managed settings, and reports
the ffprobe preflight; it never enables execution. `bootstrap` runs the Plex library bootstrap
to completion in bounded batches. `check` fails when execution is enabled and ffprobe cannot
run; `serve` refuses to start then.

`fixture` writes authored test releases (see the sandbox guide). `check` also reports whether
execution, the download client, library and Plex are configured. (`arr import`, which read a
Sonarr/Radarr snapshot, and `init --prowlarr-url` were removed.)

## Managed catalog

**Acquisition ownership.** Every catalog row has an acquisition state: `adopted`,
`legacy_managed`, `observed` or `released` (`catalog::acquisition_state`; the last three are
only seen in state written by older releases). Only adopted items are effectively monitored in
the index, so wanted, RSS matching and worker searches see only them.
`downloads::not_adopted` is checked in `enqueue`, the single acquisition funnel, and by the
explicit endpoints. Adding through `POST /library` adopts exactly the requested scope. This is
the `[migration] mode = "coexistence"` behaviour, now unconditional; the section stays
parseable (and is still written by the installers and the wizard, because the previous
release requires it with downloading on), `mode = "legacy"` is refused, and the old
`native_handoff_confirmed` / `preserve_monitoring` request fields are accepted and ignored.
The `handoffs` table and the other stored native columns are kept for rollback and are no
longer written.

Titles come from `POST /library` and the Plex bootstrap (`bootstrap_*`); user choices
(monitoring, removals) are never overwritten.

Every route resolves media through an in-memory index rebuilt from these tables after each
mutation.

Ownership is tri-state per movie and episode:

- **owned** when a present file is recorded;
- otherwise **missing** after a completed scan;
- otherwise **unknown**: only for rows an older release recorded without per-episode data.

Files are recorded with root-relative paths, size, quality, revision (only when the evidence
states it) and a probe result. A metadata refresh never changes ownership or monitoring.

`POST /library` adds a movie, a series, or a single season or episode, in one call, then
scans the item's folder, then searches (the default).

- **Scoped add.** A new series added for one season or episode monitors only that scope.
  Seasons that appear later are not monitored (`media.monitor_new = 0`). An existing
  series keeps its other choices.
- **Concurrency.** Library mutations (add, `PATCH`, `DELETE`) are serialised by one lock,
  so a release cannot fall between another request's check and its change.
- **Grabs and releases.** Every grab re-reads adoption from the stored
  rows (`library::not_adopted_now`) inside the SQLite transaction that records it. That
  covers manual, plan, request, RSS and worker grabs. The release and the grab transaction
  run on the same serialised connection, so one of them is first:
  - **Release first.** A release, or unmonitoring, committed while the torrent was fetched
    wins: nothing is recorded or submitted.
  - **Grab first.** The release is refused as an active download.
- **Movie availability** follows native `minimumAvailability = released`: the earliest
  physical/digital release, else in-cinemas + 90 days, plus `availabilityDelay`. Release
  evidence comes from Plex, TMDB, or the operator, and is never invented.
  Without a date the movie is not available. Other
  modes are reported as not implemented. Availability is evaluated against the clock, so a
  dated movie becomes available without any refresh; TMDB refreshes update the evidence.

## Decisions and revision upgrades

A release is approved only when **no** rejection applies, and every applicable rejection is
reported (`RejectionCode` schema). Codes added in this phase:

- `covers_queued`: the release overlaps a download in progress.
- `blocklisted`: by release ref or info-hash, per item.

**Revision upgrades** (`downloadPropersAndRepacks = preferAndUpgrade`): an owned file
may be replaced only by a release of exactly the same quality with a higher known revision
(`real` first, then `version`; PROPER/REPACK/RERIP/`vN`), even with profile upgrades off. The
decision fails closed when any of these is unknown:

- the old quality, the old revision or the managed path;
- the release quality, when it is only inferred.

A different quality, including a same-group proper such as WEBRip over WEB-DL, is not a
revision upgrade. Ordinary quality upgrades are not implemented.

Revision candidates come from RSS and from explicit revision searches
(`POST /plans {revisions: true}`). Import re-checks the rule against the current evidence.
Ranking is deterministic: approved, swarm health band, tier, revision, coverage, certainty,
seeders (more first), leecher-to-seeder ratio (lower first), indexer priority, size, ref.
The band (`decision.health`) is `healthy` (50+ seeders; 10+ with leechers at most 10x the
seeders; or 5+ with leechers at most equal), `ok` (3+ seeders) or `weak` (fewer, or unknown),
so a healthy 720p beats a weak 1080p, and indexer priority never beats a bigger swarm.

## Plans and execution

Plan states: `shadow_planned`, `shadow_partial`, `blocked`, `already_satisfied`,
`in_progress` (everything wanted is owned or downloading), `cancelled`, `superseded`,
`download_queued` (executed, in the outbox; `execution.submitted` is false until
qBittorrent confirms), `download_started` (confirmed), `download_failed` (every grab ended
before confirmation) and `imported`. A movie that is not available is blocked
(`not_available`) without searching.

- Plan building is unchanged, except that units covered by active downloads are `queued`.
  With qBittorrent, season packs are not eligible while any episode is queued or owned (unless
  it is a revision search). With the embedded engine a full-season pack may leave out owned
  episodes and ones whose download is making progress, and takes over idle, unconfirmed or
  unsent single-episode downloads (`decision.skips_owned`, `skips_active`, `supersedes`): the
  grab covers only the episodes it gets, and the engine fetches only their files. The
  replaced downloads are cancelled in the transaction that records the pack's grab, without a
  blocklist entry, and their torrents and incomplete data are removed through the journaled
  cleanup. A pack on a weak swarm replaces nothing. The plan lists the replaced grabs in
  `execution.superseded_grabs`.
- Executing a plan (`execute: true`, or `POST /plans/{id}/execute`) sends every planned
  candidate through the outbox. The plan records `execution.grabs`/`refused` and
  `execution.outcome` (`queued`, `partially_queued` or `nothing_queued`). It then moves to
  `download_queued`, `download_started` and finally `imported` once all its grabs are
  imported. A plan that queued nothing keeps its shadow state, so it can be executed again.
  After a partial or total refusal, the explicit request stays active for retry while a
  refused planned unit is covered by no active grab.
- Plan executions and worker searches are **automatic** grabs. `POST /grabs` with a
  `release_ref` the user chose is **interactive**. This matters for the
  `autoRedownloadFailed` and `autoRedownloadFailedFromInteractiveSearch` policies.
- Explicit requests whose plan left wanted units unplanned are remembered and retried with
  doubling backoff (default first retry 60 min, at most daily, expiring after 14 days) until
  satisfied. Searches the worker runs always bypass the search cache.

## Downloads

Outbox states: `queued` → `unconfirmed` → `downloading` → `completed` → `importing` → `imported`,
plus `import_blocked`, `failed`, `cancelled` and `released`. A grab is active (it covers its
targets) until it is imported, failed, cancelled or released.

- **Identity:** the payload comes only from a literal magnet in the search result (native TPB
  results are magnets built from their info-hash, with only configured public trackers), from
  the native indexer that produced the release (its own origin or configured download
  origins, never its key elsewhere), or from the configured Prowlarr's download proxy. With the
  embedded engine, a payload that could find no peer (no usable tracker, DHT off or a private
  torrent, no initial peers) is refused (`no_peer_discovery`), never queued. A native
  indexer's grab limit is checked before the payload is fetched. Link fields are classified by content, not by
  name: Prowlarr's search API can put the HTTP proxy in `magnetUrl` with no `downloadUrl`.
  A valid magnet (from `magnetUrl`, else `downloadUrl`) is used without a fetch. Otherwise
  each HTTP link is fetched, `downloadUrl` first, through the proxy rules: same origin,
  proxy path, no URL credentials, bounded size, at most one same-origin redirect or a
  redirect to a valid magnet, and a validated `.torrent`. An unusable field never hides a
  usable one, and nothing is rebuilt from the advertised hash. The info-hash is computed (magnet `btih`/`btmh`, or SHA-1 of the
  bencoded `info`), checked against the search result, and stored before submission.
- **Submission** (WebAPI 2.8.2+): the add carries the client policy:
  - the imported client category (or a configured override) and the tag `unified-media`;
  - the configured save path with `autoTMM=false`;
  - the imported initial state (start, force start or paused), sequential and first/last
    piece options, and content layout;
  - the indexer seed ratio and seed time as `ratioLimit`/`seedingTimeLimit`.

  The grab is marked `unconfirmed` before the call. Every later attempt looks the hash up
  first, so a timeout or crash never produces a duplicate.
- **Identity:** before anything is queued, the `.torrent`'s own name and video file paths
  (or a magnet's display name) must not contradict the item (`identity` module, with the
  same title/alias/year keys as decisions, scans and RSS, `catalog::title_keys`, including
  franchise-prefixed alias forms the item's own metadata implies). A contradiction blocklists the release
  (`wrong_payload`), and the acquisition re-plans a valid alternative (bounded). A torrent with the same hash but
  another category, or without the tag, is foreign: never touched, and the grab fails.
- **Polling** maps 4.x/5.x states. Errors and missing files fail a download after 30 min.
  A torrent absent for 10 min is treated as removed. A changed category or tag releases the
  grab. A client outage changes nothing. Completion queues the import.
- **Stalls.** No progress while downloading fails a download after `stall_timeout_minutes`
  (default 12 h).
  - **What counts as downloading:** any downloading label. That includes `downloading` with
    peers connected that deliver nothing, qBittorrent's `forcedDL` (labelled so whatever
    its speed), stalled, and fetching metadata.
  - **What counts as progress:** the client's progress only; speed alone is not progress.
    The embedded engine counts progress in whole verified pieces, so a download must
    complete at least one piece within the timeout.
  - **What resets the clock:** progress, a user pause, and the client queueing, checking,
    moving or completing it.
  - **What is not counted:** time without a client answer. That covers an outage, or this
    process not running (for the embedded engine, the engine was not running either).
    The next answered poll moves running clocks forward by the time since the last answer.
- **Failure:** the download becomes inactive and is blocklisted (except removals and
  outages). With `autoRedownloadFailed` (or its interactive variant), a `search` command is
  queued. An incomplete torrent is stopped.
- **User actions:** pause/resume (our torrent only); cancel (keep by default; remove the
  torrent; or remove torrent and data); retry (re-import, resubmit, re-evaluate cleanup, or
  search for an alternative). After a cancel, automatic searches of its targets wait 24 h.

## Completed-download import

For each completed grab, and only from its own torrent:

0. Reconcile the destination. The item's folder is scanned right before planning, so a file
   another application (such as the native app) put there since the last scan is owned.
   It is never duplicated: covered targets are skipped, or the grab waits as
   `import_blocked`.
1. Map the reported save path into `local_path`.
2. Select video files with plain relative names. Skip extras and sample folders, sample
   names, unwanted and incomplete files, and containers ffprobe cannot read.
3. Map each file to the grab's exact targets. Multi-episode files own all of their
   episodes; two files claiming one episode block the import. The torrent, folder and file
   names must not contradict the item. A contradiction fails the grab (`wrong_payload`):
   it is blocklisted and replaced automatically, never imported. Explicit language markers
   in those names (file first, then download name) must satisfy the profile's language
   rule, or the grab fails (`wrong_language`, replaced). Unmarked names rely on the
   approved release; stream languages are not guessed.
4. Probe with ffprobe and check the native sample durations. The **effective quality** is
   the source the payload names (file, then download name; else the advertised one) at the
   measured resolution (native MediaInfo thresholds, width or height). It must be allowed by the profile, and must equal the owned quality for a
   revision upgrade; otherwise the grab fails (`wrong_quality`) and is replaced. It is
   what gets recorded.
5. Hardlink (same filesystem and `copyUsingHardlinks`) or copy into staging; a copy is
   probed again.
6. Check free space against `minimumFreeSpaceWhenImporting`.
7. Publish without clobbering, then commit to the database.

Destinations are `Root/Folder/` or `Root/Series/Season N/`. With rename off, file names follow
native rules: the download title plus the file's extension for single-file and
single-episode downloads, and the original file names for season packs.

Each file has an `import_journal` row:
`planned → staged → [backed_up →] published → committed [→ backup_removed]`. At startup,
uncommitted rows are rolled back: the staged or published inode is removed only if it is
still ours, and a moved-aside original is restored. A revision upgrade moves only the old
managed file aside, and deletes it after the commit. Unplayable media fails the grab
(`invalid_media`, blocklisted) and is never owned.

## Cleanup

This follows native `removeCompletedDownloads`/`removeFailedDownloads`, captured per grab.
`[execution] download_cleanup = false` switches it off.

- After a committed import, the download is tracked as `seeding`.
- It is removed with its data only when it is ours (category, tag, hash), still at its
  recorded save path, has no symlinked component, its library files are committed and
  present, qBittorrent has stopped it, the applicable share limit is met (evaluated
  explicitly) and the indexer seed goal is met.
- The applicable limit is the per-torrent one, or the global one when the torrent uses `-2`.
  With no limit, or unknown fields, the source is kept.
- The removal is journaled (`removing`, with the file list, sizes and inodes) before
  qBittorrent is asked (`deleteFiles=true`). It is finished after a crash, and our own
  deletion touches only unchanged journaled files.
- Failed downloads with nothing imported are removed when the policy asks; complete ones need
  the same seed criteria.

## RSS sync

Every indexer with `enableRss` in the settings is read every `rssSyncInterval`, unless
`[automation] rss_interval_minutes` overrides it (0 disables RSS), through the configured
backend: a native indexer's own feed (TPB's recent top-100, filtered to the requested
categories, with the gap check over the whole shared window; Torznab `t=search` without a
query) or Prowlarr's per-indexer Torznab proxy. Each read is bounded:

- XML is parsed without DTDs or entity expansion, with node and item limits;
- links must be magnets or on the backend's own (or configured) origins, and never carry a
  native indexer's key to another origin.

Items are matched to unambiguous managed, monitored items by title or alias key, year and
numbering, and every episode they cover must be monitored. When a season's own flag is
unmonitored (an episode-scoped add), only single- or multi-episode releases that name
individually monitored episodes match. Packs for that season, and its other episodes, do
not. Matched items go through the normal decisions,
ranked best first per target, at most 20 grabs per sync, and through the outbox.

State is kept per item and per indexer:

- `rss_seen` records each item's status; grabbed items are never grabbed again.
- Per-indexer state advances only after a successful read. Failures back off. A window that
  does not reach back to the newest release the last successful sync saw (its oldest dated
  item is newer, or nothing in it is dated) is flagged `gap_possible`, whatever its size: a
  feed can return fewer rows than its nominal limit and still have moved past the last sync
  (TPB's recent feed answered 50 rows, not 100). The dates span the whole window, before
  category filtering. Overlap shows only that the window is contiguous with the last one,
  not that the provider lists every release; the first sync has no baseline.

Normal monitoring is RSS-based, like native. **No task searches the missing backlog on its
own.** `search_missing` runs only on request, in bounded batches per pass.

## Library scan

Every `rescan_hours`, or on request, each root is walked (symlinks are never followed). The
scan changes nothing on a root that is:

- unavailable, unreadable or incompletely walked;
- empty while owned items are recorded there (native also skips an empty root folder);
- for a scheduled scan, about to retire more than half of its recorded files (≥10)
  (`suspect_mass_missing`).

On a healthy root, files found are owned immediately, except files whose own names name
another title. Quality and revision are taken only from explicit file-name evidence, and
the quality is corrected to the measured resolution once probed. Recorded files that are gone are retired, and the items
are marked scanned. New or changed files are probed lazily (`probes_per_scan`); a failed
probe is reported but never drops ownership.

## Worker

The worker is one loop per process. It wakes on a notification or every `poll_seconds`. Each
pass:

1. runs up to 4 queued commands. Commands are leased, and a batched command continues in
   later passes, so user requests never starve;
2. runs the downloads pass: submit, poll, import, cleanup, plan states;
3. runs due tasks: `rss_sync`, `requests`, `refresh_metadata` (fresh TVmaze episode
   lists and TMDB movie release evidence, 25 items per run each), `rescan`, `prune`, `bin`,
   and last `sports` (every 15 minutes it looks at which followed sport leagues are due; it
   never fails the pass and stops early when the schedule source is failing).

Task status is in `tasks`. At startup the worker rolls back the import journal, returns
`importing` grabs to `completed`, reconciles `unconfirmed` adds first, and requeues commands
leased by a dead instance (at most 3 attempts).

## Identity

```
movie:tmdb:<id>
series:tvdb:<id>            series:tvmaze:<id>   (when TVmaze knows no TVDB id)
season:<tvdb|tvmaze>:<id>:<season>
episode:<tvdb|tvmaze>:<id>:<season>:<episode>
```

IDs are strict: digits only, no leading zeros, season ≥ 1. Specials (season 0) are not
supported. Entries that share a canonical ID are kept and marked `identity_conflict`, and
are blocked.

## Search and completeness

Every search and plan separates what was observed (`outcome`) from how complete the
observation is:

- `completeness.retrieval`: `complete`, `incomplete` (with issues) or `unknown` (legacy).
- `completeness.provider`: `unverified` with Prowlarr (its search reports no per-indexer
  status). With native indexers each query runs once per indexer (with that indexer's own
  categories), each view names its indexer, and `complete` means every indexer the settings
  name answered in full — an explicit "no results" counts, an empty answer without it is
  `empty_unconfirmed`, an unusable mapping is `indexer_unavailable` — else `incomplete`.

RSS reports retrieval status separately and never promises gap-free coverage.

## Sport

Sport is a module (`src/sport/`) and a set of tables (`sport_follows`, `sport_events`,
`sport_teams`, `sport_refresh`, added with `CREATE TABLE IF NOT EXISTS` inside schema 3, so an
older release ignores them) that sits beside movies and series. It is **not** a third media kind:
no `MediaId` variant, no download, no import and no Plex call exists for sport yet. A search for the releases of one event exists, and only reads.

What exists (phase one: schedules and follows):

- a built-in catalogue of leagues (soccer, basketball, rugby union and league, golf, MMA), each
  with its schedule-source path checked live, whether it has teams, and the names release titles
  use for it (read by the release matcher);
- following a league, or a team of a league that has teams, from now on; follows and events
  survive restarts and are available in shadow mode (the immediate read on follow works
  without the worker);
- events with a fixed state vocabulary (`scheduled`, `postponed`, `cancelled`, `played`; plus
  an internal `unlisted` for an event that dropped out of a complete answer, never served:
  absence never cancels, only the source can). The
  schedule parser has no field for scores, winners or result text, so none can be stored or
  served; a test keeps real recordings that carry them and proves none gets through;
- a worker task and on-demand reads that keep followed leagues current within a strict request
  budget (see the Sport section of [`docs/API.md`](docs/API.md)), and the `/api/v1/sport/*`
  routes, plus a `sport` field on `/feed` and `/calendar` and `sport_enabled` on `/feed`. After the
  followed leagues, and only if they all answered, the worker's task reads the team lists of at most
  two catalogue leagues per run (`read_catalogue_teams`: never read first, a list younger than seven
  days or in its failure wait is skipped, one failure ends the run's list reads, a league being
  read already is skipped).

### No results

Sport never keeps or shows how a contest went. The contract, which `src/sport/espn.rs`
follows:

- **Structural.** Score, winner, status-text and record fields are never parsed: no struct has a
  place for them (the status is read only as the name and state that become `scheduled`,
  `postponed`, `cancelled` or `played`). Athletes' names are not parsed at all.
- **Positive rules.** Every string taken from the schedule source is accepted by a positive rule
  for its kind, never by "anything except a denylist": ids are digits; times are parsed;
  a season is `YYYY`, `YYYY-YY` or `YYYY-YYYY`; a round is exactly one of a list of labels
  ("Round 12", "Group B", "Semi-final", ...) rebuilt by Arr; an abbreviation is up to six
  letters, digits or `&`; a team, short or venue name is letters, digits, spaces and
  `. ' & - / ( )` within a length bound, and every segment of it (a run of letters and digits; spaces, `. - / ( ) & '` are separators,
  so "Smith.outclasses" is two segments) begins with a capital, a digit or a letter of a script
  without case, or is a small particle ("de", "of", "des", "upon", "sur", ...), an apostrophe form
  ("d'Ivoire") or a possessive `s` ("Children's"), or the whole word is a sponsor web address
  such as "crypto.com"; so lowercase prose is not a name, whatever it is prefixed or joined with.
  Tournament titles and fighters' names are judged by the same segment rule; a game between two teams is named by Arr from the two
  team names (`<Away> at <Home>` for basketball, `<Home> v <Away>` for soccer and rugby), its short name from the abbreviations; a fight card's title
  begins with one of the league's own names and has a small tail and optionally
  `: <Name> vs. <Name>`; a tournament title is capitalised words, numbers and a few small
  connectors. A string that does not fit becomes a neutral value made from Arr's own data
  (`Team <id>`, `<League> event on <date>`) or null. The event or team itself is never hidden
  because of it. The one excuse in the reject list is for a team object's name (`team_name`: team
  lists and fixture sides, never free text): it may end in a word from a short compiled list of club
  words (`CLUB_WORDS`, now "Victory", for "Melbourne Victory"), with everything before it passing the
  whole rule. A team list that holds a `Team <id>` name is read again after a day instead of a week.
- **One label per team.** A team's name lives in `sport_teams` and a successful team-list read
  corrects it. Follows, the league page and search take the label from there at read time (a
  follow keeps the label it was made with only as a fallback, then `Team <id>`; `since` never
  changes). An event row keeps the home and away names, name and short name of its last
  refresh, as one consistent snapshot; they are not re-resolved at read time.
- **Reject list.** On top of each positive rule, text that reads like a score (`3-1`) or contains
  an outcome word as a whole word (def, beat, wins, winner, won, lost to, draw, ko, tko,
  submits, taps, stops, decision, forfeit, champion, upset, ...; "Championship" and
  "Decker" are not outcome words) is refused.
- **Known limit.** A positive rule for names cannot prove that a proper noun is not a result
  written in title case ("Smith Overcomes Jones" is made of capitalised words); the reject list is
  what stands behind that case, and a result phrased without any listed word would pass it.

### Release matching

`GET /api/v1/sport/event/{id}/copies` looks for the releases of one stored event and lists the
copies Arr found, for a person to look over and choose from. It reads: no link, grab or file is recorded, and nothing is sent to a
download client. A search for a movie or an episode and this one share `search::fetch` (the fan-out
over the indexers, behaviour-preserving: `search::run` calls it and keeps its own decision tail);
sport asks the indexers of the shows (`MediaKind::Series`), because the releases are filed under
TV. An event is only searched once it is over by Arr's reckoning (`find::expected_end`: the
source's end, or the start plus a length typical for the sport; an MMA start is the start of the
whole event), and a cancelled or postponed one never.

The matcher (`sport::matcher::judge`) is pure and strict: a release matches only on evidence its
own title carries, and a wrong file under an event is the failure it exists to avoid.

- **The competition** is named by the catalogue's names, as whole words at the start of the title
  (after at most a broadcaster or a year). Anything else before it (a division number, "Women's",
  another sport's name) makes it a different competition; two competitions named make it
  ambiguous; none named refuses it. Women's and youth ("U21", "Reserves") releases are refused
  for every league but the women's one.
- **A game** needs the date in the title within one day of the event's UTC day (day first, never
  month first: `01.19.2019` is not a date), and both teams. Each side must name exactly one team
  of the league (generic words like "FC" aside; a word two clubs share, "Manchester", names
  neither), and the two must be the event's home and away in either order. A round or season in
  the title that contradicts the event's refuses it.
- **A fight card** needs the card's number (a numbered card and a Fight Night are different
  events) or both headline surnames, and a date in the title near the event or a publish time
  between a day before the start and 14 days after. The part (main card, prelims, early prelims,
  full event) is read from the title. A UFC file with no part word is taken as the main card but
  only `likely` (one uploader and two cards are all the evidence): beside a prelims file of the
  same uploader it is sure, beside an explicit "Main Card" file it is the whole event. A rematch
  numeral ("Walker 2") that the schedule does not state is `likely`, unless the store holds an
  earlier meeting of the pair. With neither number nor names, only a date in the title and no
  other card within a day, or the card's place ("PFL Tampa"), give a `likely`.
- **A golf tournament** needs its name (all of the title's name words are the event's) and a day
  inside it (the source's end date is early by an evening, so the last day is allowed), and `Day N`
  must agree with the date.
- **The contract of phases C and D: no recommendation, no assurance.** Six rounds did not make
  a `sure` that automation can trust, and nothing automatic exists yet. The matcher keeps its
  internal `sure` / `likely` and every reason, exactly as below (the automatic phase will need
  them and gets its own review), but the API and the app do not say "sure" or "recommended":
  each copy has `caution: null` or `caution: {reason, note}`, the list is only ordered, and
  nothing may act on confidence except to order the list and to decide whether a caution is
  shown and a confirmation asked. No caution is not a verification; a person chooses.
- **`sure` is earned from closed evidence only.** Every word of a title (and of its `[..]`,
  `(..)` and `{..}` groups, after HTML entities and invisible characters are resolved) has a
  role (`SportRelease::roles`). A word is *verified* when a closed vocabulary or the event itself
  checks it: the competition's name; a broadcaster or a year right after it; a round, stage or
  filler word, and a number only after the word that says what it is ("Round 3") or as a card's
  number; a part word; a team or fighter (resolved against the league or the card); a place or
  tournament word (checked against the event); a format word (a resolution, source, codec,
  container, frame rate, and a number only after a codec or audio word); in a `[..]` group a sport
  label, a language, and a channel from a closed list (after "DAZN", a region from a closed list;
  a short code is a team's feed and is verified only if it is the code of one of the event's
  own two teams); and one uploader's tag: the last word of the title (after the last hyphen, or
  the last chunk), spelled exactly as one of a compiled list of known sport uploaders
  (`KNOWN_UPLOADERS`, case-sensitive, taken from real titles), once the technical words have come,
  once across the main text and the brackets together. **A tag that is not on the list is free
  text**, however plain it looks. Shoulder markers are looked for across token boundaries too (up
  to three neighbouring words joined: "Round-UP", "Wrap.Up", "Post Fight"). A word that
  is none of these is either refused (a word in the body that nothing accounts for) or *free*
  (text in the tail or a group that no list covers). **A match with any free word, any
  given name before a surname, or a feed code that is not a code of the event's teams, is at
  most `likely` (`unverified_text`), and is never
  recommended.** The shoulder words (countdowns, embedded series, weigh-ins, previews,
  highlights and their names in Spanish, Portuguese, French, German, Italian and Dutch, press
  conferences, post-fight shows, studio shows) are refused outright, as a whole word and as a
  substring of a longer word ("ExtendedHighlights", "Highlights2026"). Video games ("2K27",
  updates, DLC), episode numbering ("S02E03") and anime are refused. Every date a title carries
  must be near the event, not just one. An insertion test puts a hostile word (a summary, a
  result, a score, a hash, a single fight, a random word) at every boundary of every `sure` title
  of the table, glued and spaced, in every bracket form, after a bar and in or beside the tag,
  and none stays `sure`.
- **A bout is not the card.** "Main Event", "Co-Main", "Full Fight", "Fight 3", a bare "Main" and
  "Round 2" name one fight or part of one; only "Main Card", "Prelims", "Early Prelims" and "Full
  Event" (or "Main Card And Prelims") name a part of the card. A title naming two fighters who
  are not the headliners is refused.
- **One release's evidence is never lent to another.** A UFC file with no part word is the main
  card at `likely`; a sibling (same uploader, same card) settles that and only that, and only if
  the sibling is itself `sure`. A file that is also unsure which event it is (a date only, a rematch
  numeral, another fixture that fits, a schedule not known) stays as it was. Any other card with
  no part word (PFL, today) is `likely` as "unlabelled whole": nothing says whether it is the
  whole event, the main card or one fight, so it answers no part request and is never
  recommended.
- **Every identity claim is checked against the stored event** (`matcher/claims.rs`, one
  function, `check`, for all three shapes). A title claims a card number, a series serial
  ("Fight Night 999"), each side's names, head and place words, dates, a season, a rematch
  numeral, a round, a part. Each claim is `Verified` (the store says the same), `Unclaimed`
  (the title does not make it), `Unverifiable` (the store cannot check it) or `Contradicted`
  (the store says otherwise). A contradicted claim refuses the release; an unverifiable one
  caps the match at `likely` with its own reason: `number_unverified`, `serial_unverified`,
  `names_unverified` (the stored card has no headliners), `head_unverified`, `sides_unverified`
  (a stored game with no teams), `season_unverified`, `round_unverified`, `rematch_unverified`,
  and the part (`unlabelled_main`, `unlabelled_whole`). `sure` needs every claim verified or
  unclaimed. The schedule source gives no serial for a Fight Night, so a release that carries one
  is never `sure`; a numbered card ("UFC 334") and a title with no serial can be. A card or
  tournament of the same days whose name or headliners are unknown could be the one the title
  names, so a names-only match is `likely` (`schedule_unknown`) while one is in the window.
- **There is no filler in a `sure` title.** A `sure` title is the league's alias; the event's
  identity in the forms its shape allows, each verified; the connectives between the sides; the
  part; technical words from closed lists (resolution, source, codec, audio, container, frame
  rate, language, a channel); and one known uploader's tag. Every other word is an *assertion*,
  an unverified word, or a refusal. Stage and phase words (final, semi-final, play-offs,
  preseason, regular season, RS, group stage, leg, game N, match N, matchweek N, matchday N,
  round N, day N, week N), years and seasons (`Season 2024`, a lone year, a span) become
  `Assertion`s (`SportRelease::assertions`), and each word of one has `Role::Claim(i)`. A number
  no claim consumes is unverified. What is left of the old `FILLERS` list is `CONNECTIVES`
  (and, of, the, vs, v), which join words and assert nothing; "men" and "mens" set a flag the
  matcher checks against the league, and the old lead words only decide where the names begin.
  Every assertion is kept, not the first: equal ones are one, different ones of a kind refuse
  (two rounds, two seasons, two dates, two numbers, two rematch numerals, two parts).
  Verification means equality with the stored value (a round of another kind than the stored
  one, a lone year inside a stored span, a numeral when the stored title has no ordinal are
  incomparable and stay unverified); "an earlier meeting exists" never verifies a numeral. A
  claim on a shape that does not support it (a season on a card, a part on a game, a numeral
  after a team) is checked, capped or refused, never dropped. `matcher/claims.rs` reads every
  assertion (`claim_of` is exhaustive), and a test asserts every word of every table row is in
  exactly one bucket or attached to one assertion.
- **Round 6 additions to the claims.** A sport label in a `[..]` ("Soccer", "Football", "Rugby",
  "Basketball", "Golf", "MMA") is a claim compared with the event's sport (football is the soccer
  label; rugby is union or league; a conflicting label refuses). A series or card number on a
  game or a tournament refuses. Explicit name suffixes are compared before they are dropped
  ("Jr" is not "Sr"; one side only is unverified). A major championship is an identity, equal to
  the stored one ("The Open" is not "U.S. Open"), not a word contained in it. A title with no
  uploader's tag carries the `no_uploader_tag` caution (forum-style titles have none by nature).
  A qualifier word ("Junior", "Reserves") refuses only if it is not part of a participant's
  stored name; a number after the competition's name is a division unless a stored participant's
  name starts with it ("1. FC Union Berlin"); a numeral after the first name belongs to that
  participant, not to the card number. A league's gender is recorded (LPGA and WNBA are women's)
  and both "Men" and "Women" are applied against it.
- **Phrases are read over the title's words in their visible order,** across brackets,
  parentheses, braces, bars and separators, before any word has a role: "Main [Event]",
  "Round | Up". Part words that name different parts anywhere in the title conflict.
- **A fixture is not judged alone.** If the same two teams (or fighters, or a tournament of the
  same name) have another fixture that the title would fit as well, such as the next night of a
  series, the match is `likely` (`another_fixture_fits`) for each of them.
- **What each judgement does when what it depends on is not known.** Completeness is a
  recorded fact, not a guess from how long ago something was read:

  | depends on | recorded as | if not |
  |---|---|---|
  | the league's teams (resolving "Wanderers" or "Manchester") | `sport_teams_read`: the last read of the list dropped no entry (one without an id), renamed none (a name that failed the name rule), and was not cut at the limit; and it is under 21 days old | only the exact names of known teams are taken; no partial name |
  | the other fixtures around the title's dates ("no other fixture fits", "no other card within a day", rival tournaments) | `sport_ranges`: the stretches a schedule read judged in full (a month cut at the page limit, empty, or with a record that could not be read is not one), current while read within 3 days or after its own days were over; and the neighbours loaded were not cut at their limit | a game, or a card or tournament matched on names, is `likely` (`schedule_unknown`); a card with only a date is refused; a card with its number is unaffected |
  | an earlier meeting of the fighters (a rematch numeral) | stored events since a day before the follow | the numeral is `likely` (`rematch_unverified`) |
  | the part of a card | a part word, or a sure sibling | `likely`; and a whole-event match whose part the title does not say answers no request for a part |
  | when | the publish time, whatever the title says (a date or a card's number says which event, not when it was put up) | a release is put up from a day before the event to 14 days after its start (a tournament: after its last day); a later one is `likely` (`late_upload`), and no publish time is `likely` (`undated`), for every sport. Without a date or a number to say which event, either refuses it |

  The neighbours are loaded over the window the evidence allows: for a game the fixtures of the
  same two teams within three days; for a card or a tournament every event of the league within
  three weeks, and a list cut at 500 is incomplete. A fixture the league listed but Arr did not
  keep (it was over before the league was followed: no backlog) is kept as *evidence only*
  (`sport_evidence`): it is never served, and the loaders return it beside the kept ones, so a
  recorded stretch means what it says. An event whose two sides are unknown (the feed put one
  team on both sides) is returned by the pair loader too, and a title that fits its days is
  `likely` (`schedule_unknown`), since nothing rules it out.
  Evidence is kept for as long as any recorded span covers it (it is pruned only after the
  span itself has expired), and no clean-up deletes a stored event or team: the only deletes are
  an unfollow (the follow row), expired spans, and evidence no span covers. Judging
  of a search runs off the async workers (`spawn_blocking`), four requests at a time, with the
  wait for a slot, the setup and the judging inside one two-second budget; a request that
  finds no slot answers busy.
- **Bounds.** A title over 400 bytes, over 120 words or with more than 8 groups or 32 competition
  names is refused unread (`title_too_long`). A search judges at most 300 results, within the
  request's budget (a fifth of it, at most a second, is kept for judging), and says
  `complete: false` if there were more. The event's side of every comparison (team keys, its name
  as a title, its neighbours' names) is worked out once per search, not per result. The indexers'
  own answers are capped separately (a megabyte).
- **How often the indexers are asked** (`sport::gate`). What is kept is the *search* (the
  releases found), never a judgement of it: every answer judges it again against the event,
  its neighbours, the team data and the recorded completeness as they are then. One search per
  event at a time (requests for it queue and share its answer); never again within two minutes
  whatever `fresh` says, nor within two minutes of a search that did not finish or was dropped
  half way (its start is recorded before it is awaited; the answer is then the last search with
  its age, or "busy" if there is none); a complete answer is reused for fifteen minutes by a plain request; and at most six
  searches start in a minute for all events together, after which an event with no answer yet is
  told to come back (`busy`) and one with an answer gets it. A search is at most three queries per
  indexer (`find::MAX_QUERIES`), so this route sends at most 18 queries a minute to any indexer,
  each at the indexer's own paging and through its own gate (spacing, backoff, per-indexer limits).
  At most 256 events are remembered; an unknown event is a 404 before any of this.
- **No text a release wrote reaches a client.** A copy is numbers, times and words from closed
  lists (the part, the resolution and source, the language, the reason); not the title, not
  the uploader's tag, not the channel. The neutral `label` is built from the closed words.

What does not exist yet: downloading, filing and Plex refresh for sport, automatic pickup, and a
stored history older than yesterday.

## Not implemented

- quality/cutoff upgrades, renaming, recycle bins, extra files and custom formats: they block
  plans or file import (fail closed);
- usenet, tag-scoped routing, and anime, daily and scene numbering;
- Cardigann definitions other than `thepiratebay` (keep them behind a Torznab endpoint);
- OAuth for remote MCP (static bearer token only);
- specials;
- multi-file movie imports (more than one non-sample video blocks the import);
- partial replacement of multi-episode files (fail closed).

