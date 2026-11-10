# Arr

Website: <https://newtonharry.github.io/Arr/>

Arr turns a computer at home into a complete movie and TV setup, in one program. You ask for
a movie or a season. Arr searches, downloads it with its built-in BitTorrent engine, checks the
file with ffprobe, files it into your Plex library and tells Plex to refresh, so it's ready on
your TV. It replaces the usual Sonarr, Radarr, Prowlarr and qBittorrent stack. New to Plex? On
Linux the setup page installs it for you, links it to your Plex account and creates your
libraries; already run Plex? Arr fits into your existing libraries.

You use Arr from its web app (search, tap Add, and follow it until it's in Plex), through an
AI assistant over MCP ("add season 2 of Severance"), or through its JSON API.

## Install

On the machine that runs Plex:

```sh
curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/install.sh | sh
```

The script takes a few minutes. It:

1. downloads the Arr binary for your system and checks its SHA-256 (no compiler needed);
2. installs FFmpeg if `ffprobe` is missing (it asks first; on a Mac without Homebrew it offers
   to install Homebrew first, which asks for your password);
3. opens a setup page in your web browser. You sign in to Plex there, and Arr finds your
   Plex on this computer, your movie and TV libraries and their folders (Arr moves files
   into Plex's folders, so it runs on the same computer as Plex). You confirm the folders,
   choose where downloads go and whether Arr installs its own updates, then click
   **Finish setup**. No Plex yet? The page offers **Install Plex for me**, then links the new
   Plex to your account and creates your Movies and TV Shows libraries:
   - Linux (apt or dnf): your computer asks for your password, and Arr installs Plex Media
     Server from Plex's own package source and makes `/srv/media/Movies` and
     `/srv/media/TV Shows` (yours, and readable by Plex);
   - macOS: Arr downloads Plex from plex.tv (checked), puts it in Applications, starts it and
     starts it at every log-in, and makes `~/Media/Movies` and `~/Media/TV Shows`;
   - Windows: see below;
4. imports your Plex library, so Arr knows what you own. It downloads nothing at this step;
5. starts Arr as a service (systemd on Linux, launchd on macOS), and can start it when the
   computer starts rather than when you log in (see
   [Starting when the computer starts](#starting-when-the-computer-starts));
6. offers to set up access from ChatGPT on your phone (see below; you can say no and do it
   later).

If you're connected to the machine over SSH, the script prints a link to open on the computer
you're typing on. The link includes a secret code, so nobody else on your network can use
the page. To answer the questions in the terminal instead, add `--terminal`:
`curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/install.sh | sh -s -- --terminal`.

Arr saves its setup only when setup has fully worked. If it stops halfway, nothing is kept:
run the install command again to start over.

Arr runs on macOS and on Linux with glibc 2.35 or newer (Debian 12, Ubuntu 22.04, Raspberry
Pi OS 12 and later), on x86_64 and ARM64. Plex can run natively or in Docker; when the script
can see the Plex container, it maps Docker paths back to your disk.

### Windows

On the PC that runs Plex, open PowerShell (Start menu, type "PowerShell") and paste:

```powershell
irm https://raw.githubusercontent.com/newtonharry/Arr/main/install.ps1 | iex
```

It needs no administrator rights, except to start Arr when the computer starts (see
[Starting when the computer starts](#starting-when-the-computer-starts)). It installs FFmpeg
with `winget` if it's missing (it asks first), opens the same setup page, and starts Arr in the
background whenever you log in, as Plex itself does on Windows, or at boot if you choose that.
Arr lives in `%LOCALAPPDATA%\Programs\Arr`, its config and data
in `%LOCALAPPDATA%\Arr` (only your account can open it), and its log in
`%LOCALAPPDATA%\Arr\arr.log`. Windows Firewall may ask whether `arr-background` may accept
connections: allowing it lets other torrent peers reach you, which makes downloads faster.
Options are environment variables, listed at the top of
[`install.ps1`](install.ps1). At the end it offers to set up access from ChatGPT on your phone
(see below).

### Starting when the computer starts

After a power cut a Plex box often sits at its login screen. Arr can start before anyone logs
in, with its tunnel (and the daily update job), on every system:

- **Linux, installed with `sudo`:** always. The systemd service starts at boot.
- **Linux, installed without `sudo`:** the installer turns on lingering (`loginctl enable-linger`),
  which lets your user services start at boot and keep running after you log out. It tries
  without a password; at a terminal it then runs `sudo loginctl enable-linger <you>` (one
  password prompt); with nobody to ask it prints that command.
- **macOS:** the installer asks "Start Arr when this computer starts, even before anyone logs
  in?" (default yes). Yes installs launch daemons in `/Library/LaunchDaemons` with the same names
  as the login items; they run as you, with your files, and the login items are removed so two
  copies never fight over the port. It needs your admin password once. If FileVault is on, the
  Mac still waits at its unlock screen after a power cut; Arr starts as soon as it is unlocked.
  Media on an external drive or in Desktop, Documents or Downloads may need `arr` added under
  System Settings, Privacy & Security, Full Disk Access.
- **Windows:** the installer asks the same question (default yes). Yes re-registers the **Arr**,
  **Arr tunnel** and **Arr update** tasks to also start at boot and to run whether or not you're
  logged on (no password stored; they still run as you, not as an administrator). Windows asks
  permission once (a UAC prompt), or run the installer from a PowerShell opened as
  administrator. It has to be your own account that is an administrator: a standard user who
  approves the prompt with another administrator's password stays on start-at-log-in, and the
  installer says so. Windows gives a task like that no network credentials, so media on a network
  drive mapped to a letter (`Z:`) isn't reachable until you log in: use a local drive.

For scripts: `--at-boot` or `--no-at-boot` (`install.sh`), `$env:ARR_AT_BOOT = '1'` or `'0'`
(`install.ps1`; PowerShell's `irm | iex` can't take flags). Without a flag the question is asked
only when someone is at the terminal. An install run with nobody to ask (`--yes` without
`--at-boot`, no terminal, and the daily update job) never asks for a password: it keeps the
kind of install you have, leaves its service definitions as they are if changing them would need
an administrator, and still swaps the binary, restarts Arr, checks it answers and goes back to
the old version if it doesn't. To change the kind later, run the installer again with the flag;
switching back from boot to log-in also needs the password.

An older Arr's tools don't know about boot services, and are safe to leave in place until you
run the installer again (it rewrites the updater every time). On a Mac, an updater from before
this existed can't restart a system daemon: it swaps the binary, sees the new version never
answered, and puts the old one back, so Arr keeps running but doesn't update. An old
`install.sh` run against a start-at-boot Mac adds a login item next to the daemon; the two fight
over the port until the next run of the current installer, which removes the login item. On
Windows, an old updater works on the boot tasks as they are (you can start and stop them). An
old installer registers the Arr task at log-on again: either that works and the install goes
back to starting at log-in, or Windows refuses and the installer stops with an error while Arr
keeps running. Run the current installer again to start at boot.

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

### Something's wrong, or you want to change a setting

Run the install command again. It updates Arr, checks and repairs it, and keeps your setup.
Settings you leave out stay as they are; a setting you give is applied, or refused before
anything changes. On a re-run the installer:

- finds the Arr that's already installed. If you ran it with `sudo` and Arr was installed
  without, or the other way round, it stops and tells you which. On a Mac, never use `sudo`;
- checks Arr's config with the new version (`arr check`) before restarting anything. If the
  config has a problem, it says what and stops; Arr keeps running as it was;
- checks that Plex still accepts Arr's sign-in. Plex stops accepting it after "sign out of all
  devices" or a password change; at a terminal, the installer walks you through signing in
  again;
- keeps the previous version of Arr, and puts it back if the new one doesn't start;
- removes the tunnel if remote access is no longer in the config, and on Linux and macOS makes
  Arr's folder and secrets private again.

These options change an existing install: `--tmdb-key`, `--plex-token`, `--plex-url`,
`--search-only` (turns downloading off), `--upload-limit-kib`, `--torrent-port`,
`--error-reports` or `--no-error-reports`, and `--auto-update` or `--no-auto-update`. On
Windows, set `ARR_TMDB_KEY`, `ARR_UPLOAD_LIMIT_KIB`, `ARR_TORRENT_PORT`, `ARR_ERROR_REPORTS` or
`ARR_AUTO_UPDATE` before the install line.

`--port`, `--movies`, `--tv`, `--downloads`, `--movie-section`, `--tv-section` and `--terminal`
apply only to the first setup, as do the remote access values once remote access is set up. A
re-run with a different value stops with an `error:` line (at a terminal it warns and carries
on without it; on Windows it warns and ignores it).

#### Changing one setting by hand: `arr set`

`arr set` changes one setting. It checks the change first (keys, tokens and Plex's address are
tried for real) and changes nothing if that fails. Run it as the user Arr runs as; the
installer printed the config's path as `Config:`.

```sh
arr set --config ~/.local/share/arr/config.toml downloading on
arr set --config ~/.local/share/arr/config.toml upload-limit-kib 2048
printf %s YOUR_KEY | arr set --config ~/.local/share/arr/config.toml tmdb-key
```

| Setting | Value |
| --- | --- |
| `tmdb-key`, `plex-token` | read from stdin; at a terminal, paste it when asked |
| `plex-url` | Plex's address, like `http://127.0.0.1:32400` |
| `downloading` | `on` or `off` |
| `upload-limit-kib` | KiB/s for all torrents; `0` for no limit |
| `torrent-port` | 1 to 65535 |
| `error-reports` | `on` or `off` |
| `api-token`, `mcp-token` | no value: makes a new random token (update anything that uses it) |

Plex stopped accepting Arr's sign-in? `arr plex-signin --config <config>` prints a plex.tv
link; sign in, and it replaces only Arr's saved Plex token. `--check` only says whether Plex
still accepts it.

#### Restarting Arr

`arr set`, `arr plex-signin` and `arr setup` take effect when Arr restarts. Running the install
command again restarts it too, or:

- Linux, installed with `sudo`: `sudo systemctl restart arr`
- Linux, installed without: `systemctl --user restart arr`
- macOS: `launchctl kickstart -k gui/$(id -u)/io.github.newtonharry.arr`; if Arr starts at boot
  it is a system daemon that runs as you, so end your own process instead and launchd starts it
  again: `pkill -u "$(id -u)" -f 'arr serve'` (or `sudo launchctl kickstart -k system/io.github.newtonharry.arr`)
- Windows, in PowerShell: `Stop-ScheduledTask Arr; Start-ScheduledTask Arr` (also when Arr starts at boot)

#### Changing library folders, Plex libraries or Arr's port

These can't be changed in place. Arr's catalog remembers where every file is, and the port is
in your MCP client settings, the updater and the tunnel. To change them, remove Arr with its
data and set it up again:

```sh
curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/uninstall.sh | sh -s -- --arr --delete-data
```

On Windows, run `$env:ARR_REMOVE = 'arr'; $env:ARR_DELETE_DATA = '1'` in PowerShell before
the [`uninstall.ps1`](uninstall.ps1) line. This deletes Arr's config, secrets and database, never
your movie and TV files. Then run the install command again; setup reads your Plex library
again, so Arr still knows what you own.

### Movies come from Plex; a TMDB key is an optional backup

Arr finds movies through your Plex sign-in, so there is nothing extra to set up. If you want a
second source, give Arr a free key from TMDB, the movie database. Arr then uses it whenever
Plex can't answer, and prefers its original languages, other titles and release dates. If you
had a key before, it stays as that backup. [docs/TMDB.md](docs/TMDB.md) has the steps.

To add a key later:

```sh
curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/install.sh | sh -s -- --tmdb-key YOUR_KEY
```

On Windows, run `$env:ARR_TMDB_KEY = 'YOUR_KEY'` in PowerShell before the install line. A key
TMDB doesn't accept changes nothing. Without the installer, use
`arr set --config <config> tmdb-key` and [restart Arr](#restarting-arr).

### Error reports

Setup also asks whether Arr may send error reports when something fails (off unless you tick
it). Before a report leaves your computer, Arr removes paths, names, titles, addresses, host
names and anything that looks like a password or token. Nothing about your library is sent.
Reports go to Arr's Sentry project in the EU. [docs/PRIVACY.md](docs/PRIVACY.md) lists exactly
what's sent. In a terminal install, `--error-reports` or `--no-error-reports` answers the
question. To change your answer later, run `arr set --config <config> error-reports off` (or
`on`), then [restart Arr](#restarting-arr).

### Uninstall

To remove Arr, Plex Media Server, or both:

```sh
curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/uninstall.sh | sh
```

It shows what it found, asks what to remove and whether to delete each one's data (Arr's
config, secrets and database; Plex's library database, watch history and settings), and asks
once more before changing anything. It never deletes your movie and TV files or the download
folder. Plex is removed however it was installed: the system package, snap, a Docker container
or the macOS app. Add `--dry-run` to see what it would do, or `--yes --all --delete-data` to
remove everything without questions (`--help` lists the options).

### Let an AI agent install it

Every question has a flag, so an agent can install Arr without prompts or the setup page. Give your agent this
README and ask it to install Arr. It needs three things from you:

- **SSH or terminal access** to the machine that runs Plex, as the user that owns your media
  folders.
- **A Plex token**, only if the script can't read it from Plex's settings. In Plex Web, open
  any movie, choose ⋯ > Get Info > View XML, and copy the value after `X-Plex-Token=`.
- **Nothing else for movies:** they come from the Plex sign-in. A TMDB key is only an optional
  backup (see [docs/TMDB.md](docs/TMDB.md)); don't ask the user for one.

The agent then runs:

```sh
curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/install.sh | sh -s -- --yes
                                    # add --plex-token "$PLEX_TOKEN" if asked for one
```

`--yes` accepts every default and never waits for input. A failed run ends with one `error:`
line that names the problem, usually with the flag that fixes it. Fix it and run the script
again. On an install that's already set up, a re-run keeps everything you leave out and applies
or refuses each option you give, before changing anything; folders, Plex sections and `--port`
apply only to the first setup (see
[Something's wrong, or you want to change a setting](#somethings-wrong-or-you-want-to-change-a-setting)).

With `--yes`, automatic updates stay off unless the agent also passes `--auto-update`. An agent
should ask you first: updates change the software on your machine without further prompts.

Useful flags: `--movies DIR`, `--tv DIR`, `--downloads DIR`, `--plex-url URL`, `--port N`,
`--upload-limit-kib N`, `--search-only` (don't download; `arr set ... downloading on` turns it
on later), `--install-only`, `--at-boot` / `--no-at-boot` (without a terminal the installer
never asks for a password, so on a Mac `--at-boot` is what lets it use `sudo`: tell the user).
`install.sh --help` lists them all.

To confirm the install worked, the agent can run `arr check --config <state dir>/config.toml`;
it prints JSON with `"config": "ok"`.

The agent can also set up access from ChatGPT on your phone. You create the ngrok account and
the Google OAuth client yourself, since both need your logins, then give the agent five values.
[docs/REMOTE.md](docs/REMOTE.md#let-an-ai-agent-set-it-up) has the exact command; it passes the
secrets as environment variables so they stay out of the process list.

## Use it

### In your web browser

Arr serves a small web app, made for phones: search for a movie or show and tap **Add** (a
show gets its latest season and every new episode), then follow it on the home screen until
it's in Plex. It also lists what's coming up, ideas from shows with new episodes this week (with IMDb and Rotten Tomatoes scores from Plex and a quick verdict, or their TVmaze viewer rating when Plex has none),
and your library.

- **With remote access** (below): open your ngrok address, for example
  `https://quiet-river-1234.ngrok-free.app`, on any device and sign in with Google. Only the
  accounts you listed get in.
- **On the computer that runs Arr**, without remote access: run
  `arr app-link --config ~/.local/share/arr/config.toml` and open the link it prints. A link
  works once, within 15 minutes; anyone who has it can sign in, so share it only with people
  you'd give Arr to.

A sign-in lasts 30 days. **Sign out** is at the bottom of the app.

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
seasons of Severance do I have" and it reads your catalog, including everything Arr read
from Plex. By default it adds what you ask for and Arr picks the release. Ask to "choose a
release" if you want to pick one yourself.

### From ChatGPT on your phone or any device

ChatGPT reaches Arr from OpenAI's servers, so Arr needs a public HTTPS address and a sign-in.
Arr uses a free [ngrok](https://ngrok.com) domain for the address (no router changes) and
Google sign-in to let in only the accounts you list. Once set up, Arr works in ChatGPT on the
web, on your phone and on every other device signed in to your ChatGPT account.

You need a free ngrok account and a Google OAuth client, about 15 minutes of clicking that
[docs/REMOTE.md](docs/REMOTE.md) walks through. A first install offers it at the end; to add
it later, run the installer again:

```sh
curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/install.sh | sh -s -- --remote
```

On Windows, in PowerShell:

```powershell
$env:ARR_REMOTE = '1'; irm https://raw.githubusercontent.com/newtonharry/Arr/main/install.ps1 | iex
```

It asks for each value in turn, opening the ngrok and Google pages where you find them and
saying what to copy; secrets you paste stay hidden. Then it installs ngrok, configures the sign-in, starts the tunnel in the background (a service on
Linux, a launchd agent on macOS, a log-on task on Windows), checks the public address and
prints the steps to add Arr to ChatGPT.

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
[config/setup.example.toml](config/setup.example.toml), run
`arr setup --config <config> --settings <file>`, then [restart Arr](#restarting-arr).

It refuses things it can't do safely yet: quality upgrades of files you own, renaming,
custom formats, usenet, anime and daily-show numbering, and specials (season 0). It never
overwrites a file it didn't download.

Sonarr and Radarr can keep running next to Arr. Arr ignores titles you add there, and they
ignore titles you add to Arr. If both monitor the same title, both may download it; Arr
won't replace their file, so you lose bandwidth, not media.

The default search source is The Pirate Bay's public API. You can add any Torznab indexer
(Jackett, Prowlarr) as an `[[indexer]]` in `config.toml`
([config/standalone.example.toml](config/standalone.example.toml) has one). Arr uses it once
you run `arr setup --config <config>`, then [restart Arr](#restarting-arr). Without
`--settings`, `arr setup` keeps your current profiles and seed goals; if you gave it a settings
file that lists `[[indexers]]`, add the new one there too. Only download what you have the
right to.

## Other ways to run it

- **Your own config.** `arr init`, then edit the template;
  [config/standalone.example.toml](config/standalone.example.toml) shows the common options, and
  [docs/STANDALONE.md](docs/STANDALONE.md) explains each step.
- **Docker.** The [Dockerfile](Dockerfile) builds an image; mount your state directory at
  `/state` and your media folders read-write.
- **From source.** You need Rust 1.94 or newer and FFmpeg:

  ```sh
  cargo build --release --locked      # target/release/arr
  cargo test --locked                 # needs /usr/bin/ffmpeg
  ```

  The integration tests run the default stack (managed setup, a native Torznab indexer) with
  an in-memory stand-in for the download engine. It lives behind the `test-client` cargo
  feature, which `cargo test` switches on through a dev-dependency of the crate on itself; a
  release build never contains it and rejects the `[test_client]` config section it uses.

- **Coming from Sonarr/Radarr.** Arr no longer imports their settings or titles. Set Arr up
  fresh (`arr setup`), then let it read what Plex already has (`arr bootstrap`). A state
  folder made by the old importer is refused at startup; make a new one.

## Documentation

- [docs/STANDALONE.md](docs/STANDALONE.md): setting Arr up by hand, step by step.
- [docs/REMOTE.md](docs/REMOTE.md): using Arr from ChatGPT on your phone (ngrok and Google
  sign-in).
- [docs/MCP.md](docs/MCP.md): the MCP tools, remote access and Google sign-in in detail.
- [docs/API.md](docs/API.md): endpoints, errors and examples.
- [docs/EMBEDDED-DOWNLOADER.md](docs/EMBEDDED-DOWNLOADER.md): the BitTorrent engine, seeding
  and limits.
- [docs/TMDB.md](docs/TMDB.md): getting a free TMDB key, so Arr can find new movies.
- [docs/PRIVACY.md](docs/PRIVACY.md): what Arr sends (nothing, unless you allow error
  reports) and what's removed first.
- [docs/EXECUTION-SANDBOX.md](docs/EXECUTION-SANDBOX.md): testing downloads safely in a
  sandbox.
- [ARCHITECTURE.md](ARCHITECTURE.md): how the pieces fit together.

## Credits

Movie details come from Plex's metadata service and, if you add a key, from
[TMDB](https://www.themoviedb.org); TV details come from [TVmaze](https://www.tvmaze.com).
Arr uses TMDB and the TMDB APIs but is not endorsed, certified or otherwise approved by TMDB.
