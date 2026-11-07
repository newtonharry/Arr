# Readiness

**Verdict:** the acquisition, import, cleanup, automation and reconciliation paths are
**implemented** and pass an automated suite against loopback mocks with **real generated
media** (ffmpeg/ffprobe). Real qBittorrent sandbox checks and a small selected-title production trial (acquisition,
import, Plex and cleanup) have passed. The default stays **shadow**
(`[execution] enabled = false`).
This build is **not** a replacement for Sonarr or Radarr; replacement needs sandbox and
production evidence plus an explicit user decision. Keep Sonarr, Radarr, Prowlarr,
qBittorrent and Plex unchanged.

`GET /api/v1/readiness` keeps service readiness (`ready`: state and import loaded) separate
from **migration readiness**. `migration.stages` computes three stages from the
configuration, the active import, the acquisition gates, ownership counts and
operator-recorded evidence:

| stage | scope | blocked by (examples) |
|---|---|---|
| `limited` | selected titles adopted one by one | no import, execution off, policy blockers, no usable media kind |
| `coexistence` | the full native catalog imported, only adopted scopes acquired | legacy mode, `category_shared`, `legacy_managed` items |
| `broad` | most or all titles adopted, native apps as a suspended fallback | RSS off, TMDB or TVmaze missing, nothing adopted, no recorded user decision |

How the stages are computed:

- **Pending verification.** Each stage lists every capability *in use* (implemented and
  configured) that lacks operator-recorded `production` evidence. Unused capabilities, such
  as rename or quality upgrades, are listed as accepted limitations, not demanded.
- **Status and `ready`.** A stage's `status` is `blocked`, `pending_verification` or
  `prerequisites_met`. `ready` is true only in the last case.
- **What `ready` does not cover.** Even then, `external_requirements` state what this server
  cannot observe: native monitoring, native queues, and native categories changed after the
  snapshot.

Record only what was actually observed. Rare paths such as failed-download handling and
revision upgrades have independent real-client **sandbox** proof; they stay `sandbox`
(and so `pending_verification` for production) until production observes them. Do not
relabel them to turn a stage green. A staged migration can proceed per scope with stages
still pending; the decision flag is the user's alone.

Evidence comes from `STATE/verification.toml`, as recorded by the operator. It is a record,
not a live check:

```toml
[verified]
download_submission = "production"   # only after observing it
download_cleanup = "production"
[decisions]
broad_migration = "approved"         # the explicit user decision
```

`GET /api/v1/capabilities` gives `implemented` / `configured` / `verified` for each
capability. Verification is `unit_tests` or `loopback_mocks` from this build's tests, or
`sandbox`/`production` from that file.

## What works (automated tests)

- **Shadow features from earlier phases** (unchanged and still tested):
  - authenticated API with limits and typed errors;
  - atomic, content-addressed private imports;
  - Prowlarr search with machine-readable completeness;
  - adversarial decisions;
  - durable idempotent plans;
  - TVmaze/TMDB discovery;
  - state schema 1 → 3 and 2 → 3 migrations that keep plans and keys.
- **Managed catalog** (`tests/library.rs`):
  - seeded insert-only from imports;
  - add with automatic profile and root, explicit operator metadata when TMDB is missing,
    TVmaze series;
  - monitor and unmonitor at every level; removal keeps files;
  - user choices survive a restart and a re-import;
  - wanted and calendar views.
- **qBittorrent** (`tests/execution.rs`):
  - a 4.3.9 (WebAPI 2.8.2) mock that records any parameter 4.3.9 does not know (none are
    sent), and 5.x (`stop`/`start`, `stopped`);
  - cookie login and a single re-login after 403;
  - torrent-file and magnet adds with the category, the `unified-media` tag and seed limits;
  - ambiguous adds reconciled by hash, including across a restart (no duplicate);
  - foreign torrents never touched;
  - diagnostics.
- **End-to-end** with real playable MKVs: search → grab → submit → complete →
  ffprobe-validated hardlink import under the native scene name → source kept while seeding
  → removed only after the share limit and the seed goal are met → library file still
  playable → Plex section refresh → plan state `imported`.
- **Import safety** (`tests/ingest.rs`):
  - truncated, junk and audio-only media fail, are blocklisted and are never owned;
  - native sample durations;
  - symlinked sources refused; existing destinations never overwritten (retry after the user
    resolves it);
  - free-space minimum; copy fallback re-probed;
  - season packs with exact multi-episode ownership.
- **Native revision upgrades**:
  - exact-quality PROPER replaces only the managed file, through a journaled backup;
  - ordinary quality upgrades, same-revision releases and same-group different-quality
    propers are refused;
  - an unknown revision fails closed;
  - import re-checks the rule against changed evidence;
  - crash recovery restores the original byte for byte and rolls back staged or published
    files.
- **Cleanup** (`tests/cleanup.rs`):
  - unmet, unlimited or unknown share limits keep the source;
  - global and per-torrent limits and the indexer seed goal are evaluated;
  - foreign, moved and symlinked downloads are refused;
  - restart at both removal boundaries stays `imported` and is never re-downloaded;
  - disabled flags and the kill switch are honoured;
  - failed-download cleanup keeps the blocklist and the re-search choice;
  - explicit removal needs the seed goal or an override.
- **Failures** (`tests/failures.rs`):
  - client errors and external removal are classified after grace periods; category
    changes release the download;
  - a client outage changes nothing;
  - cancel blocklists on request and holds automatic searches for 24 h;
  - explicit requests retry with backoff, outages do not count as attempts, and a request is
    satisfied when a release appears.
- **RSS** (`tests/rss.rs`):
  - a namespaced Torznab feed through the configured Prowlarr origin;
  - only managed, monitored, wanted items are grabbed; untracked, unmonitored, wrong-season
    and wrong-category items are ignored;
  - replays and restarts never grab twice;
  - a 1501-episode backlog is never searched on its own while the new episode is grabbed;
    an explicit backlog search runs in bounded batches;
  - explicit season searches still find packs;
  - newly refreshed metadata becomes matchable;
  - outages back off without advancing state;
  - DTD/XXE and off-origin links are refused; `gap_possible` is flagged; no link appears in
    responses.
- **Library scan** (`tests/library.rs`): healthy roots reconcile ownership with lazy probes.
  Unavailable, empty-with-owned-items and scheduled mass-missing roots change nothing; an
  explicit rescan applies. Unmatched folders are reported.

## Cutover blockers (all must be evidenced before any replacement decision)

1. Every migration stage in `GET /api/v1/readiness` holds. That means no configuration
   blockers, and production evidence recorded for the capabilities in use, including
   `coexistence_guard`: a full-catalog instance with execution on, observed to acquire only
   adopted scopes.
2. Workflow evidence for the adopted scopes: acquisition, import, Plex, normal cleanup and
   recovery, plus RSS if it is enabled. Explicit regressions and actual workflow
   observations are the evidence, not elapsed time.
3. Unsupported native features stay fail-closed: quality/cutoff upgrades, renaming, custom
   formats, tag routing, usenet, anime/daily/scene numbering, specials.
4. An explicit user decision, recorded as `[decisions] broad_migration = "approved"`.

## Known risks and limits

- The release-title parser is heuristic. Ambiguity is rejected, so the risk is
  under-approval.
- RSS covers a window of the latest releases per indexer. Releases published between syncs
  beyond that window are missed. A window that does not overlap the last sync's newest release
  is flagged `gap_possible` (by publish dates, not by row count), and it never triggers a
  backlog search on its own.
- A healthy scan of a root marks items without files missing (native behaviour). Empty roots
  and scheduled mass changes are held back, but a wrongly mapped non-empty root still needs
  operator care: check `GET /api/v1/library/roots` and diagnostics before enabling execution.
- Prowlarr does not distinguish an indexer that failed from one that returned no results
  (`completeness.provider: unverified`).
- A torrent removed from qBittorrent after import leaves its data alone (`client_removed`).
  Cleanup never deletes what it cannot verify.
