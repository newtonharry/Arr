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
- **Search backend: exactly one.** Native indexers (`[[indexer]]`: The Pirate Bay's JSON API,
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
| `indexer`, `tpb`, `torznab` | native indexers: the registry and provider contract (gate, health, mapping of imported ids, seed goals, `Backend` = native or Prowlarr); The Pirate Bay's JSON API; Torznab caps/search/RSS, the shared hardened RSS parser, credential confinement and guarded downloads |
| `setup`, `bootstrap` | managed setup documents normalised into `Settings` with an empty catalog, replacement guards, Plex mapping checks; the read-only, resumable Plex library bootstrap into owned, unmonitored catalog entries |
| `torrent`, `fixture` | magnet/`.torrent` info-hash identity; authored fixtures (ffmpeg media, `.torrent` with real piece hashes) |
| `library` | the managed catalog: insert-only seeding from imports, add/monitor/remove, episode upserts, wanted, calendar, history events |
| `target`, `decision`, `search`, `plan`, `store` | target resolution from the managed index; release decisions (including native revision upgrades, queued coverage, blocklist); search and ranking; plans; SQLite (schema 3) |
| `downloads` | the grab outbox, submission/reconciliation, polling, failure classification, user actions, completed/failed download cleanup |
| `ingest`, `probe` | completed-download import with a journal and crash recovery; the bounded ffprobe media probe |
| `scan` | per-root library reconciliation |
| `rss` | Torznab parsing, matching and RSS sync |
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
Ranking is unchanged and deterministic: approved, tier, revision, coverage, certainty,
indexer priority, seeders, size, ref.

## Plans and execution

Plan states: `shadow_planned`, `shadow_partial`, `blocked`, `already_satisfied`,
`in_progress` (everything wanted is owned or downloading), `cancelled`, `superseded`,
`download_queued` (executed, in the outbox; `execution.submitted` is false until
qBittorrent confirms), `download_started` (confirmed), `download_failed` (every grab ended
before confirmation) and `imported`. A movie that is not available is blocked
(`not_available`) without searching.

- Plan building is unchanged, except that units covered by active downloads are `queued`, and
  season packs are not eligible while any episode is queued or owned (unless it is a revision
  search).
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
   lists and TMDB movie release evidence, 25 items per run each), `rescan`, `prune`.

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

## Not implemented

- quality/cutoff upgrades, renaming, recycle bins, extra files and custom formats: they block
  plans or file import (fail closed);
- usenet, tag-scoped routing, and anime, daily and scene numbering;
- Cardigann definitions other than `thepiratebay` (keep them behind a Torznab endpoint);
- OAuth for remote MCP (static bearer token only);
- specials;
- multi-file movie imports (more than one non-sample video blocks the import);
- partial replacement of multi-episode files (fail closed).

