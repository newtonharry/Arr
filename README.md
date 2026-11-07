# Arr

Arr replaces Sonarr, Radarr, Prowlarr and qBittorrent with one program for your Plex server.
You ask for a movie or a season. Arr searches, downloads it with its built-in BitTorrent
engine, checks the file with ffprobe, moves it into your Plex library and tells Plex to
refresh.

Arr has no web interface. You talk to it through an AI assistant over MCP ("add season 2 of
Severance") or through its JSON API.

## Install

On the machine that runs Plex:

```sh
curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/install.sh | sh
```

The script takes a minute or two. It:

1. downloads the Arr binary for your system and checks its SHA-256 (no compiler needed);
2. installs FFmpeg if `ffprobe` is missing (it asks first);
3. finds Plex, reads its token from Plex's own settings, and reads your movie and TV
   folders from your Plex libraries;
4. writes the config, starts Arr as a service (systemd on Linux, launchd on macOS);
5. imports your Plex library, so Arr knows what you own. It downloads nothing at this step.

Arr runs on macOS and on Linux with glibc 2.35 or newer (Debian 12, Ubuntu 22.04, Raspberry
Pi OS 12 and later), on x86_64 and ARM64. On Windows, run it inside WSL2. Plex can run
natively or in Docker; when the script can see the Plex container, it maps Docker paths back
to your disk.

### Updates

The installer asks whether Arr should keep itself up to date. Say yes and Arr checks for a new
release once a day, at a random time overnight. When one appears, it downloads it, checks
its SHA-256, swaps the binary and restarts. If the new version doesn't start within 30
seconds, it puts the old one back. It only installs release builds from this repository, and
it never runs a script fetched from the internet.

To update by hand, run `arr-update` (next to the `arr` binary) or the install command again;
both keep your config. To stop automatic updates, run the installer with `--no-auto-update`.
Update logs go to `journalctl -u arr-update` (`--user` for a user install), or `update.log` in
the state folder on macOS.

### Let an AI agent install it

Every question has a flag, so an agent can install Arr without prompts. Give your agent this
README and ask it to install Arr. It needs three things from you:

- **SSH or terminal access** to the machine that runs Plex, as the user that owns your media
  folders.
- **A Plex token**, only if the script can't read it from Plex's settings. In Plex Web, open
  any movie, choose ⋯ > Get Info > View XML, and copy the value after `X-Plex-Token=`.
- **A TMDB key** (optional, free at <https://www.themoviedb.org/settings/api>). Without one,
  Arr can't find movies you don't already own. TV works without it.

The agent then runs:

```sh
curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/install.sh | sh -s -- --yes \
  --tmdb-key "$TMDB_KEY"            # add --plex-token "$PLEX_TOKEN" if asked for one
```

`--yes` accepts every default and never waits for input. A failed run ends with one `error:`
line that names the problem, usually with the flag that fixes it. Fix it and run the script
again.

With `--yes`, automatic updates stay off unless the agent also passes `--auto-update`. An agent
should ask you first: updates change the software on your machine without further prompts.

Useful flags: `--movies DIR`, `--tv DIR`, `--downloads DIR`, `--plex-url URL`, `--port N`,
`--upload-limit-kib N`, `--search-only` (set up without downloading), `--install-only`.
`install.sh --help` lists them all.

To confirm the install worked, the agent can run `arr check --config <state dir>/config.toml`;
it prints JSON with `"config": "ok"`.

The agent can also set up access from ChatGPT on your phone. You create the ngrok account and
the Google OAuth client yourself, since both need your logins, then give the agent five values.
[docs/REMOTE.md](docs/REMOTE.md#let-an-ai-agent-set-it-up) has the exact command; it passes the
secrets as environment variables so they stay out of the process list.

## Use it

### From Claude Desktop, Claude Code or another MCP client on the same machine

The installer prints this block with your paths filled in:

```json
{
  "mcpServers": {
    "arr": {
      "command": "/home/you/.local/bin/arr",
      "args": ["mcp", "--api-url", "http://127.0.0.1:8787/",
               "--token-file", "/home/you/.local/share/arr/secrets/api_token"]
    }
  }
}
```

The assistant gets seven tools: `search_titles`, `add_media`, `media_status`, `requests`,
`library`, `release_choices` and `start_download`. Ask "what's in my library" or "which
seasons of Severance do I have" and it reads your catalog, including everything Arr imported
from Plex. By default it adds what you ask for and Arr picks the release. Ask to "choose a
release" if you want to pick one yourself.

### From ChatGPT on your phone or any device

ChatGPT reaches Arr from OpenAI's servers, so Arr needs a public HTTPS address and a sign-in.
Arr uses a free [ngrok](https://ngrok.com) domain for the address (no router changes) and
Google sign-in to let in only the accounts you list. Once set up, Arr works in ChatGPT on the
web, on your phone and on every other device signed in to your ChatGPT account.

You need a free ngrok account and a Google OAuth client, about 15 minutes of clicking that
[docs/REMOTE.md](docs/REMOTE.md) walks through. Then run the installer again:

```sh
curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/install.sh | sh -s -- --remote
```

It installs ngrok, configures the sign-in, starts the tunnel as a service, checks the public
address and prints the steps to add Arr to ChatGPT.

### From the API

```sh
TOKEN=$(cat ~/.local/share/arr/secrets/api_token)
curl -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:8787/api/v1/discover?kind=series&q=severance"
```

`GET /api/v1/openapi.json` returns the full schema, and [docs/API.md](docs/API.md) has
examples.

## What Arr does, and what it refuses

Arr adds movies, whole seasons and single episodes. It checks each download against what you
asked for, refuses broken or mismatched files, picks up new episodes of seasons you added
through RSS, and stops seeding once a torrent reaches its seed goal (ratio 1.0 or 72 hours by
default). To change the goals or quality profiles, edit a copy of
[config/setup.example.toml](config/setup.example.toml) and run
`arr setup --config <config> --settings <file>`.

It refuses things it can't do safely yet: quality upgrades of files you own, renaming,
custom formats, usenet, anime and daily-show numbering, and specials (season 0). It never
overwrites a file it didn't download.

Sonarr and Radarr can keep running next to Arr. Arr ignores titles you add there, and they
ignore titles you add to Arr. If both monitor the same title, both may download it; Arr
won't replace their file, so you lose bandwidth, not media.

The default search source is The Pirate Bay's public API. You can add any Torznab indexer
(Jackett, Prowlarr) in `config.toml`. Only download what you have the right to.

## Other ways to run it

- **Your own config.** `arr init`, then edit the template; every option is in
  [config/standalone.example.toml](config/standalone.example.toml), and
  [docs/STANDALONE.md](docs/STANDALONE.md) explains each step.
- **Docker.** The [Dockerfile](Dockerfile) builds an image; mount your state directory at
  `/state` and your media folders read-write.
- **From source.** You need Rust 1.94 or newer and FFmpeg:

  ```sh
  cargo build --release --locked      # target/release/arr
  cargo test --locked                 # needs /usr/bin/ffmpeg
  ```

- **Migrating from Sonarr/Radarr, Prowlarr or qBittorrent.** Arr can import their settings
  and keep using Prowlarr or qBittorrent; see [docs/MIGRATION.md](docs/MIGRATION.md).

## Documentation

- [docs/STANDALONE.md](docs/STANDALONE.md): setting Arr up by hand, step by step.
- [docs/REMOTE.md](docs/REMOTE.md): using Arr from ChatGPT on your phone (ngrok and Google
  sign-in).
- [docs/MCP.md](docs/MCP.md): the MCP tools, remote access and Google sign-in in detail.
- [docs/API.md](docs/API.md): endpoints, errors and examples.
- [docs/EMBEDDED-DOWNLOADER.md](docs/EMBEDDED-DOWNLOADER.md): the BitTorrent engine, seeding
  and limits.
- [docs/MIGRATION.md](docs/MIGRATION.md): importing Sonarr/Radarr/Prowlarr settings.
- [docs/READINESS.md](docs/READINESS.md): what the tests cover and the known limits.
- [docs/EXECUTION-SANDBOX.md](docs/EXECUTION-SANDBOX.md): testing downloads safely in a
  sandbox.
- [ARCHITECTURE.md](ARCHITECTURE.md): how the pieces fit together.
