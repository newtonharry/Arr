# API v1

Base URL: `http://127.0.0.1:8787`. The schema is at `GET /api/v1/openapi.json` (OpenAPI 3.1,
no auth). The schema's enumerations come from the same constants the code uses, and a test
checks that every route is documented.

## Conventions

- **Auth.** Every route except `/api/v1/health` and `/api/v1/openapi.json` needs
  `Authorization: Bearer <STATE/secrets/api_token>`. A missing or wrong token gets `401`
  with `WWW-Authenticate: Bearer`.
- **Format.** Responses are JSON with `Cache-Control: no-store`,
  `X-Content-Type-Options: nosniff` and `x-request-id`. Request bodies must be
  `application/json`.
- **Limits.**
  - Bodies are capped at `max_body_bytes` (default 64 KiB). Larger bodies get `413`.
  - At most `max_concurrent_requests` (default 16) are served at once. Requests beyond that
    get `503 overloaded`, except `/api/v1/health`.
  - Each request has a deadline of `request_timeout_secs` (default 180). Requests that exceed
    it get `503 request_timeout`.
  - Upstream bodies are capped at `max_response_bytes`.
  - Unknown body fields and unknown query parameters are rejected.
- **Errors.** Every error has the shape
  `{"error": {"code": "<code>", "message": "...", "details": {...}?}}`.

| code | HTTP | meaning |
|---|---|---|
| `unauthorized` | 401 | missing or invalid bearer token |
| `not_found` / `method_not_allowed` | 404 / 405 | unknown route, unknown plan, or wrong method |
| `invalid_json` / `invalid_query` / `invalid_request` | 400 | malformed JSON, bad query parameters, invalid header or profile argument |
| `invalid_body` | 422 | JSON is valid but has the wrong shape (unknown field, wrong type) |
| `invalid_media_id` | 400 | non-canonical media ID, or a whole series where a movie, season or episode is needed |
| `unsupported_media_type` / `payload_too_large` | 415 / 413 | |
| `idempotency_key_reused` / `plan_not_active` | 409 | |
| `profile_required` | 422 | target is outside the catalog and there is not exactly one profile for its kind |
| `metadata_unavailable` | 404 | target is not in the catalog and no metadata provider could describe it |
| `catalog_not_imported` | 503 | no active settings (no managed setup and no import) |
| `provider_not_configured` / `no_usable_indexers` | 503 | no search backend (native `[[indexer]]` or `[prowlarr]`) / no indexer in the settings can be searched (with native indexers the message lists why: not configured, or an imported policy the native provider does not implement) |
| `invalid_setup` | 422 | a managed setup document failed validation; `details.errors` lists every problem |
| `setup_conflict` | 409 | managed setup or the Plex bootstrap refused: this state folder holds a Sonarr/Radarr snapshot import, or the replacement would drop a profile/root folder the catalog uses, or an indexer/download category active downloads use |
| `indexer_limit_reached` | 409 | a native indexer's configured `grab_limit` is used up for its window |
| `no_peer_discovery` | 409 | the embedded engine could find no peer for the payload (no tracker it may use, DHT off or a private torrent, no initial peers); nothing was queued |
| `overloaded` / `request_timeout` | 503 | load shedding or server deadline |
| `upstream_timeout` | 504 | provider timed out |
| `upstream_auth` | 502 | provider rejected the configured credential |
| `upstream_unavailable` | 502 | connection failure or 5xx |
| `upstream_bad_response` | 502 | malformed JSON, oversize body, redirect, unexpected 4xx |
| `upstream_rate_limited` | 503 | provider returned 429, is in its `Retry-After` backoff, or a native query limit is used up (`details.failure.kind` = `rate_limited` / `throttled`) |
| `execution_disabled` | 409 | the request needs `[execution] enabled = true` (execute, grabs, commands) |
| `client_not_configured` | 409 | no `[qbittorrent]`/`[library]` root, no imported torrent client, or an unsupported client option |
| `import_unsupported` | 409 | imported settings block file import for that media kind (nothing is grabbed that could not be imported) |
| `already_queued` | 409 | the release overlaps a download in progress |
| `blocklisted` / `duplicate_torrent` | 409 | the release is blocklisted for the item / the torrent is already tracked |
| `invalid_payload` / `no_download_link` | 409 | the downloaded torrent is invalid or its hash differs from the search result / no link |
| `release_not_found` | 404 | no approved release with that ref for that target (search first; links expire after 7 days) |
| `wrong_payload` | 409 | the downloaded torrent's own names contradict the item (another title, or another year for a movie); the release is blocklisted |
| `grab_state` | 409 | the download cannot do that in its current state |
| `seed_criteria_unmet` | 409 | explicit data removal while the seed goal is unmet (pass `ignore_seed_criteria`) |
| `cleanup_blocked` | 409 | data removal refused: not ours, moved, symlinked or library file changed |
| `foreign_torrent` | 409 | the torrent in qBittorrent is not owned by arr |
| `not_managed` / `active_downloads` | 404 / 409 | not a managed item / removal refused while downloads are active |
| `internal` | 500 | details only in the server log |

Upstream errors set `details.provider` and `details.failure`. They never contain URLs,
response bodies or credentials.

## Endpoints

### Service

| method | path | notes |
|---|---|---|
| GET | `/api/v1/health` | Liveness, no auth: `{"status":"ok","mode":"shadow"|"execution",...}` |
| GET | `/api/v1/readiness` | `200` when the state DB and an import are available, otherwise `503`. Includes the import's `observed_at` and catalog age, provider configuration (providers are not contacted), the execution gates per media kind, and a `migration` object kept for older clients: `mode` (`coexistence`), the `ownership` counts, `accepted_limitations`, and an always-empty `stages` list (the staged Sonarr/Radarr migration readiness is gone). Service `ready` is the top-level field |
| GET | `/api/v1/capabilities` | The capability matrix. Each entry has `state` (`available`, `not_configured`, `disabled` or `not_implemented`), `implemented`, `configured` and `verified` (`none`, `unit_tests`, `loopback_mocks`, `loopback_peers`, `sandbox` or `production`). With the embedded engine the client-dependent capabilities are named `embedded_*` (plus `embedded_download_engine`). `verified` is the level this build's tests reach (`STATE/verification.toml` is no longer read, so `sandbox` and `production` are no longer produced). Also returns `execution_enabled` and `policy_blocking_issues` |
| GET | `/api/v1/diagnostics` | Read-only live checks: `download_client` (`backend`, `running`); qBittorrent (WebAPI version, 4.x/5.x naming, categories, global share limits, warnings) or `embedded` (engine, status, listen address, `transport` (`tcp+utp`), `ipv6`, `upnp` and `upnp_mapping`, `connections` (attempt/success/error counts per transport and family), DHT/LSD/PEX/tracker settings, share limits used without an indexer goal, and read-only aggregates: torrent count, `engine_states`, `live_peers`, measured download/upload bytes per second; no hashes, paths or peer addresses), ffprobe, library roots (existence, free space, native root mapping), execution blockers, download counts by status, RSS state |
| GET | `/api/v1/settings` | Normalised imported settings: profiles, tiers, size limits, indexer mapping, release policy, download-client descriptions. No secrets |
| GET | `/api/v1/migration-report` | The active import's report (for managed setup: each applied setting with status and effect, and catalog notes) |

### Managed catalog

- **`POST /api/v1/library`** adds a movie, a series, or a single season or episode, in one call:
  `{"media_id", "search"?: true, "monitored"?, "monitor"?: "all"|"future"|"none", "quality_profile"?, "root_folder"?, "metadata"?}`. Older clients may still send `native_handoff_confirmed` and
  `preserve_monitoring`: both are accepted and ignored.
  - **Scope.** `movie:tmdb:N` and `series:…` add the whole item. `season:…:S` and
    `episode:…:S:E` add the series too when it is new, but then monitor **only that scope**:
    other seasons stay unmonitored, and seasons that appear later are not monitored.
  - **Active series.** For a series that is already *active* here (managed, monitored and,
    adopted), only the requested scope is added and its other choices
    are untouched. A series that is not active is re-activated with exactly the requested
    scope. That covers a new series, a released one, or one that is unmonitored. Any dormant
    flags it carried are cleared first. `monitor` and `monitored: false` are refused for
    scoped adds (use PATCH).
  - **Adoption.** Adding is adopting: only titles added here (or read from Plex and then
    added or followed) are acquired. The response includes `adoption`: the `mode`
    (`coexistence`, the only mode), the `acquisition` state, and the constant `native_app`
    (`null`) and `native_handoff_confirmed` (`false`) that older clients read. There is no
    hand-over step any more, and `409 native_handoff_required` / `409 unsupported_scope` are
    no longer returned.
  - **Specials** (season 0) are never acquired here. Series responses keep a
    `native_retained` list, now always empty.
  - **Movie acquisition blockers.** Movie adds return `acquisition_blockers`: metadata that
    would stop every release from being grabbed. `runtime_unknown` (size limits apply),
    `original_language_unknown` (the profile needs the original language) and
    `release_date_unknown` each name the `metadata` field to supply. The other codes are
    `availability_unsupported`, `profile_unknown`, and `not_released_yet` with its date.
  - **Scan first.** Before anything is planned, the item's folder is scanned (only that
    folder). Files already in the library are owned and never grabbed again. `scan` in the
    response reports `ok` (with `files`, `added`, `unmatched`), `no_folder`, `unavailable`,
    `incomplete`, `root_unmapped` or `not_configured`.
  - **Search** defaults to true. A movie, season or episode returns its plan: executed
    (`download_queued`) with execution enabled, a shadow plan otherwise. A whole series
    queues a `search` command. `search: false` only adds and monitors.
  - The profile and root folder are chosen automatically when exactly one exists for the
    media kind; otherwise pass them. Replays and concurrent calls are serialised with every other library
    change (`PATCH`, `DELETE`), and grabs never duplicate.
  - Movies outside the catalog use Plex's metadata service (TMDB, when a key is configured,
    wins for language, aliases and release dates): title, runtime, aliases and release dates
    (primary release date as in-cinemas, plus the earliest digital and physical releases; from
    Plex, "digital" is the day Plex first showed somewhere to buy or stream it). Without Plex
    or TMDB, pass explicit
    `metadata: {title, year, runtime_minutes?, original_language?, alternate_titles?, imdb_id?, in_cinemas?, physical_release?, digital_release?}`
    (dates `YYYY-MM-DD`). Without either, the answer is `409 metadata_unavailable`, with
    guidance.
  - **Availability** follows native `minimumAvailability = released`: the earliest
    physical/digital release, else in-cinemas + 90 days, plus the imported
    `availabilityDelay`. A movie with no release date is monitored but not available to grab
    (`not_available`, with the reason). Unavailable movies are not searched; an explicit
    request is retried with backoff and grabbed once the date has passed. Other
    `minimumAvailability` modes are reported as not implemented.
  - Series outside the catalog need TVmaze (for the episode list).
- **`PATCH /api/v1/library/{media_id}`** with `{"monitored": bool}` works for a movie,
  series, season or episode.
- **`DELETE /api/v1/library/{media_id}`** stops managing a movie or series. Files, history and
  ownership evidence are kept. It answers `409 active_downloads` while downloads for the item
  are active. In coexistence mode every grab (manual, plan, request, RSS, worker search) re-reads adoption from the stored rows in the transaction that records it, so a release or unmonitoring committed while its torrent was being fetched wins: the grab is refused (`409 not_adopted`) and nothing is recorded or submitted. A grab recorded first makes a later release answer `409 active_downloads`.
- **`POST /api/v1/remove`** with `{"media_id"}` (a movie or series) is `DELETE /library/{media_id}`
  for people: it first cancels the title's downloads that have not finished (with their
  download data) and ends its retrying request, then stops managing it. Answers
  `{media_id, managed: false, cancelled}`. A download whose data is all there and that is being
  added to Plex is left to finish, and the call answers `409 active_downloads` until it is done.
  Files in the library are never touched. The web app's "Remove from Arr" uses it.
- **`GET /api/v1/library/roots`** shows each root's status from the last rescan.
- **`GET /api/v1/storage`** returns `{drives: [{label, free_bytes, total_bytes}]}`: the drive
  holding the movie folder and the one holding the TV folder, or a single `Movies and TV` entry
  when both are on the same drive. Labels are `Movies`, `TV` or `Movies and TV`; paths are never
  returned. A folder that cannot be read is left out.
- **`GET /api/v1/wanted?media_id=&limit=&offset=`** lists monitored, aired, missing items
  that are not being downloaded, with their search schedule. These are searched only on
  request (`search_missing`); routine acquisition is RSS.
- **`GET /api/v1/calendar?start=&end=`** lists episodes in a window (at most 92 days), each
  with `owned`, `queued`, `missing`, `unknown` or `upcoming`.

The web app's proxy (`/app/api/v1/...`, signed-in browsers only) adds one key to these replies:
`requested_by`, the name of the person who first added the title from the web app, or `null`
(on library items and on the movie and show pages). The name is the part of a Google account's
address before the `@`; a sign-in link carries no name, and adds made through this API or MCP
have none. The plain API does not return it. The movie and show pages also carry `removable`
(Arr manages the title, so it can stop tracking it).

### Ownership, live status and attention

- **`GET /api/v1/ownership?state=&limit=&offset=`** lists acquisition ownership. Each row has
  one state:
  - `adopted`: acquired here;
  - `legacy_managed` and `observed`: only in state written by an older release (never
    acquired);
  - `released`: removed after an adoption.

  Series rows carry their effective `scope` per season (`season_monitored`,
  `monitored_episodes`, `episodes_known`, `native_monitored_episodes` as stored), plus
  `monitor_new_seasons` and `native_specials_monitored`. Rows keep `handoffs` and `native`
  as stored by older releases (nothing writes them now); `handoff` is a constant note that
  this server does not contact or depend on Sonarr/Radarr.
- **`GET /api/v1/status/{media_id}`** gives the live status of a movie, series, season or
  episode (TVDB- or TVmaze-keyed scopes resolve to the same series):
  - `acquisition` and `monitored`;
  - for movies, `state` (`owned`, `downloading`, `missing`, `not_available`, `unknown`),
    `file`, `availability` and the active `download`;
  - for series, `seasons` from the live episode list: `episode_file_count` (owned),
    `episode_count` (aired), `total_episode_count`, `queued`, `missing`, `monitored_missing`,
    `unaired`, `complete_aired`, `complete`, and `source: live` (or `snapshot` when no
    episode list is known), plus the series' `metadata_hold` (if any) and `native_retained`
    (specials kept native);
  - for season and episode scopes, `episodes`, each with its `state` (`owned`,
    `downloading`, `missing`, `unaired`, `unknown`), monitoring, file and active download
    progress;
  - `downloads` from the last 7 days, by what each actually covers (not by the plan that
    created it), and the `requests` in scope (a season includes its episodes' requests, and
    an episode the request of its season);
  - `attention`.
- **`GET /api/v1/attention`** lists, bounded:
  - metadata numbering holds (`metadata_hold`, only on series an older release imported);
  - imports that cannot proceed (`import_blocked`, with the reason and the retry action);
  - failed downloads of the last 7 days;
  - requests retrying 3 or more times;
  - requests that expired in the last 14 days.

  No notifications are sent.
- **`GET /api/v1/feed`** is what the web app's home shows, in plain words: `needs_you`,
  `on_its_way`, `arrived`, `looking` and `coming_up`. Catalog and state only, no network
  calls. Two things in it help a client explain itself:
  - each `needs_you` item has `problem` (a short sentence), plus `kind` (`download_failed`,
    `import_blocked` or `request_expired`) and `failure` (why a download failed, for example
    `stalled` or `wrong_quality`; null otherwise);
  - each `on_its_way` item has `eta_seconds`, the time left at the download's recent rate
    (smoothed over polls, kept in memory), or null when unknown, paused or stalled.
- Season counts in `/library/series`, `/discover` and `/media/{series}` come from the live
  episode list (`source: live`), not from the snapshot's statistics.

### Catalog and discovery

- **`GET /api/v1/library/movies?q=&sort=&limit=&offset=`** and
  **`GET /api/v1/library/series?q=&sort=&limit=&offset=`** return the managed catalog (seeded from
  the import, then updated by this server; removed items are hidden). Series include
  `episode_counts`. Every item carries `added_at` (when the title entered the catalog, RFC 3339)
  and `size_bytes` (the file, or the sum of a series' episode files, as far as Arr knows them).
  `sort` is `title` (A to Z), `added` (newest first) or `size` (largest first); without it the
  list is A to Z, with `q` in match order.
  `q` is a folded title/alias search: accents and case are ignored, and an exact key match is
  listed first. Responses are `{total, items, observation: {import_id, observed_at, note}}`.
- **`GET /api/v1/discover?kind=movie|series&q=&year=`** merges catalog hits with TVmaze (for
  series, when enabled) and Plex's metadata service, then TMDB as the backup (for movies, when
  configured). Each result carries
  `media_id`, `in_catalog`, `library_state` (`in_plex`, `on_its_way`, `tracking` or `null`),
  `sources` and `external_ids`. `providers[]` gives each provider's
  status: `ok`, `failed` (with `error`), `disabled`, `not_configured` or `not_imported`. When
  neither Plex nor TMDB can answer, the `plex` provider is `not_configured` and its message says
  the install needs to sign in to Plex.
- **Review scores.** Plex's metadata service (needs the Plex sign-in) gives IMDb and Rotten
  Tomatoes scores. `scores` is present on `/discover` results, the web app's `/ideas` items,
  `/show/{media_id}` and `/movie/{media_id}`, and is `null` when Plex has none (no sign-in, an
  unknown title, a slow or failed lookup; a show lookup never delays an answer by more than
  1.5 seconds). Existing keys such as `rating` are unchanged.
  ```json
  "scores": {"imdb": 7.8, "critics": 96, "audience": 87, "verdict": "likely_good"}
  ```
  `imdb` is out of 10 (one decimal); `critics` and `audience` are Rotten Tomatoes percents.
  Unknown scores are left out. `verdict` is `null` unless both Rotten Tomatoes scores are known;
  otherwise the first rule that fits: both 70 or more `likely_good`, both under 50
  `probably_skip`, critics 15 or more above the audience `critics_pick`, audience 15 or more
  above critics `crowd_pleaser`, else `mixed`. Movies in `/discover` get scores only from
  Plex's own search hits.
- **`GET /api/v1/ideas`** lists popular shows with a new episode in the last 7 days (TVmaze
  web schedule, cached 6 hours): `{status: ok|unavailable|disabled, items}`. Each item has
  `media_id`, `title`, `year`, `network`, `summary`, `rating`, `genres` (up to four),
  `in_library`, `owned` (already in the library or in Plex) and `library_state` (as in
  discover). Shows hidden with `hide` are left out.
- **`POST /api/v1/ideas/hide`** with `{media_id}` (a series id) is "Not interested": the show
  stops appearing in Ideas for everyone using this Arr. The list is kept in the private
  `web.sqlite` (table `hidden_ideas`). It changes no download, file or library entry.
  **`DELETE /api/v1/ideas/hide/{media_id}`** undoes it. Both are idempotent.
- **`GET /api/v1/show/{media_id}`** and **`GET /api/v1/movie/{media_id}`** carry the title page:
  `summary`, `runtime_minutes`, `genres`, and `cast` (up to six `{name, character}`, from Plex or
  TVmaze, or TMDB for a movie when it has a key). Any of them can be empty or null.
- **`GET /api/v1/media/{media_id}`** resolves a target. For a season or episode it returns the
  episode list with `aired` and `ownership` (`owned` / `missing` / `unknown`), plus
  `ownership_source` and `episode_list_source`.

### Release search (no state change)

`GET /api/v1/media/{media_id}/releases?refresh=false` searches the configured backend (native
indexers, or Prowlarr) with the settings' interactive-search indexers and evaluates every
release. With native indexers each query runs once per indexer with that indexer's own
categories; every query view carries `provider: "native"`, `indexer`, `dropped_results` and,
without results, `empty: "confirmed" | "unconfirmed"`, and indexers the settings name that
cannot be searched appear with status `unavailable`. `completeness.provider` is then
`complete` (every indexer answered in full; an explicit "no results" counts) or `incomplete`
(issues `query_failed`, `page_failed`, `page_limit`, `indexer_unavailable`,
`empty_unconfirmed`, each with its `indexer`). Through Prowlarr it stays `unverified`. The same
torrent (info-hash) from several indexers is one candidate: the copy with more seeders, then
the later publish date, is kept, and `found_by` lists every query that returned it. A
release whose tracker seed requirement (Torznab `minimumratio`/`minimumseedtime`) the
indexer's seed goal cannot meet is rejected `seed_requirement_unmet`.

```bash
curl -s -H "Authorization: Bearer $(cat STATE/secrets/api_token)" \
  http://127.0.0.1:8787/api/v1/media/season:tvdb:372264:1/releases
```

```jsonc
{
  "target": "season:tvdb:372264:1", "requested": "season:tvdb:372264:1", "profile": "sonarr:6",
  "policy_blockers": [],              // target-level problems (apply to every release)
  "outcome": "results",               // what was observed: "results" | "observed_empty"
  "completeness": {                   // how much that observation covers
    "retrieval": "complete",          // "complete" | "incomplete" (see issues)
    "provider": "unverified",         // per-indexer coverage is not observable via Prowlarr search
    "issues": [],                     // [{ "query", "kind": "page_limit" | "page_failed" | "query_failed", "detail" }]
    "note": "Prowlarr GET /api/v1/search reports no per-indexer status and can return HTTP 200 with an empty list after an indexer failure; ..."
  },
  "counts": { "unique": 3, "approved": 1, "rejected": 2, "rejection_codes": { "covers_owned": 1, "multi_season_pack": 1 } },
  "queries": [{ "query": "Slow Horses S01", "categories": [5000, 5040, 5045], "indexer_ids": [1], "status": "complete",
                "pages_fetched": 1, "raw_results": 3, "unique_results": 3, "truncated_by_page_limit": false,
                "from_cache": false, "fetched_at": "..." }],
  "releases": [
    { "rank": 1, "release_ref": "rel_15a61a4c8f92d4120e5dedf0", "title": "Slow.Horses.S01E02.1080p.WEB-DL.x264-GRP",
      "indexer": { "id": 1, "name": "The Pirate Bay" }, "protocol": "torrent", "size_bytes": 2684354560, "seeders": 20,
      "parsed": { "title": "slow horses", "year": null, "numbering": { "kind": "episodes", "season": 1, "episodes": [2] },
                  "quality": "webdl_1080p", "quality_basis": "parsed", "revision": { "version": 1, "real": 0 }, "languages": [] },
      "decision": { "approved": true, "health": "healthy", "rejections": [], "covered_episodes": [2], "owned_overlap": [],
                    "notes": ["release names no year; matched by title only", "..."] } },
    { "rank": 2, "title": "Slow.Horses.S01.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb",
      "decision": { "approved": false, "full_season": true, "owned_overlap": [1, 3],
                    "rejections": [{ "code": "covers_owned", "message": "would replace owned episodes [1, 3]: E01: the owned file's revision is unknown; revision upgrades fail closed; ..." }] } }
  ],
  "releases_omitted": 0,              // evaluated releases beyond the 200-entry response cap
  "uncertainty": ["Prowlarr's search API does not report per-indexer failures: ...", "..."]
}
```

How the response separates observation from completeness:

- If every query fails, the response is an upstream error (`502`/`504`).
- `outcome` is `results` when at least one release was observed. It is `observed_empty`
  (status `200`) when none was. That is not proof of absence: Prowlarr can return HTTP 200
  `[]` after an internal indexer failure, so `completeness.provider` is always `unverified`.
- `completeness.retrieval` is `incomplete` when any query failed (`query_failed`), failed on
  a later page (`page_failed`), or stopped at the configured page bound with a full last page
  (`page_limit`). Releases that were retrieved are still listed and evaluated.
- Per-query `status` is `complete`, `truncated`, `partial` or `failed`.
- TV queries keep a native title disambiguator as a bare year (`WAR (2026)` gives
  `WAR 2026 S01`). Movie queries carry the movie year once.
- Results are cached per query for `cache_ttl_secs` (complete or page-bounded results only,
  never partial ones). `refresh=true` bypasses the cache.

Approved results of this endpoint can be grabbed with `POST /api/v1/grabs` (below). Their
link material is kept privately for 7 days.

### Plans

`POST /api/v1/plans` (optional header `Idempotency-Key: [A-Za-z0-9._:-]{1,128}`):

```json
{ "media_id": "season:tvdb:372264:1", "quality_profile": null, "replan": false, "execute": false, "revisions": false }
```

- `execute: true` submits the planned candidates through the download outbox. It needs
  `[execution] enabled`; otherwise the answer is `409 execution_disabled`. An existing active
  shadow plan is executed as it is.
- `revisions: true` also searches owned units for native revision upgrades (exact quality,
  higher known revision).
- `POST /api/v1/plans/{plan_id}/execute` executes a stored `shadow_planned`/`shadow_partial`
  plan. The plan records `execution.grabs` and `execution.refused` (per-candidate refusal
  codes such as `covers_queued` or `wrong_payload`). Its state is `download_queued` until
  qBittorrent confirms a grab (`execution.submitted` then turns true, with `confirmed_at`),
  then `download_started`, then `imported`; `download_failed` means every grab ended
  before any confirmation. A candidate refused as `wrong_payload` is replaced by a fresh plan
  (the response is that plan).
- `execution.outcome` says what the execution did:
  - `queued`: every candidate entered the outbox;
  - `partially_queued`: some candidates were refused, and the state follows the queued grabs;
  - `nothing_queued`: every candidate was refused, and the plan keeps its shadow state, so it
    can be executed again.

  The field is absent for shadow plans.
- An executed plan is retried as an explicit request with backoff (see
  `GET /api/v1/requests`) when it left wanted units unplanned, or when it refused a candidate
  for a planned unit that no active download covers. This applies to `partially_queued` as
  well as `nothing_queued`. A retry plans only the units that are still missing; queued and
  owned units are never grabbed again. A refusal for a unit that is already being downloaded
  (`covers_queued`) creates no request.

- Returns `201 {"created": true, "plan": {...}}`.
- If the target already has an active plan, returns `200 {"created": false, "plan": <existing>}`.
- **`Idempotency-Key`.** A key is bound to the request's fingerprint and to the plan the
  request returned, whether that plan was newly created, already active, or the winner of a
  concurrent race.
  - Replaying the key returns that same plan (`200`), even after it is cancelled or
    superseded, and across restarts.
  - Reusing the key for a different request returns `409 idempotency_key_reused`.
  - Keys bound by earlier releases (state schema 1/2) replay unchanged: omitted or false
    `execute`/`revisions` do not change the request fingerprint.
- `replan: true` supersedes the active plan.
- `quality_profile` applies only to targets outside the catalog when more than one profile
  exists. It may not override a catalog item's own profile.

Plan shape (abridged, from the synthetic Slow Horses test scenario: E01 and E03 owned):

```jsonc
{
  "id": "pln_997e3cbb823ee547495b40bb", "target": "season:tvdb:372264:1", "title": "Slow Horses S01",
  "state": "shadow_partial", "selection": "episodes", "profile": "sonarr:6", "destination_root": "/tvshows",
  "execution": { "mode": "shadow", "download_submission": "disabled", "submitted": false, "note": "...", "grabs": [], "refused": [], "executed_at": null, "confirmed_at": null },
  "import_id": "imp-...", "catalog_observed_at": "2026-10-01T12:00:00Z",
  "units": [
    { "label": "S01E01", "status": "owned", "reason": "has a file; owned items are re-searched only by an explicit revision search (revisions: true)" },
    { "label": "S01E02", "status": "planned", "candidate": "rel_15a6...", "reason": "best approved episode release" },
    { "label": "S01E05", "status": "no_approved_release", "reason": "2 release(s) covered this episode, none approved: covers_owned x1, quality_not_allowed x1" }
  ],
  "candidates": [{ "release_ref": "rel_15a6...", "title": "Slow.Horses.S01E02.1080p.WEB-DL.x264-GRP", "quality": "webdl_1080p",
                   "size_bytes": 2684354560, "seeders": 20, "covers_episodes": [2], "full_season": false }],
  "blockers": [{ "code": "no_approved_release", "message": "no approved release for [\"S01E05\"]" }],
  "notes": ["season packs not eligible: they would re-download owned episodes [1, 3] (owned items are re-searched only by an explicit revision search)"],
  "search": { "completeness": { "retrieval": "complete", "provider": "unverified", "issues": [], "note": "..." },
              "releases_considered": 6, "approved": 3, "rejection_counts": { "...": 1 }, "queries": ["..."] }
}
```

- **States:**
  - `shadow_planned`: every wanted unit has an approved candidate.
  - `shadow_partial`: some wanted units do.
  - `blocked`: nothing can be planned; see `blockers`.
  - `already_satisfied`: everything wanted is owned.
  - `in_progress`: everything wanted is owned or being downloaded.
  - `cancelled` and `superseded`: the plan is no longer active.
  - `download_queued`, `download_started`, `download_failed` and `imported`: the plan was
    executed (see above). An outbox entry is never reported as a started download.
  - Plans recorded by earlier builds keep `download_submission: "not_implemented"`.
- **Unit statuses:** `planned`, `owned`, `queued`, `not_aired`, `no_approved_release`
  (covering releases were observed, none approved), `no_release_observed` (nothing
  covering was observed; provider completeness unverified), `search_failed`,
  `not_searched`, `ownership_unknown`.
- **Plan blockers** mirror the units: `no_approved_release`, `no_release_observed`,
  `search_failed`, `not_searched`, plus any target-level rejection codes.
- **Completeness.** `search.completeness` uses the same record as the search endpoint.
  `notes` name any incomplete retrieval. Plans stored by earlier builds show
  `retrieval: "unknown"`, and their queries may show `status: "ok"`.
- **Other plan routes:**
  - `GET /api/v1/plans?state=&target=&limit=&offset=` lists plans, newest first.
  - `GET /api/v1/plans/{plan_id}` returns one plan.
  - `POST /api/v1/plans/{plan_id}/cancel` changes local state only, and returns `409
    plan_not_active` if the plan is already inactive.

Release decisions use the codes in the `RejectionCode` schema.


### Grabs and the download queue

Raw torrent info-hashes, download links, magnets and payloads never appear in queue, history
or blocklist responses. The grab id (`grb_…`) and release ref (`rel_…`) are the handles.

- **`POST /api/v1/grabs {"media_id","release_ref"}`** makes an interactive grab of an approved
  result of `GET /media/{media_id}/releases`. It answers `201 {"grab": ...}`. Possible
  refusals:
  - `409 execution_disabled`, `already_queued`, `blocklisted`, `duplicate_torrent`,
    `invalid_payload`, `import_unsupported` or `client_not_configured`;
  - `404 release_not_found`;
  - `502`/`504` when the Prowlarr download fails.
- **`GET /api/v1/queue?all=&limit=&offset=`** lists active downloads and those still tracked
  for cleanup (with `all=true`, also anything updated in the last 7 days).
  **`GET /api/v1/queue/{grab_id}`** returns one download with its import journal (`dest`,
  `method`, `state`) and its events.

A grab looks like this:

```jsonc
{ "id": "grb_…", "media_id": "movie:tmdb:603", "target": "movie:tmdb:603", "release_ref": "rel_…", "title": "…", "client_name": "…",
  "state": "imported",            // queued | unconfirmed | downloading | completed | importing | import_blocked | imported | failed | cancelled | released
  "status": "seeding",            // user-facing: pending_submission, submitting, downloading, paused, stalled, fetching_metadata,
                                  // client_error, missing_files, queued_in_client, checking, moving, import_pending, importing,
                                  // import_blocked, imported, seeding, cleanup_pending, cleanup_blocked, cleaned, failed, cancelled, released
  "origin": "automatic", "progress": 1.0, "client_state": "seeding", "user_paused": false,
  "last_error": null, "failure": null, "retry": null,
  "seed_goal": { "ratio": 2.0, "seeding_time_minutes": null },
  "cleanup": { "state": "seeding", "detail": "still seeding (qBittorrent has not stopped it at a share limit)", "at": null,
               "policy": { "remove_completed": true, "remove_failed": true } },
  "imported_files": 1, "targets": [{ "target": "movie:tmdb:603", "replaces": false }] }
```

Queue actions:

- **`POST /api/v1/queue/{id}/pause`** and **`/resume`** act on our torrent only; anything else
  is `409 foreign_torrent`. 4.x uses `pause`/`resume`, 5.x `stop`/`start`.
- **`POST /api/v1/queue/{id}/cancel`** takes an optional body
  `{"blocklist": false, "remove": "keep"|"torrent"|"torrent_and_data", "ignore_seed_criteria": false, "research": false}`.
  - `keep` (the default) stops an incomplete torrent and keeps everything.
  - `torrent` removes only our torrent entry.
  - `torrent_and_data` journals and removes our torrent and its tracked files. It never
    touches library files, and answers `409 seed_criteria_unmet` unless the seed goal is met
    or `ignore_seed_criteria` is set.
  - Without `research`, the targets wait 24 h before an automatic search.
- **`POST /api/v1/queue/{id}/retry`** answers with an `action`:
  - `reimport` for `import_blocked`;
  - `resubmit` for pending submissions;
  - `cleanup` to re-evaluate a blocked cleanup;
  - `search` for an alternative to a failed or cancelled download (the blocklist stays).

### History and blocklist

- `GET /api/v1/history?media_id=&kind=&limit=&offset=` returns events, newest first. Kinds
  include `grabbed`, `release_rejected`, `download_started`, `download_completed`, `file_imported`,
  `file_replaced`, `import_completed`, `import_blocked`, `download_failed`,
  `download_cancelled`, `download_released`, `download_removed`, `added`, `monitored`,
  `unmonitored`, `removed`, `metadata_refreshed`, `rescan` and `recovered`.
- `GET /api/v1/blocklist?media_id=` and `DELETE /api/v1/blocklist/{entry_id}`.

### Commands, tasks, requests and RSS

- **`POST /api/v1/commands {"name", "media_id"?}`** answers `202` when queued, or `200` with
  the pending command if an identical one is already queued. It needs execution enabled.
  Names:
  - `search` (movie, series, season or episode);
  - `search_missing` (the backlog, optionally for one item, in batches of `searches_per_run`
    per pass, skipping held units);
  - `rss_sync` (forced, ignoring backoff);
  - `refresh_metadata` (TVmaze episode lists);
  - `rescan` (explicit, so the mass-missing guard does not apply);
  - `poll_downloads`, `cleanup_downloads`;
  - `plex_refresh` (needs a media_id).

  Commands are leased, retried up to 3 times on outages, and requeued after a restart.
  Follow them with `GET /api/v1/commands` and `GET /api/v1/commands/{id}` (`state`, `result`).
- **`GET /api/v1/system/tasks`** shows each scheduled task (`downloads`, `rss_sync`,
  `requests`, `refresh_metadata`, `rescan`, `prune`) with its last start, finish, status and
  result, and its next run.
- **`GET /api/v1/requests`** lists explicit requests that are still owed something: a wanted
  unit was unplanned, or its candidate was refused and no active download covers it. It shows
  attempts, next attempt, expiry, `last_result` (e.g. `download_queued: partially queued
  (invalid_payload)`) and state (`active`, `satisfied` or `expired`).
- **`GET /api/v1/rss`** shows the feeds (RSS-enabled imported indexers and their
  categories), per-indexer state (`last_success_at`, `newest_published_at`, `failures`,
  `next_attempt_at`, `gap_possible`) and the 200 most recent items. Each item has its
  `status` (`grabbed`, `rejected`, `deferred` or `ignored`) and the reason. Links are never
  included. `provider_completeness` is `unverified` through Prowlarr and `per_indexer` with
  native indexers. `gap_possible` is set when the last read's window did not reach back to
  the newest release the sync before it saw (its oldest dated item was newer, or none was
  dated), independently of how many rows the feed returned; the first sync never sets it.


### Standalone setup, Plex discovery and the library bootstrap

- `GET /api/v1/setup` — what the deployment has (active settings and their `origin`:
  `managed_setup`; search and download backends; library; Plex mapping;
  metadata; MCP) and `next_steps`. Nothing is contacted.
- `POST /api/v1/setup` `{"document": {...}, "dry_run": false}` — creates or replaces managed
  settings from a setup document (the TOML of `config/setup.example.toml` as JSON; every key
  optional, `{}` = defaults) and activates them in this process. Answers `201` (`created`,
  with `import_id`, `replaced`, profiles, roots, indexer policy with seed goals, download
  policy, `warnings`) or `200` (`unchanged`, `dry_run`). Policy only: URLs and credentials
  stay in `config.toml`. Never enables execution or queues searches. Refused (`409
  setup_conflict`) over a state folder that holds a Sonarr/Radarr snapshot import, or when the catalog or active downloads use
  what the replacement drops; invalid documents give `422 invalid_setup` with every error.
- `GET /api/v1/setup/plex` — read-only Plex discovery: `sections` (key, title, type,
  locations) and `mapping` per media kind (`ok`, `not_configured`, `section_not_found`,
  `wrong_type`, `path_unmapped`, `path_not_in_section`, with candidates and hints).
- `POST /api/v1/setup/plex/bootstrap` `{"media": "all"|"movies"|"tv", "restart": false,
  "dry_run": false, "batch": 50}` — one bounded batch per media kind of the read-only library
  bootstrap; repeat until `done`. Each run reports `status` (`running`, `done`, `failed`),
  the resume `offset`/`total`, `counts` (`items_seen`, `titles_created`, `titles_existing`,
  `files_recorded`, `files_known`, `episodes_owned`, `unresolved`, `unmapped`,
  `missing_locally`, `unsupported`, `conflicts`, `metadata_failures`) and the first 200
  `issues`. Titles become owned and **not monitored**; nothing is searched. A file naming
  another title is a `conflict`. For a new show found by its exact TVDB id, Plex's title
  (when it differs from TVmaze's) and an official TVmaze AKA that a file name matches exactly
  (`GET /shows/{id}/akas`, read only when such files exist) are kept as alternate titles, each
  reported as a `note`. A metadata outage
  stops the batch at that item (`last_error`) and the next call resumes there. Needs managed
  settings (`409` over a Sonarr/Radarr snapshot import). `GET` lists the stored runs.

`GET /api/v1/readiness` adds `search_backend` (native: per-indexer health and the
`settings_mapping` of every indexer id, `ok`/`degraded`/`blocked`) and `peer_discovery`
(embedded engine: `ok`, or `blocked` when trackerless magnets meet DHT off and no initial
peers). `GET /api/v1/diagnostics` adds `search_backend` with each native indexer's origin,
limits, caps and health; its `kind` is `thepiratebay`, `knaben` or `torznab`, and
`recent_feed` says whether an RSS read of the indexer is on.

### Web app notifications (signed-in browser routes)

These belong to the web app, not the bearer-token API, so they are not in the OpenAPI
document. They take the web app's session cookie and the `X-Arr-App: 1` header, like every
`/app/api` route, and answer `401` without a sign-in.

- `GET /app/api/v1/notifications/key`: `{"key": "<base64url>"}`, the server's VAPID public
  key (the browser's `applicationServerKey`). The key pair is made on first use and kept in
  `STATE/secrets/vapid_key`.
- `POST /app/api/v1/notifications/subscribe`: a browser's `PushSubscription.toJSON()`,
  `{"endpoint", "keys": {"p256dh", "auth"}}`. `204` when stored (sending the same endpoint
  again updates it), `400 invalid_request` for a bad body, keys or endpoint, `409
  too_many_devices` past 100 devices. The endpoint must be `https`, on the default port, at
  a public host (never a private, loopback or link-local address), and at most 2048
  characters.
- `DELETE /app/api/v1/notifications/subscribe`: `{"endpoint"}`; `204`.

Arr checks the web app's feed once a minute and pushes "*Title* is ready to watch" for each
new item in Arrived and "*Title* needs you" for each new item in Needs you, as an encrypted
(RFC 8291 `aes128gcm`) message with a VAPID (RFC 8292) `Authorization` header. The message is
`{"title", "body", "url"}`. The first pass after the feature is first used only records what
is there. A subscription the push service answers `404` or `410` for is dropped. The service
worker is served at `/app/sw.js` (scope `/app/`).

### Update, app settings and health

These take the bearer token on `/api/v1` (and the web app's session through `/app/api/v1`, which
passes exactly these routes). All are in the OpenAPI document.

- `GET /api/v1/update`: `{current, latest, available, can_update, auto_updates, release_url,
  status}`. `latest` is GitHub's latest release, checked at most every six hours and only when
  automatic updates are installed (`null` otherwise, or when GitHub didn't answer).
  `can_update` is true only when the installed updater and the service manager's watcher can
  be started from here; installs from before this existed report `false` until the installer
  runs again. `status` is `{state, from, to, at}` from the updater's `update-status.json`
  (`idle`, `requested`, `checking`, `up_to_date`, `updating`, `failed`, `done`).
- `POST /api/v1/update`: no body; asks the installed updater to run. `202` with the same body
  as `GET`; `409` when this install can't update from here, Arr is up to date, or a request is
  already on its way. Nothing sent decides what is installed.
- `GET /api/v1/settings/app`: `{downloading, embedded, upload_limit_kib, torrent_port,
  error_reports, error_reports_available, tmdb_key_set, can_restart}`. Never a secret.
- `POST /api/v1/settings/app`: `{"name", "value"}` where name is `downloading`,
  `upload-limit-kib`, `torrent-port`, `error-reports` or `tmdb-key` (anything else is `400`).
  It runs `arr set`'s checks and rolls every file back on failure, then restarts Arr. `409` when
  Arr wasn't started by a service manager. A value already in place changes and restarts
  nothing (`changed: false`).
- `GET /api/v1/system/health`: `{items: [{id, label, level: ok|warn|bad, text}], problems:
  [{at, level, component, message}]}`. Problems are the last 50 warnings and errors, newest
  first, held in memory and scrubbed.

(`/api/v1/settings` is unchanged: it is the imported settings document.)

### Remote MCP

With `[mcp] enabled`, `POST /mcp` serves MCP over Streamable HTTP (stateless JSON) with its
own bearer token; it is not part of `/api/v1` and is described in [MCP.md](MCP.md).
