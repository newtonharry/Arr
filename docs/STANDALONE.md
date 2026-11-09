# Standalone deployment (no Sonarr, Radarr, Prowlarr or qBittorrent)

One `arr` binary, one `config.toml`, one private state directory. Release search
goes straight to native indexers (The Pirate Bay's JSON API, or any Torznab endpoint),
downloads run in the embedded BitTorrent engine, completed downloads are validated with
`ffprobe` and imported into your library, and Plex is refreshed for the imported folder
(the whole library while it is still empty, since Plex skips folder scans there). Policy
(quality profiles, seed goals, cleanup) is created by `setup`. Nothing writes outside the
state directory until you enable execution.

Arr no longer imports Sonarr/Radarr settings: a state folder made by the removed `arr import`
is refused at startup (nothing in it is changed). Set up a new state folder with `arr init`,
`arr setup` and `arr bootstrap`. The `[prowlarr]` and `[qbittorrent]` sections still work.

## Requirements

- **A supported system:** see the [README's install section](../README.md#install) for the
  operating systems and versions the release binaries run on, or build from source
  (`cargo build --release --locked`, Rust 1.94+).
- **`ffprobe`** (FFmpeg) — an external dependency, never bundled or downloaded. Debian/Ubuntu:
  `apt install ffmpeg`; Fedora: `dnf install ffmpeg-free`; Alpine: `apk add ffmpeg`. Set
  `[library] ffprobe` if it is not `/usr/bin/ffprobe`. `arr check` and `setup`
  report it; `serve` refuses to start in execution mode without it, with the install hint.
  Media are never imported unvalidated.

## From zero to automatic downloads

```bash
STATE=/srv/arr/state           # private: created 0700, files 0600
arr init --state-dir "$STATE"  # API token + standalone config template (never overwrites)
```

1. **Edit `$STATE/config.toml`** (see [config/standalone.example.toml](../config/standalone.example.toml)):
   - `[[indexer]]`: id 1, `kind = "thepiratebay"`, `base_url = "https://apibay.org/"` (the exact
     API base you trust), and/or a `kind = "torznab"` endpoint with `api_key_file`;
   - `[embedded]`: `download_dir` (dedicated), `listen`, and **`dht = true`** — TPB magnets carry
     no trackers unless you list public `magnet_trackers`, so without the DHT (or initial
     peers) they could never find a peer. Such grabs are refused (`no_peer_discovery`) and
     readiness says `peer_discovery: blocked`; nothing is queued that cannot download.
     Torrents flagged private never use the DHT (the engine honours the flag);
   - `[library] movies` / `tv`: your library roots as this host sees them;
   - `[plex]` (optional): `base_url`, `token_file`, `movie_section`, `tv_section`, and
     `movies_path` / `tv_path` — the same roots **as Plex sees them** (for example
     `/media_library/movies` inside a container for host `/mnt/data/library/movies`);
   - `[tmdb]` with `credential_file` (movie metadata; a v4 read token or v3 key) and
     `[tvmaze] enabled = true` (free; series and full episode lists);
   - `[mcp]` (optional): remote MCP, see [MCP.md](MCP.md).
2. **Secrets, as files only** (`chmod 600`, one value per file, never in config):
   ```bash
   install -m 600 /dev/null "$STATE/secrets/plex_token"     && "$EDITOR" "$STATE/secrets/plex_token"
   install -m 600 /dev/null "$STATE/secrets/tmdb_credential" && "$EDITOR" "$STATE/secrets/tmdb_credential"
   ```
   For TMDB, `arr set --config "$STATE/config.toml" tmdb-key` does both steps: it reads the key
   from stdin, checks it with TMDB, saves it and turns on `[tmdb]` (restart Arr afterwards).
3. **Managed settings** (profiles, size limits, per-indexer policy with explicit seed goals,
   download/cleanup policy, root folders) and an empty catalog:
   ```bash
   arr setup --config "$STATE/config.toml" [--settings setup.toml] [--dry-run]
   ```
   Without `--settings` the defaults apply (see [config/setup.example.toml](../config/setup.example.toml)):
   one HD 720p/1080p profile per media kind, seed goal ratio 1.0 or 72 hours (whichever comes
   first), categories `um-movies`/`um-tv`, cleanup after a verified import and the seed goal,
   revision upgrades off, RSS every 15 minutes (only once execution is enabled). The output
   lists missing secret files and the ffprobe preflight. `setup` validates the config without
   needing secrets yet, never enables execution and never queues a search. Re-running it
   with the same document is a no-op (`unchanged`). Run again without `--settings`, it keeps
   the current document (so after adding an `[[indexer]]` it adds the new indexer and keeps
   your profiles and seed goals). A running `serve` uses new settings only after a restart.
4. **Check** (config, secret modes, ffprobe, indexer mapping): `arr check --config "$STATE/config.toml"`.
5. **Serve** (shadow mode: searches and plans only): `arr serve --config "$STATE/config.toml"`.
6. **Plex discovery and the existing library** (read-only):
   ```bash
   TOKEN=$(cat "$STATE/secrets/api_token"); API=http://127.0.0.1:8787/api/v1
   curl -s -H "Authorization: Bearer $TOKEN" $API/setup/plex      # sections, mapping status
   # Repeat until "done": true. Each call is one bounded batch; the cursor is durable.
   # Then each run's "status" is "done" ("failed" means it stopped; its issues say why).
   curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' -d '{}' $API/setup/plex/bootstrap
   ```
   (or `arr bootstrap --config "$STATE/config.toml"` before `serve`). Owned movies
   (TMDB id; IMDB-only items are resolved through Plex's metadata service, then TMDB when configured, otherwise reported
   unresolved) and owned episodes (TVDB id plus Plex's exact season/episode numbers) become
   catalog entries that are owned and **not monitored**. Ownership needs a validated local
   file: inside the mapped root, a plain relative video path, no symbolic links, present.
   Season episode lists come only from TVmaze; without it they stay unknown (nothing is
   assumed complete). A file whose name gives another title is a conflict. For a new show
   found by its exact TVDB id, the accepted names are the TVmaze title, **Plex's own title**
   when TVmaze names the show differently (e.g. English in Plex and the files, the original
   language in TVmaze), and an **official TVmaze AKA** of that show only when a file name
   matches it exactly. Both are kept as alternate titles, so later scans and decisions agree.
   Nothing is abbreviated or fuzzy-matched, and series already in the catalog are not changed
   (the bootstrap only inserts). Unresolved, unmapped, missing, unsupported (specials) and conflicting
   items are counted and listed; a missing section, unmapped path or unavailable root fails
   the run and changes nothing. Nothing is searched, monitored or downloaded. Searches still
   query the canonical (TVmaze) title.
7. **Enable execution deliberately** (`[execution] enabled = true`, or
   `arr set --config "$STATE/config.toml" downloading on`), restart, and check
   `GET /api/v1/readiness` (`search_backend`, `peer_discovery`, `execution_*`).
8. **Add what you want** (one call: add, scan the folder, search, download, import):
   ```bash
   curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
        -d '{"media_id":"movie:tmdb:603"}' $API/library
   curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
        -d '{"media_id":"season:tvdb:372264:1"}' $API/library
   ```
   An owned title gives `already_satisfied`; a season of a bootstrapped show searches only the
   missing episodes. RSS then picks up new releases of monitored items; unmet requests are
   retried with backoff.

To change one setting later (a key or token, Plex's address, downloading on or off, the
upload limit, the torrent port, error reports), use `arr set --config "$STATE/config.toml"
NAME [VALUE]`: it checks the change and changes nothing if that fails. Restart afterwards.
`arr set --help` lists the names; the README's
[`arr set` section](../README.md#changing-one-setting-by-hand-arr-set) has examples.

API-only setup is possible too: with a config but no settings, `GET /api/v1/setup` lists
what is missing and `POST /api/v1/setup {"document": {...}}` creates and activates managed
settings in the running process (no restart). Connections and credentials stay in
`config.toml`; the API manages policy only.

## Native indexers

- **The Pirate Bay** (`kind = "thepiratebay"`): `q.php?q=<keywords>&cat=<categories>` with
  spaces encoded `%20` and a literal comma list (the API answers `+`-encoded spaces with its
  "no results" row), and `precompiled/data_top100_recent.json` for RSS. Torznab movie
  categories map to TPB 201/202/207/209/210/211 (2020 adds clips/other), TV to 205/208/212
  (5050 adds handheld); a request that maps to none is refused rather than searching every
  category, and rows outside the requested categories are dropped and counted. "Season 1
  Complete" names are normalised to `S01`. The site's "no results" row is a confirmed empty;
  an empty array is reported `empty_unconfirmed`. One page of at most 100 rows: a full page is
  reported truncated. Magnets are built from the info-hash with only your `magnet_trackers`
  (public trackers; no query strings, values never echoed). Optional `uploaders` allowlist.
- **Torznab** (`kind = "torznab"`, spec 1.3): `t=caps` (cached a day; search must be
  available; the result limit is honoured), `t=search` with `offset` paging (`total`
  honoured, `max_pages` bound), typed `<error>` codes (100–102 credentials, 500/501 limits,
  910 disabled; the endpoint's description is never repeated), attributes including
  `minimumratio`/`minimumseedtime` (a release whose tracker requirement the seed goal cannot
  meet is rejected `seed_requirement_unmet`) and the volume factors (reported). The API key
  goes only to `base_url`'s origin (the spec's `apikey` parameter; same-origin download links
  without it get it); links on other origins are kept only if listed in `download_origins`
  and are fetched without the key; a link, magnet tracker, web seed or `.torrent` announce URL
  that would carry the key to any other origin is refused unless that origin is in
  `key_origins`. Downloads follow at most one redirect, to a magnet or an allowed origin.
- **Every indexer**: per-indexer concurrency and spacing (`max_concurrent`,
  `min_interval_ms`), HTTP 429 `Retry-After` backoff honoured by queued requests too, timeouts,
  bounded bodies, a result cache, optional durable `query_limit`/`grab_limit` per hour or day,
  and health in `/api/v1/diagnostics`. Searches report one status per indexer and query;
  `completeness.provider` is `complete` only when every indexer the settings name answered.

## Running it as a service

```ini
# /etc/systemd/system/arr.service
[Unit]
Description=arr
After=network-online.target
Wants=network-online.target

[Service]
User=arr
ExecStart=/usr/local/bin/arr serve --config /srv/arr/state/config.toml
Restart=on-failure
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/srv/arr /srv/media
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

The process stops gracefully on SIGTERM (the worker finishes its pass, the engine records its
counters). `RUST_LOG` controls logging (stderr). Keep the API on loopback; for anything else
set `allow_non_loopback = true` deliberately and put TLS in front.

## Release binary

```bash
cargo build --release --locked
ldd target/release/arr   # libc, libm, libgcc_s, ld-linux only
```

There is no bundle: the binary plus a system `ffprobe` is the deployment. The highest glibc
symbol version the binary needs is reported in the implementation report for the build host.

## Limits

- Specials (season 0), anime/daily/scene numbering, quality/cutoff upgrades, renaming and
  custom formats are not implemented (fail closed).
- Feeds are windows of the latest releases. A window whose oldest release is newer than the
  newest one the last sync saw (or that has no dated release) is flagged `gap_possible`,
  however many rows it has: TPB's recent feed is nominally 100 rows but answered 50 live, and
  is shared by every category (the dates span the whole window). Overlap shows only that the
  window is contiguous with the last one, not that the provider lists every release.
- The bootstrap reads Plex's view: files Plex does not know are not owned until a scan (an
  add scans the item's folder first). Movies with more than one file are recorded as owned;
  multi-file imports are still refused.
- Remote MCP uses a static bearer token, or Google OAuth sign-in when `[mcp.oauth]` is
  configured (what a ChatGPT connector needs; see [MCP.md](MCP.md)). It has no TLS listener.
