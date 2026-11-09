# Migration report `imp-433458ad4f62827b-s1`

- snapshot sha256: `5f8c839af68a26a8751881dd880d0deef23a4a7ba9341713fc9483d4dd8cb71b` (1300 bytes)
- observed at: 2026-11-09T02:50:47Z / imported at: 2026-11-09T02:50:47Z
- tool version: 0.9.0

## Summary

```json
{
  "profiles": 2,
  "size_limits": 58,
  "indexers_usable": 2,
  "indexers_unusable": 0,
  "download_clients": 2,
  "root_folders": 2,
  "movies": 0,
  "series": 0,
  "series_with_episode_samples": 0,
  "catalog_skipped": 0,
  "catalog_identity_conflicts": 0,
  "blocking_issues": 0,
  "unsupported_items": 0,
  "not_implemented_items": 0,
  "credential_entries": 0
}
```

## Blocking issues (0)

None: no imported policy blocks plans.


## Settings

| app | area | setting | status | effect |
|---|---|---|---|---|
| setup | movies | root folder | enforced | new titles are filed here (Root/Folder) |
| setup | tv | root folder | enforced | new titles are filed here (Root/Folder) |
| setup | profile setup:movies-hd | tiers | enforced | releases must match an allowed quality; a higher tier ranks first; upgrades of owned files are not performed |
| setup | profile setup:movies-hd | language | enforced | language rule for releases and payload names |
| setup | profile setup:tv-hd | tiers | enforced | releases must match an allowed quality; a higher tier ranks first; upgrades of owned files are not performed |
| setup | profile setup:tv-hd | language | enforced | language rule for releases and payload names |
| setup | size_limits | MiB per minute of runtime | enforced | size must fall within min/max x runtime (x episodes for TV); an unknown runtime with a minimum rejects the release |
| setup | radarr | release policy | enforced | decisions, delay, revision handling, failed-download re-search and RSS interval |
| setup | radarr | file import | enforced | completed-download import: hardlink when possible, free-space floor, safe names (rename off) |
| setup | radarr | download policy | enforced | applied through the configured download client ([embedded] or [qbittorrent]); cleanup waits for the seed goal and share limit |
| setup | sonarr | release policy | enforced | decisions, delay, revision handling, failed-download re-search and RSS interval |
| setup | sonarr | file import | enforced | completed-download import: hardlink when possible, free-space floor, safe names (rename off) |
| setup | sonarr | download policy | enforced | applied through the configured download client ([embedded] or [qbittorrent]); cleanup waits for the seed goal and share limit |
| setup | indexer[1] The Pirate Bay | radarr | enforced | searched with these categories; priority and minimum seeders applied; seed goal: ratio or seeding time, whichever comes first (negative = the client's limit) |
| setup | indexer[1] The Pirate Bay | sonarr | enforced | searched with these categories; priority and minimum seeders applied; seed goal: ratio or seeding time, whichever comes first (negative = the client's limit) |

## Credentials detected (values not copied)


## Catalog

- managed setup: the catalog starts empty; titles are added with POST /api/v1/library

## Cutover blockers

