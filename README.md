# Arr

Arr is one small program that does the jobs of Sonarr, Radarr, Prowlarr and qBittorrent for
an existing Plex server. Point it at your Plex libraries and it learns what you already own,
then finds, downloads, verifies and imports new movies and TV into the same folders, and
asks Plex to refresh. It has no web UI: you drive it through an authenticated JSON API, or
from an AI assistant over MCP ("add season 2 of …").

Arr is young. It deliberately refuses what it does not support yet (quality upgrades,
renaming, custom formats, usenet, anime/daily numbering, specials) instead of guessing. It
can run alongside an existing Sonarr/Radarr setup, which it never modifies.

**Status: standalone-capable, shadow by default.** One binary, one `config.toml`, one private
state directory, no Sonarr, Radarr, Prowlarr or qBittorrent needed at runtime:

- **managed setup** (`arr setup` or `POST /api/v1/setup`) creates quality profiles,
  size limits, per-indexer policy with explicit seed goals, download/cleanup policy and root
  folders with an empty catalog — no Sonarr/Radarr snapshot needed;
- **native indexers**: The Pirate Bay's JSON API and generic Torznab, searched directly with
  per-indexer status, pacing, `Retry-After` backoff, limits and credential confinement;
- the **embedded BitTorrent engine** (librqbit, in this process);
- verified import into **your library** (rename off, `ffprobe` validation, hardlink/copy,
  journaled recovery), **Plex** folder refreshes, and a read-only **Plex library bootstrap**
  so already-owned titles are known before anything is requested;
- **remote MCP** at `/mcp` (Streamable HTTP, separate static bearer token) besides the stdio
  adapter.

The earlier layouts keep working unchanged: an offline import of a Sonarr/Radarr/Prowlarr
snapshot, search through `[prowlarr]`, downloads through `[qbittorrent]`, coexistence with
running native apps. The search backend (native or Prowlarr) and the download backend
(embedded or qBittorrent) are each chosen explicitly; there is never a fallback between them.

With `[execution] enabled = true` the server submits torrents, imports completed downloads,
follows the cleanup policy, reads RSS feeds and refreshes Plex. Setup never enables it.

**Start here:** [docs/STANDALONE.md](docs/STANDALONE.md) — from zero to automatic adds on a
fresh state with native providers.

## Build

Requires Rust 1.94+. `ffprobe` is an external runtime dependency (Debian/Ubuntu package
`ffmpeg`; the Docker runtime image installs it); it is never bundled or downloaded, and
`check`/`setup` report it. The tests also need `/usr/bin/ffmpeg`. The release binary links
only the system C library (glibc) dynamically; see [docs/STANDALONE.md](docs/STANDALONE.md).

```bash
cargo build --release --locked        # binary: target/release/arr
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --locked
```

In a container, mount only the private state directory and publish the port to
`127.0.0.1`. When execution is enabled in a sandbox, also mount the sandbox's download and
library directories, and nothing else.

## Run (standalone)

```bash
STATE=/srv/arr/state
arr init  --state-dir "$STATE"                 # standalone config template + API token
"$EDITOR" "$STATE/config.toml"                            # [[indexer]], [embedded] (dht = true), [library], [plex], [tmdb]
arr setup --config "$STATE/config.toml"         # managed settings + empty catalog (no execution)
arr check --config "$STATE/config.toml"         # config, secrets, ffprobe, indexer mapping
arr serve --config "$STATE/config.toml"
curl -s -H "Authorization: Bearer $(cat $STATE/secrets/api_token)" -d '{}' -H 'content-type: application/json' \
     http://127.0.0.1:8787/api/v1/setup/plex/bootstrap   # repeat until done: owned titles, not monitored
```

## Run (legacy: imported snapshot and Prowlarr)

```bash
STATE=/srv/arr/state        # private, outside this repo; created 0700
arr init --state-dir "$STATE" --prowlarr-url http://127.0.0.1:9696
# Put the Prowlarr API key in an owner-only file (no other content):
install -m 600 /dev/null "$STATE/secrets/prowlarr_api_key" && "$EDITOR" "$STATE/secrets/prowlarr_api_key"
arr import --state-dir "$STATE" --snapshot /private/native.json --observed-at 2026-10-26T06:56:00Z
arr check  --config "$STATE/config.toml"
arr serve  --config "$STATE/config.toml"     # listens on 127.0.0.1:8787

TOKEN=$(cat "$STATE/secrets/api_token")
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8787/api/v1/readiness
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8787/api/v1/media/season:tvdb:372264:1/releases
curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
     -d '{"media_id":"season:tvdb:372264:1"}' http://127.0.0.1:8787/api/v1/plans
```

All config keys are listed in `config/local.example.toml` (legacy layout) and
`config/standalone.example.toml` (standalone); the setup document in `config/setup.example.toml`. TVmaze (free) and TMDB (needs your
own credential; none is bundled) are optional. Without TMDB, movie discovery is limited to the
catalog, and movies outside it can be added only with explicit metadata
(`POST /api/v1/library {"media_id":"movie:tmdb:N","metadata":{"title":…,"year":…}}`). The
API says so.

Execution (sandbox only until verified): add `[execution] enabled = true` with `[qbittorrent]`
and `[library]` sections. Then follow
[docs/EXECUTION-SANDBOX.md](docs/EXECUTION-SANDBOX.md): diagnostics, add-and-search, queue,
cleanup, RSS, commands and restart checks. `arr fixture` generates authored
playable test releases with matching `.torrent` files.

## Documents

- [docs/STANDALONE.md](docs/STANDALONE.md): the standalone recipe, native indexers, Plex
  bootstrap, ffprobe dependency, service unit, release binary and limits.
- [docs/API.md](docs/API.md): endpoints, error codes, examples. The machine-readable schema
  is `GET /api/v1/openapi.json`.
- [docs/MCP.md](docs/MCP.md): the stdio adapter and remote Streamable HTTP MCP, and what
  they do not cover.
- [docs/EMBEDDED-DOWNLOADER.md](docs/EMBEDDED-DOWNLOADER.md): the embedded librqbit engine:
  configuration, state binding, durability and metrics, tests and limits.
- [ARCHITECTURE.md](ARCHITECTURE.md): boundaries, state layout, CLI, snapshot contract,
  identity, decision and plan semantics.
- [docs/MIGRATION.md](docs/MIGRATION.md): importing an existing Sonarr/Radarr/Prowlarr
  configuration, where Arr is stricter than they are, and gaps.
- [docs/READINESS.md](docs/READINESS.md): what works, what is only mock-tested, and what
  must be true before replacing Sonarr/Radarr.
- [docs/EXECUTION-SANDBOX.md](docs/EXECUTION-SANDBOX.md): isolated sandbox setup, API flow,
  ownership and cleanup rules, restart checks, real-client harness.
