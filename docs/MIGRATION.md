# Configuration and catalog migration

The import is **a copy, not a move**. Sonarr, Radarr and Prowlarr are never contacted or
modified; the importer reads one JSON snapshot file.

```bash
STATE=/srv/arr/state          # any private path outside this repo
arr init --state-dir "$STATE" --prowlarr-url http://127.0.0.1:9696
install -m 600 /dev/stdin "$STATE/secrets/prowlarr_api_key" <<<"$PROWLARR_KEY"   # or write it however you prefer
arr import --state-dir "$STATE" --snapshot /private/native.json --observed-at 2026-10-26T06:56:00Z
arr check --config "$STATE/config.toml"
```

`import` prints a JSON summary: `status` (`created`, `unchanged` or `dry_run`), `import_id`,
counts, `blocking_issues` and `warnings`. The full report is written to
`imports/<id>/migration-report.{json,md}` and served at `GET /api/v1/migration-report`.

## What is copied

| source | normalised into | notes |
|---|---|---|
| quality profiles (both apps) | `settings.profiles[]` | Allowed qualities, grouped into tiers in native ascending order. Disallowed items are dropped because they are never approvable. Cutoff tier, upgrade flag and language rule are kept. |
| quality definitions | `settings.size_limits[]` | MiB/minute min, max and preferred, per media kind. A max of `null` or 0 means unlimited (native semantics). |
| Radarr/Sonarr indexers | `settings.indexers[]` | Mapped to the Prowlarr indexer ID from the Torznab/Newznab `baseUrl` (`http://prowlarr:9696/<id>/`). Copies categories (only 2000–2999 for movies, 5000–5999 for TV), priority, `minimumSeeders`, the automatic/interactive/RSS flags and seed criteria. |
| download clients | `settings.download_clients[]` | Host, port, SSL, URL base, category, post-import category, initial state, sequential, first/last, layout and remove flags. **Credential values are never copied**, only the field names. The field `execution: "disabled"` is a historical marker. With `[execution] enabled` the runtime uses the enabled torrent client with the lowest priority value: category (unless overridden in `[qbittorrent]`), initial state, sequential, first/last, content layout, post-import category and `removeCompleted/FailedDownloads`. The connection comes from `[qbittorrent]`, never from the snapshot. |
| `config/mediamanagement`, `config/naming`, `config/downloadclient` | `settings.media_management[]`, release policy, import-scoped issues | `copyUsingHardlinks`, `minimumFreeSpaceWhenImporting` and `replaceIllegalCharacters` drive the import. `autoRedownloadFailed` (and its interactive variant) drive re-search. Anything import cannot honour adds an **import-scoped** blocking issue: file import (and execution) for that media kind is refused, while searches and plans continue. |
| root folders | `settings.root_folders[]` | Path, plus accessibility and free space at snapshot time (observations). |
| `config/indexer`, delay profile, `downloadPropersAndRepacks` | `settings.release_policies[]` | Maximum size, hardcoded subs, torrent enabled and delay, highest-quality bypass, proper preference. |
| `radarr.movie` | `catalog.movies[]` | TMDB/IMDb IDs, titles, original title, aliases, year and secondary year, runtime, original language, status, `isAvailable`, monitored, profile, root folder, folder relative to the root, `hasFile`, `movieFile` evidence (relative path, size, quality, and **revision only when exported**; never guessed), provenance. |
| `sonarr.series` + `episode_samples` | `catalog.series[]` | TVDB/TVmaze/TMDB/IMDb IDs, title, aliases (season-scoped; scene-season aliases are dropped), year, type, scene numbering, runtime, language, season statistics, folder relative to the root, and per-episode `hasFile`/air date/runtime/monitoring plus `episodeFile` evidence (relative path, size, quality, revision when exported) for sampled series. |

Credential detection (values never copied) covers:

- fields with non-`normal` privacy (`apiKey`, `password`, `userName`)
- fields of type `password`
- names containing `apikey`, `password`, `passkey`, `username`, `token`, `secret`,
  `cookie`, `rsskey` or `authkey`

Each such field is reported as `present` or `masked_or_empty`. URL values in the report have
userinfo and query stripped. `additionalParameters` is reported as `<withheld>`. The original
snapshot is kept byte-for-byte, privately, as `source-native.json` (`0600`).

## Status of every relevant setting

The report gives every item one of these statuses:

- `enforced`: native semantics.
- `enforced_stricter`: never approves something native would reject.
- `observed_only`: recorded only.
- `not_applicable`: has no effect given the rest of the configuration.
- `unsupported_blocks_plans`: plans in scope are blocked.
- `unsupported_blocks_import`: file import (and execution) for that media kind is refused;
  searches and plans continue.
- `not_implemented`: a cutover blocker.

A typical small setup (Radarr 5.26 / Sonarr 4.0.15, one profile each, TPB via Prowlarr,
qBittorrent) imports with **0 blocking issues**.

| native setting | status | behaviour here |
|---|---|---|
| allowed qualities and group order | enforced | A quality outside the allowed tiers is rejected. A higher tier ranks first. |
| `upgradeAllowed: false` | enforced | A release covering an owned movie or episode is rejected (`covers_owned`), except a native **revision upgrade** (see `downloadPropersAndRepacks`). This matches Sonarr v4.0.15 / Radarr v5.26.2 `IsUpgradeAllowed`/`IsUpgradable`. |
| `upgradeAllowed: true`, cutoff | not_implemented | Quality upgrades of owned files are never planned. This under-grabs rather than over-grabs. |
| quality definitions (size) | enforced | min ≤ size ≤ max × runtime. For TV the runtime is the sum of the covered episodes' runtimes, falling back to the series runtime (the same formula as Sonarr v4 `AcceptableSizeSpecification`). |
| profile language (Radarr) | enforced | Explicit markers must include the language. Unmarked releases are assumed to be in the movie's original language (Radarr behaviour). `Original` and `Any` are supported. |
| Sonarr v4 `languageprofile` (the `Deprecated` stub) | observed_only | Language profiles were retired in v4 (language rules moved to custom formats; see the official v4 release announcement). The stub is recorded only. With no scored custom formats, Sonarr applies no language filter, and neither does this build (native parity). |
| any other Sonarr language profile (pre-v4) | unsupported_blocks_plans | `language_profile_unsupported` (series only). |
| `minimumSeeders`, indexer priority | enforced | Per media kind and indexer. Unknown seeders are rejected. |
| `allowHardcodedSubs: false` (Radarr) | enforced | `HC`, `SUBBED` and `<lang>SUB(S)` markers are rejected (the Radarr pattern). Sonarr has no such rule, so TV accepts them. |
| `whitelistedHardcodedSubs` | enforced_stricter (if set) | The whitelist is not evaluated, so all hardcoded subs are rejected. |
| `maximumSize` | enforced | MB = MiB, as in native `.Megabytes()`. |
| `minimumAge`, `retention` | not_applicable | Usenet only. |
| delay profile (untagged) | enforced | Torrent delay measured from publish date; top-tier bypass when enabled. `enableTorrent: false` rejects torrents. |
| delay profile with tags, tagged indexer | unsupported_blocks_plans | `tag_routing` for that media kind. |
| `requiredFlags` | unsupported (per indexer) | That indexer's releases are rejected (`indexer_policy_unsupported`). |
| `multiLanguages` | enforced / enforced_stricter | Where a profile requires a language (Radarr), MULTI without an explicit allowed language is rejected as ambiguous. |
| custom formats scored in a profile, `minFormatScore ≠ 0` | unsupported_blocks_plans | `custom_format_scoring`. |
| per-profile-item sizes (Sonarr v5 shape) | unsupported_blocks_plans | `profile_size_overrides`. |
| allowed quality with an unknown name | unsupported_blocks_plans | `unknown_allowed_quality`. |
| allowed quality without a definition | unsupported_blocks_plans | `missing_size_definition`. |
| Radarr profile language "Unknown" | unsupported_blocks_plans | `unsupported_language_rule`. |
| enabled release profiles (required/ignored terms) | unsupported_blocks_plans | `release_profile_terms`. **If the `releaseprofile` section is absent from the snapshot it is reported as unknown/not enforced.** Include `/api/v3/releaseprofile` in the export to close this gap. |
| `downloadPropersAndRepacks` | enforced | Propers and repacks rank above originals unless `doNotPrefer`. With `preferAndUpgrade`, an owned file may be replaced by **exactly the same quality with a higher known revision** (`real`, then `version`), even with upgrades off. An unknown old quality, revision or path fails closed. A different quality (for example a WEBRip PROPER over a WEB-DL) is not a revision upgrade. The replacement is journaled and reversible. |
| indexer `enableRss`, `rssSyncInterval` | enforced | Torznab RSS through Prowlarr for each RSS-enabled indexer, at the native interval (0 disables RSS for that app; the default is 15 min). Feed items match only managed, monitored items. There is no automatic backlog search. |
| seed ratio / seed time (indexer `seedCriteria`) | enforced | Sent as `ratioLimit`/`seedingTimeLimit` on add (the season-pack seed time for full-season packs), and required as the seed goal before an automatic cleanup removes a download. |
| download client `removeCompletedDownloads` / `removeFailedDownloads` | enforced | After a verified import and a met share limit, the client removes our download with its data. Failed downloads are removed when nothing was imported from them. `false`, or `[execution] download_cleanup = false`, keeps them. Library files are never removed. |
| `copyUsingHardlinks`, `minimumFreeSpaceWhenImporting`, `replaceIllegalCharacters`, `enableMediaInfo` | enforced | Hardlink when on the same filesystem, else copy; the free-space minimum is checked before staging; illegal characters are replaced in folder and file names; every imported file is probed with ffprobe. |
| `renameMovies`/`renameEpisodes = false`, standard folder formats, smart colon replacement | enforced | Folders `Movie Title (Year)` and `Series Title/Season N`. Files keep native names (the download title for single-file or single-episode downloads; the original names in season packs). Anything else is `unsupported_blocks_import`. |
| `importExtraFiles`, `setPermissionsLinux`, `useScriptImport`, recycle bin, `fileDate`, completed-download handling disabled | unsupported_blocks_import when enabled | |
| `autoRedownloadFailed`, `autoRedownloadFailedFromInteractiveSearch` | enforced | A failed automatic or interactive download queues a re-search; the failed release is blocklisted. |
| remote path mappings | not used | The download directory mapping comes from `[qbittorrent] save_path`/`local_path`. |
| `minimumAvailability = released`, `inCinemas`/`physicalRelease`/`digitalRelease`, `availabilityDelay` | enforced | Imported per movie and evaluated against the clock (native `IsAvailable`): the earliest physical/digital release, else in-cinemas + 90 days, plus the delay. With no date the snapshot's `isAvailable` is kept as evidence; otherwise the movie is not available. Other modes (`announced`, `inCinemas`, `preDB`) are reported not implemented for that movie. |
| `seasonSearchMaximumSingleEpisodeAge`, `removeYear`, `additionalParameters` | observed_only | |

### Where this build is deliberately stricter than native

Checked against the official source of Sonarr v4 (`main`) and Radarr (`develop`):

- **Unknown release size.** Native skips the size check; here it is rejected (`size_unknown`).
- **Unknown runtime.** Native defaults to 45 min (Sonarr, pilot window) or 110 min (Radarr).
  Here it is rejected (`size_unverifiable_runtime`) when a limit applies.
- **Movie releases without a year.** Rejected (`year_missing`).
- **Titles matching more than one catalog item.** Rejected (`ambiguous_title`) unless the year
  disambiguates.
- **Other rejections.** 3D, sample/extras, collections and bundles, executable/script
  extensions, conflicting source markers, and (for movies, where Radarr requires English)
  MULTI without an explicit language.
- **Packs.** Multi-season packs, partial-season packs and complete-series packs are rejected for
  exact-season targets. Season packs for a season that has not fully aired are rejected.
- **Scene numbering.** Series with scene numbering, and anime or daily series, are blocked.

## Coexistence with the running native apps

`[migration] mode` decides what an import means for acquisition. It is required explicitly
when `[execution] enabled = true`. Without execution it defaults to `coexistence`.

- **`coexistence`** (recommended; the template default). Sonarr and Radarr keep running and
  keep every title that is not adopted here.
  - **Seeding.** A fresh full import seeds every native title **observed**. Its catalog
    data, ownership evidence and native monitoring (`native_monitored`, Sonarr
    `monitorNewItems`) are recorded, but nothing about it is wanted, searched, matched by
    RSS or grabbed. Until it is adopted, an observed title follows the latest import's native
    flags.
  - **Adoption.** `POST /api/v1/library` with `native_handoff_confirmed: true` adopts
    exactly the requested scope. Before calling it, the operator or bridge suspends native
    acquisition for that scope and confirms there is no native download for it. This server
    never contacts the native apps.
  - **Bulk transfer.** `preserve_monitoring: true` (with `search: false`) adopts a whole
    imported movie or series with its current imported monitoring untouched (a partial
    native setup is copied as it is). This is the latest import's intent, not a fresh native
    query, so the before/after native ledger still belongs to the handoff.
  - **While adopted.** Native acquisition stays suspended for the scope, movies included,
    because the copied `preferAndUpgrade` still lets the native app grab PROPER/REPACK
    revisions with quality upgrades off. Restore it only after release
    (`DELETE /api/v1/library`), drain or rollback. A bounded legacy pilot's post-import
    restoration (RSS off) is a verification procedure, not this handoff.
  - **Not transferred.** Specials (season 0) stay native and are listed in
    `native_retained`. Anime/daily and scene-numbered series, identity conflicts and
    unsupported availability modes are refused (`unsupported_scope`), so they stay native.
  - **One gate.** All acquisition passes `downloads::enqueue`: plans, interactive grabs,
    RSS, worker searches and request retries. It refuses any target that is not adopted and
    monitored here (`not_adopted`). The explicit endpoints refuse up front.
  - **Categories.** The effective qBittorrent category must differ from every enabled
    native client category of both apps, including post-import categories
    (`category_shared` blocks execution for that media kind). The native post-import
    category is not applied.
  - **State from legacy mode.** A state already seeded in legacy mode keeps its managed rows
    and active grabs. Those never-adopted rows are reported as `legacy_managed`: they are
    refused here and block the coexistence readiness stage.
- **`legacy`**. Every imported item is managed with its native monitoring, as in earlier
  builds. Use it only for a catalog that holds just adopted titles (the selected-title pilot).

## Catalog semantics

- **Managed catalog.** The immutable import seeds the managed catalog in `state.sqlite`
  insert-only when the server starts. Later imports add new native items and refresh native
  metadata. They never override user choices (monitoring, removal, added items, a series'
  "monitor new seasons") or live evidence (scans, imports by this server).
  - **Re-imports.** Episodes and seasons that a later import adds to a series managed here
    follow that series' own rules: its flag, its season's flag, and "monitor new seasons".
    The native flags are not used, so a re-import never widens an adopted scope. The native
    flags are still recorded.
  - **Monitor new seasons.** Sonarr's `monitorNewItems` (`all` or `none`) seeds a new
    series' "monitor new seasons". It is refreshed by later imports only while this server
    does not manage the series.

  The rules below describe the snapshot evidence a new item starts with.
- **Metadata refresh (TVmaze) on imported series.** A series with imported per-episode
  evidence keeps the native (TVDB) numbering:
  - **What is added.** A refresh appends only the next episode of a season that has
    episodes, or the first episode of the season right after the latest season with
    episodes.
  - **What is refused and reported.** An episode whose title and air date match an existing
    one (renumbering), or one that aired before the latest imported episode. So is anything
    else (gaps, numbers inside the imported range). These appear in the refresh result as
    `divergence` and `skipped`.
  - **Imported episodes.** They only have missing fields filled; a differing air date is
    reported, not rewritten.
  - **Hold.** While a refresh finds any conflict, no new season is appended, and the series
    carries a durable `metadata_hold`, shown in `/attention` and `/status`. It clears only
    with a refresh that finds no conflict and lists every imported episode. An empty,
    truncated or partial response is not evidence of resolution, so a series whose provider
    lacks some native numbers keeps its hold until an operator resolves it.
- **Ownership.**
  - Sampled series: from per-episode `hasFile`.
  - Unsampled series: from season statistics. Zero files means missing. All files present
    *and* the episode count matches TVmaze means owned. Anything else means **unknown**,
    which blocks the plan.
  - Targets outside the catalog are assumed missing, with a note.
- **Episode lists.** From `episode_samples` when they cover the season, otherwise from TVmaze
  (when enabled). Without either, season plans are blocked (`episode_list_unknown`).
- **Collisions.** Duplicate TMDB/TVDB IDs mark every colliding entry as `identity_conflict`,
  and plans for them are blocked. A TVmaze ID shared by two series is reported. Entries
  without IDs, and malformed entries, are skipped with a reason. Sample keys that match no
  series are reported.
- **Provenance and freshness.** Every entry records `source_app`, `native_id`, `import_id`
  and `observed_at`. API responses repeat the observation time and catalog age. The catalog
  is evidence from the snapshot moment, not live truth.

## Not in the snapshot contract (gaps)

Native data that is not exported, and how its absence affects this build:

- **Native blocklist and history.** A release that failed natively can be planned again. This
  build keeps its own blocklist and history from the moment execution is enabled.
- **Native queue.** In-flight native downloads are invisible. In this build, a download it
  started covers its targets (`covers_queued`). A native torrent with the same hash is
  foreign and is never touched.
- **Release profiles**, if not exported. See above.
- **Import lists, notifications, metadata and auto-tagging.** No decision impact in shadow mode.
- **Prowlarr app profiles and sync.** Not used; Prowlarr `GET /api/v1/search` queries the
  mapped indexers directly.
