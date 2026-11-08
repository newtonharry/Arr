#!/bin/sh
# Arr installer: downloads a prebuilt binary, then sets Arr up next to your Plex server.
#
#   curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/install.sh | sh
#
# Every question has a flag, so scripts and AI agents can run it without prompts:
#
#   curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/install.sh | sh -s -- --yes \
#     --plex-token TOKEN --tmdb-key KEY
#
# Run with --help for all options. Nothing here needs a compiler.
set -eu

REPO="newtonharry/Arr"
VERSION="${ARR_VERSION:-latest}"
DOWNLOAD_BASE="${ARR_DOWNLOAD_BASE:-}"
BIN_DIR="${ARR_BIN_DIR:-}"
STATE_DIR="${ARR_STATE_DIR:-}"
PLEX_URL="${ARR_PLEX_URL:-http://127.0.0.1:32400}"
PLEX_URL_SET=0; [ -z "${ARR_PLEX_URL:-}" ] || PLEX_URL_SET=1
PLEX_TOKEN="${ARR_PLEX_TOKEN:-}"
TMDB_KEY="${ARR_TMDB_KEY:-}"
MOVIES="${ARR_MOVIES:-}"
TV="${ARR_TV:-}"
DOWNLOADS="${ARR_DOWNLOADS:-}"
MOVIE_SECTION="${ARR_MOVIE_SECTION:-}"
TV_SECTION="${ARR_TV_SECTION:-}"
PORT="${ARR_PORT:-8787}"
TORRENT_PORT="${ARR_TORRENT_PORT:-50413}"
UPLOAD_LIMIT_KIB="${ARR_UPLOAD_LIMIT_KIB:-0}"
YES="${ARR_YES:-0}"
REMOTE="${ARR_REMOTE:-0}"
NGROK_TOKEN="${ARR_NGROK_TOKEN:-}"
NGROK_DOMAIN="${ARR_NGROK_DOMAIN:-}"
GOOGLE_ID="${ARR_GOOGLE_CLIENT_ID:-}"
GOOGLE_SECRET="${ARR_GOOGLE_CLIENT_SECRET:-}"
ALLOW_EMAILS="${ARR_ALLOW_EMAILS:-}"
AUTO_UPDATE="${ARR_AUTO_UPDATE:-}"
ERROR_REPORTS="${ARR_ERROR_REPORTS:-}"
SETUP=1
TERMINAL=0
SERVICE=1
DOWNLOADING=1

usage() {
	cat <<'EOF'
Install Arr and set it up next to your Plex server.

Usage: install.sh [options]

  --yes                  answer every question with its default (no prompts)
  --terminal             answer setup questions here instead of on a web page
  --install-only         install the binary and stop (no setup)
  --version vX.Y.Z       install a specific release (default: latest)
  --bin-dir DIR          where to put the arr binary
  --state-dir DIR        where Arr keeps its config and data
  --plex-url URL         Plex server address (default: http://127.0.0.1:32400)
  --plex-token TOKEN     Plex token (default: read from Plex's Preferences.xml if possible)
  --movies DIR           movie library folder as this machine sees it
  --tv DIR               TV library folder as this machine sees it
  --movie-section N      Plex movie library number (default: the first one)
  --tv-section N         Plex TV library number (default: the first one)
  --downloads DIR        where torrents download before import
  --tmdb-key KEY         TMDB API key or read token, to find movies you don't own yet
  --port N               API port (default: 8787)
  --torrent-port N       BitTorrent port (default: 50413)
  --upload-limit-kib N   upload limit in KiB/s for all torrents (default: 0 = none)
  --search-only          set Arr up without letting it download anything yet
  --no-service           don't install a background service
  --auto-update          install new releases automatically (a daily check); asked if not given
  --no-auto-update       don't, and turn automatic updates off if they were on
  --error-reports        send error reports, scrubbed of personal data (docs/PRIVACY.md);
                         asked in the terminal setup if not given (the web setup has a box)
  --no-error-reports     don't

Use Arr from ChatGPT on your phone or any device (docs/REMOTE.md). Run again with these to
add it to an existing install:

  --remote                     set up a public HTTPS address (ngrok) and Google sign-in
  --ngrok-token TOKEN          your ngrok authtoken (dashboard.ngrok.com)
  --ngrok-domain NAME          your free ngrok domain, e.g. name.ngrok-free.app
  --google-client-id ID        Google OAuth client ID (Web application)
  --google-client-secret SECRET
  --allow-email ADDRESS        a Google account allowed to use Arr; repeat or comma-separate

Each option can also come from an environment variable: ARR_PLEX_TOKEN, ARR_MOVIES,
ARR_NGROK_TOKEN, ARR_GOOGLE_CLIENT_SECRET, ARR_ALLOW_EMAILS, ...
EOF
}

while [ $# -gt 0 ]; do
	case "$1" in
	--yes | -y) YES=1 ;;
	--install-only) SETUP=0 ;;
	--terminal) TERMINAL=1 ;;
	--no-service) SERVICE=0 ;;
	--search-only) DOWNLOADING=0 ;;
	--version) VERSION="$2"; shift ;;
	--bin-dir) BIN_DIR="$2"; shift ;;
	--state-dir) STATE_DIR="$2"; shift ;;
	--plex-url) PLEX_URL="$2"; PLEX_URL_SET=1; shift ;;
	--plex-token) PLEX_TOKEN="$2"; shift ;;
	--movies) MOVIES="$2"; shift ;;
	--tv) TV="$2"; shift ;;
	--movie-section) MOVIE_SECTION="$2"; shift ;;
	--tv-section) TV_SECTION="$2"; shift ;;
	--downloads) DOWNLOADS="$2"; shift ;;
	--tmdb-key) TMDB_KEY="$2"; shift ;;
	--port) PORT="$2"; shift ;;
	--torrent-port) TORRENT_PORT="$2"; shift ;;
	--upload-limit-kib) UPLOAD_LIMIT_KIB="$2"; shift ;;
	--auto-update) AUTO_UPDATE=1 ;;
	--no-auto-update) AUTO_UPDATE=0 ;;
	--error-reports) ERROR_REPORTS=1 ;;
	--no-error-reports) ERROR_REPORTS=0 ;;
	--remote) REMOTE=1 ;;
	--ngrok-token) NGROK_TOKEN="$2"; REMOTE=1; shift ;;
	--ngrok-domain) NGROK_DOMAIN="$2"; REMOTE=1; shift ;;
	--google-client-id) GOOGLE_ID="$2"; REMOTE=1; shift ;;
	--google-client-secret) GOOGLE_SECRET="$2"; REMOTE=1; shift ;;
	--allow-email) ALLOW_EMAILS="$ALLOW_EMAILS${ALLOW_EMAILS:+,}$2"; REMOTE=1; shift ;;
	-h | --help) usage; exit 0 ;;
	*) echo "Unknown option: $1 (see --help)" >&2; exit 2 ;;
	esac
	shift
done
# Remote settings from environment variables turn remote setup on, as their flags do.
[ -z "$NGROK_TOKEN$NGROK_DOMAIN$GOOGLE_ID$GOOGLE_SECRET$ALLOW_EMAILS" ] || REMOTE=1

say() { printf '%s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }
die() {
	printf 'error: %s\n' "$*" >&2
	# Told to Arr's developer only if this install's config says so (docs/PRIVACY.md).
	if [ -n "${ARR:-}" ] && [ -x "${ARR:-}" ] && [ -f "${CONFIG:-}" ]; then
		"$ARR" report --config "$CONFIG" --component installer --message "$*" >/dev/null 2>&1 </dev/null || true
	fi
	exit 1
}
have() { command -v "$1" >/dev/null 2>&1; }

# Prompts read from the terminal, so they work under `curl ... | sh`. Without one, or with
# --yes, every question takes its default.
interactive() { [ "$YES" != 1 ] && [ -r /dev/tty ] && (: </dev/tty) 2>/dev/null; }
ask() { # ask "Question" default -> answer
	if interactive; then
		printf '%s [%s]: ' "$1" "$2" >/dev/tty
		read -r a </dev/tty || a=""
		[ -n "$a" ] && { printf '%s' "$a"; return; }
	fi
	printf '%s' "$2"
}
ask_secret() { # ask_secret "Question" -> answer, not shown as it's typed
	interactive || return 0
	printf '%s: ' "$1" >/dev/tty
	stty -echo </dev/tty 2>/dev/null || true
	read -r a </dev/tty || a=""
	stty echo </dev/tty 2>/dev/null || true
	printf '\n' >/dev/tty
	printf '%s' "$a"
}
confirm() { # confirm "Question" y|n
	a=$(ask "$1 (y/n)" "$2")
	case "$a" in [Yy]*) return 0 ;; *) return 1 ;; esac
}
toml() { # a TOML basic string
	printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')"
}
sha256() {
	if have sha256sum; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
fetch() { curl -fsSL --retry 3 "$@"; }

have curl || die "curl is required"
have tar || die "tar is required"

# ---------------------------------------------------------------- install the binary

os=$(uname -s)
arch=$(uname -m)
case "$arch" in
x86_64 | amd64) arch=x86_64 ;;
aarch64 | arm64) arch=aarch64 ;;
*) die "no prebuilt Arr for $arch; build from source (see README)" ;;
esac
case "$os" in
Linux) target="$arch-unknown-linux-gnu" ;;
Darwin) target="$arch-apple-darwin" ;;
*) die "Arr runs on Linux and macOS. On Windows, run this inside WSL2." ;;
esac

if [ -z "$DOWNLOAD_BASE" ]; then
	if [ "$VERSION" = latest ]; then
		DOWNLOAD_BASE="https://github.com/$REPO/releases/latest/download"
	else
		DOWNLOAD_BASE="https://github.com/$REPO/releases/download/$VERSION"
	fi
fi
if [ -z "$BIN_DIR" ]; then
	if [ "$(id -u)" = 0 ]; then BIN_DIR=/usr/local/bin; else BIN_DIR="$HOME/.local/bin"; fi
fi

step "Downloading Arr ($VERSION, $target)"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
asset="arr-$target.tar.gz"
fetch -o "$tmp/$asset" "$DOWNLOAD_BASE/$asset" || die "download failed: $DOWNLOAD_BASE/$asset"
fetch -o "$tmp/$asset.sha256" "$DOWNLOAD_BASE/$asset.sha256" || die "checksum download failed"
[ "$(sha256 "$tmp/$asset")" = "$(cut -d' ' -f1 "$tmp/$asset.sha256")" ] || die "checksum mismatch for $asset"
tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$BIN_DIR"
# The real folder, without symlinks: services and the updater run exactly the path checked.
BIN_DIR=$(cd "$BIN_DIR" && pwd -P)
install -m 755 "$tmp/arr-$target/arr" "$BIN_DIR/arr"
ARR="$BIN_DIR/arr"
say "Installed $("$ARR" --version) to $ARR"
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) say "Add $BIN_DIR to your PATH to run 'arr' directly." ;; esac

[ "$SETUP" = 1 ] || exit 0

# ---------------------------------------------------------------- ffprobe

step "Checking for ffprobe (Arr uses it to check every download before import)"
if ! have ffprobe; then
	sudo=""
	# With --yes, sudo must fail instead of waiting for a password nobody will type.
	if [ "$(id -u)" != 0 ]; then
		if [ "$YES" = 1 ]; then sudo="sudo -n"; else sudo="sudo"; fi
	fi
	if have apt-get; then cmd="$sudo apt-get install -y ffmpeg"
	elif have dnf; then cmd="$sudo dnf install -y ffmpeg-free"
	elif have pacman; then cmd="$sudo pacman -S --noconfirm ffmpeg"
	elif have apk; then cmd="$sudo apk add ffmpeg"
	elif have brew; then cmd="brew install ffmpeg"
	elif [ -x /opt/homebrew/bin/brew ]; then cmd="/opt/homebrew/bin/brew install ffmpeg"
	elif [ -x /usr/local/bin/brew ]; then cmd="/usr/local/bin/brew install ffmpeg"
	else cmd=""; fi
	# A Mac without Homebrew (the usual way to add tools to a Mac): Homebrew's own installer
	# first, which asks for your password, then FFmpeg with it.
	if [ -z "$cmd" ] && [ "$os" = Darwin ] && [ "$(id -u)" != 0 ] && interactive &&
		confirm "ffprobe is missing, and so is Homebrew, the usual way to add tools to a Mac. Install Homebrew (it asks for your password), then FFmpeg?" y; then
		fetch -o "$tmp/brew-install.sh" https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh ||
			die "couldn't download Homebrew's installer; see https://brew.sh"
		/bin/bash "$tmp/brew-install.sh" </dev/tty || die "Homebrew wasn't installed; see https://brew.sh, then run this again"
		for b in /opt/homebrew/bin/brew /usr/local/bin/brew; do [ -x "$b" ] && cmd="$b install ffmpeg" && break; done
		[ -n "$cmd" ] || die "Homebrew installed, but brew wasn't found; open a new terminal and run this again"
		YES_FFMPEG=1
	fi
	[ -n "$cmd" ] || die "install FFmpeg (it provides ffprobe), then run this again"
	if [ "${YES_FFMPEG:-0}" = 1 ] || confirm "ffprobe is missing. Install it now with '$cmd'?" y; then
		# shellcheck disable=SC2086
		$cmd </dev/null || die "could not install FFmpeg (run '$cmd' yourself, then run this again)"
	else
		die "install FFmpeg (it provides ffprobe), then run this again"
	fi
fi
# Homebrew's folders may not be on this shell's PATH yet (a fresh Homebrew, or a shell that
# never loaded its setup).
[ "$os" != Darwin ] || PATH="$PATH:/opt/homebrew/bin:/usr/local/bin"
FFPROBE=$(command -v ffprobe)
say "Found $FFPROBE"

# ---------------------------------------------------------------- state directory

if [ -z "$STATE_DIR" ]; then
	if [ "$os" = Darwin ]; then STATE_DIR="$HOME/Library/Application Support/arr"
	elif [ "$(id -u)" = 0 ]; then STATE_DIR=/var/lib/arr
	else STATE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/arr"; fi
fi
CONFIG="$STATE_DIR/config.toml"

# The service runs as the user who ran sudo, so check access as that user.
writable() {
	if [ "$(id -u)" = 0 ] && [ -n "${SUDO_USER:-}" ]; then sudo -u "$SUDO_USER" test -w "$1"
	else [ -w "$1" ]; fi
}

# Finds Plex and your libraries, then writes and checks a new config. Skipped when a config
# already exists: running the installer again only updates the binary and restarts Arr.
configure() {
# ---------------------------------------------------------------- Plex

step "Finding Plex"
fetch -o /dev/null "$PLEX_URL/identity" 2>/dev/null ||
	PLEX_URL=$(ask "Plex didn't answer at $PLEX_URL. Plex server address?" "$PLEX_URL")
fetch -o /dev/null "$PLEX_URL/identity" || die "can't reach Plex at $PLEX_URL (use --plex-url)"
say "Plex is at $PLEX_URL"

plex_container=""
if have docker; then
	plex_container=$(docker ps --format '{{.Names}} {{.Image}}' 2>/dev/null | grep -i plex | grep -iv -e tautulli -e overseerr -e seerr | head -n1 | cut -d' ' -f1 || true)
fi
# Where Plex keeps its token, for the usual ways of installing it.
prefs_candidates() {
	printf '%s\n' \
		"/var/lib/plexmediaserver/Library/Application Support/Plex Media Server/Preferences.xml" \
		"/var/snap/plexmediaserver/common/Library/Application Support/Plex Media Server/Preferences.xml" \
		"$HOME/Library/Application Support/Plex Media Server/Preferences.xml"
	if [ -n "$plex_container" ]; then
		cfg=$(docker inspect -f '{{range .Mounts}}{{if eq .Destination "/config"}}{{.Source}}{{end}}{{end}}' "$plex_container" 2>/dev/null || true)
		[ -n "$cfg" ] && printf '%s\n' "$cfg/Library/Application Support/Plex Media Server/Preferences.xml"
	fi
	return 0
}
if [ -z "$PLEX_TOKEN" ]; then
	prefs_candidates >"$tmp/prefs"
	while IFS= read -r f; do
		[ -r "$f" ] || continue
		PLEX_TOKEN=$(sed -n 's/.*PlexOnlineToken="\([^"]*\)".*/\1/p' "$f" | head -n1)
		[ -n "$PLEX_TOKEN" ] && { say "Read the Plex token from $f"; break; }
	done <"$tmp/prefs"
fi
if [ -z "$PLEX_TOKEN" ] && [ "$os" = Darwin ]; then
	PLEX_TOKEN=$(defaults read com.plexapp.plexmediaserver PlexOnlineToken 2>/dev/null || true)
fi
if [ -z "$PLEX_TOKEN" ]; then
	say "Arr needs your Plex token. In Plex Web, open any movie, choose ... > Get Info > View XML,"
	say "and copy the value after X-Plex-Token= in the address bar."
	PLEX_TOKEN=$(ask "Plex token" "")
fi
[ -n "$PLEX_TOKEN" ] || die "a Plex token is required (use --plex-token)"
# Tokens go to curl through private header files, never on its command line (visible in ps).
printf 'X-Plex-Token: %s\n' "$PLEX_TOKEN" >"$tmp/plex.header"
fetch -H @"$tmp/plex.header" -o "$tmp/sections.xml" "$PLEX_URL/library/sections" ||
	die "Plex refused the token"

# One line per library location: <type> <section number> <path as Plex sees it>
awk '
	/<Directory / {
		type = ""; key = ""
		if (match($0, / type="[^"]*"/)) type = substr($0, RSTART + 7, RLENGTH - 8)
		if (match($0, / key="[^"]*"/)) key = substr($0, RSTART + 6, RLENGTH - 7)
	}
	/<Location / && match($0, / path="[^"]*"/) {
		p = substr($0, RSTART + 7, RLENGTH - 8)
		gsub(/&quot;/, "\"", p); gsub(/&apos;/, "\047", p); gsub(/&lt;/, "<", p); gsub(/&gt;/, ">", p); gsub(/&amp;/, "\\&", p)
		print type " " key " " p
	}
' "$tmp/sections.xml" >"$tmp/locations"

pick() { # pick <movie|show> <section or empty> -> "<section> <path>"
	if [ -n "$2" ]; then grep "^$1 $2 " "$tmp/locations" | head -n1 | cut -d' ' -f2-
	else grep "^$1 " "$tmp/locations" | head -n1 | cut -d' ' -f2-; fi
}
movie_loc=$(pick movie "$MOVIE_SECTION")
tv_loc=$(pick show "$TV_SECTION")
[ -n "$movie_loc$tv_loc" ] || die "Plex has no movie or TV library yet; add one in Plex first"

# A folder as this machine sees it. Plex in Docker sees its own paths, so map them back
# through the container's mounts.
local_path() {
	if [ -n "$plex_container" ]; then
		# The most specific mount wins: /media/movies before /media.
		mapped=$(docker inspect -f '{{range .Mounts}}{{.Destination}}	{{.Source}}{{println}}{{end}}' "$plex_container" 2>/dev/null |
			awk -F '	' 'NF == 2 { print length($1) "	" $0 }' | sort -rn |
			while IFS='	' read -r _ dst src; do
				# "(pattern)": macOS's /bin/sh (bash 3.2) takes a bare "pattern)" inside $(...)
				# for the end of the substitution and fails to parse this script.
				case "$1" in ("$dst" | "$dst"/*) printf '%s%s' "$src" "${1#"$dst"}"; break ;; esac
			done)
		[ -n "$mapped" ] && { printf '%s' "$mapped"; return 0; }
	fi
	[ -d "$1" ] && printf '%s' "$1"
	return 0
}
MOVIE_SECTION=${movie_loc%% *}; PLEX_MOVIES=${movie_loc#* }
TV_SECTION=${tv_loc%% *}; PLEX_TV=${tv_loc#* }
if [ -n "$movie_loc" ]; then
	[ -n "$MOVIES" ] || MOVIES=$(ask "Movies folder on this machine (Plex sees $PLEX_MOVIES)" "$(local_path "$PLEX_MOVIES")")
	[ -d "$MOVIES" ] || die "movies folder not found: $MOVIES (use --movies)"
	say "Movies: $MOVIES (Plex library $MOVIE_SECTION)"
fi
if [ -n "$tv_loc" ]; then
	[ -n "$TV" ] || TV=$(ask "TV folder on this machine (Plex sees $PLEX_TV)" "$(local_path "$PLEX_TV")")
	[ -d "$TV" ] || die "TV folder not found: $TV (use --tv)"
	say "TV: $TV (Plex library $TV_SECTION)"
fi

# Downloads go next to the library by default, so imports can hardlink instead of copy.
first_root=${MOVIES:-$TV}
if [ -z "$DOWNLOADS" ]; then
	DOWNLOADS=$(ask "Download folder" "$(dirname "$first_root")/arr-downloads")
fi
if [ ! -d "$DOWNLOADS" ]; then
	mkdir -p "$DOWNLOADS" || die "can't create $DOWNLOADS"
	[ "$(id -u)" != 0 ] || [ -z "${SUDO_USER:-}" ] || chown "$SUDO_USER" "$DOWNLOADS"
fi
for d in "$MOVIES" "$TV" "$DOWNLOADS"; do
	[ -z "$d" ] || writable "$d" ||
		die "${SUDO_USER:-$(id -un)} can't write to $d; fix its permissions or run as the user your media belongs to"
done

if [ -z "$TMDB_KEY" ]; then
	say "Optional: a free TMDB key lets Arr find movies you don't own yet"
	say "(https://www.themoviedb.org/settings/api). TV search works without one."
	TMDB_KEY=$(ask "TMDB API key or read token (Enter to skip)" "")
fi

if [ "$DOWNLOADING" = 1 ] && ! confirm "Let Arr download and import what you ask for?" y; then
	DOWNLOADING=0
fi

# ---------------------------------------------------------------- write the config

if [ -z "$ERROR_REPORTS" ]; then
	ERROR_REPORTS=0
	if confirm "Send error reports to help fix Arr? Names, folders, addresses and passwords are removed first, and nothing about your library is sent (docs/PRIVACY.md)" n; then
		ERROR_REPORTS=1
	fi
fi

step "Writing $CONFIG"
umask 077
# init creates the folders and the API token; a previous failed run may have done so already.
[ -f "$STATE_DIR/secrets/api_token" ] || "$ARR" init --state-dir "$STATE_DIR" --listen "127.0.0.1:$PORT" >/dev/null
secret() { printf '%s' "$2" >"$STATE_DIR/secrets/$1"; chmod 600 "$STATE_DIR/secrets/$1"; }
secret plex_token "$PLEX_TOKEN"
od -An -N32 -tx1 /dev/urandom | tr -d ' \n' >"$STATE_DIR/secrets/mcp_token"
chmod 600 "$STATE_DIR/secrets/mcp_token"
[ -z "$TMDB_KEY" ] || secret tmdb_credential "$TMDB_KEY"
execution=false; [ "$DOWNLOADING" = 1 ] && execution=true
tmdb=false; [ -n "$TMDB_KEY" ] && tmdb=true
{
	cat <<EOF
# Arr configuration, written by install.sh. Secrets live in secrets/ (mode 600).
# All options: https://github.com/$REPO/blob/main/config/standalone.example.toml

[server]
listen = $(toml "127.0.0.1:$PORT")
api_token_file = "secrets/api_token"

[state]
dir = "."

[[indexer]]
id = 1
name = "The Pirate Bay"
kind = "thepiratebay"
base_url = "https://apibay.org/"
recent_feed = true

[tvmaze]
enabled = true

[tmdb]
enabled = $tmdb
EOF
	[ "$tmdb" = true ] && echo 'credential_file = "secrets/tmdb_credential"'
	cat <<EOF

[execution]
enabled = $execution
download_cleanup = true

# Sonarr/Radarr can keep running: Arr only acquires what you add to Arr.
[migration]
mode = "coexistence"

[embedded]
download_dir = $(toml "$DOWNLOADS")
listen = $(toml "0.0.0.0:$TORRENT_PORT")
trackers = true
dht = true
max_active_downloads = 3
EOF
	[ "$UPLOAD_LIMIT_KIB" -gt 0 ] 2>/dev/null && echo "upload_limit_bytes_per_sec = $((UPLOAD_LIMIT_KIB * 1024))"
	cat <<EOF

[library]
ffprobe = $(toml "$FFPROBE")
EOF
	[ -n "$MOVIES" ] && echo "movies = $(toml "$MOVIES")"
	[ -n "$TV" ] && echo "tv = $(toml "$TV")"
	cat <<EOF

[plex]
base_url = $(toml "$PLEX_URL")
token_file = "secrets/plex_token"
EOF
	[ -n "$MOVIES" ] && printf 'movie_section = %s\nmovies_path = %s\n' "$MOVIE_SECTION" "$(toml "$PLEX_MOVIES")"
	[ -n "$TV" ] && printf 'tv_section = %s\ntv_path = %s\n' "$TV_SECTION" "$(toml "$PLEX_TV")"
	cat <<EOF

# MCP for AI assistants on this machine (Claude Desktop, Claude Code, ...); see docs/MCP.md.
[mcp]
enabled = true
token_file = "secrets/mcp_token"
EOF
	[ "$ERROR_REPORTS" != 1 ] ||
		printf '\n# Error reports with names, paths, addresses and tokens removed; see docs/PRIVACY.md.\n[reporting]\nenabled = true\n'
} >"$CONFIG"
chmod 600 "$CONFIG"

# A config that fails its checks is moved aside, so running this again starts fresh.
failed() { mv "$CONFIG" "$CONFIG.failed"; die "$1 (the config is saved as $CONFIG.failed)"; }
"$ARR" setup --config "$CONFIG" >/dev/null || failed "arr setup failed; see the message above"
"$ARR" check --config "$CONFIG" >"$tmp/check.json" || { cat "$tmp/check.json"; failed "arr check failed"; }
say "Config checked"
}

# The address this machine uses on its network.
lan_ip() {
	if have ip; then
		ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p' | head -n1
	elif have ipconfig; then
		ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || true
	fi
}

# sudo usually drops the desktop's variables. Finds the sudo user's desktop session again,
# so the setup page can open in their own browser.
desktop_env() {
	[ "$os" = Linux ] && [ "$(id -u)" = 0 ] && [ -n "${SUDO_USER:-}" ] && have loginctl || return 0
	[ -z "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] || return 0
	sid=$(loginctl show-user "$SUDO_USER" -p Display --value 2>/dev/null) || return 0
	[ -n "$sid" ] || return 0
	rt="/run/user/$(id -u "$SUDO_USER")"
	case "$(loginctl show-session "$sid" -p Type --value 2>/dev/null)" in
	wayland) WAYLAND_DISPLAY=$(ls "$rt" 2>/dev/null | grep -m1 '^wayland-[0-9]*$') || return 0 ;;
	x11) DISPLAY=$(loginctl show-session "$sid" -p Display --value 2>/dev/null) || return 0 ;;
	esac
	[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] || return 0
	DESKTOP_ENV="XDG_RUNTIME_DIR=$rt DBUS_SESSION_BUS_ADDRESS=unix:path=$rt/bus"
	[ -z "${DISPLAY:-}" ] || DESKTOP_ENV="$DESKTOP_ENV DISPLAY=$DISPLAY"
	[ -z "${WAYLAND_DISPLAY:-}" ] || DESKTOP_ENV="$DESKTOP_ENV WAYLAND_DISPLAY=$WAYLAND_DISPLAY"
}

# Setup on a web page: sign in to Plex, pick libraries and folders, import what Plex has.
# `arr wizard` writes the same config configure() does, then exits.
browser_setup() {
	step "Setting up Arr in your web browser"
	# Over SSH (or on a machine without a screen) the page has to open on another computer,
	# so it listens on this machine's network address. The link carries a secret code.
	bind=127.0.0.1
	DESKTOP_ENV=""
	desktop_env
	if [ -n "${SSH_CONNECTION:-}" ]; then
		bind=$(printf '%s' "$SSH_CONNECTION" | awk '{print $3}')
	elif [ "$os" = Linux ] && [ -z "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
		bind=$(lan_ip)
	fi
	case "$bind" in "" | *%*) bind=$(lan_ip) ;; esac
	[ -n "$bind" ] || bind=127.0.0.1
	umask 077
	mkdir -p "$STATE_DIR"
	set -- "$ARR" wizard --state-dir "$STATE_DIR" --bind "$bind" --port "$PORT" \
		--torrent-port "$TORRENT_PORT" --upload-limit-kib "$UPLOAD_LIMIT_KIB" --ffprobe "$FFPROBE"
	[ "$bind" != 127.0.0.1 ] || set -- "$@" --open
	# Arr runs as the user who ran sudo, so the page checks folders as that user too.
	if [ "$(id -u)" = 0 ] && [ -n "${SUDO_USER:-}" ]; then
		chown "$SUDO_USER" "$STATE_DIR"
		# shellcheck disable=SC2086 # NAME=value words without spaces
		set -- sudo -u "$SUDO_USER" env $DESKTOP_ENV "$@"
	fi
	"$@" </dev/null || die "setup wasn't finished; run this script again to start over"
	# The page asked about automatic updates; the answer is used once, below.
	if [ -z "$AUTO_UPDATE" ] && [ -f "$STATE_DIR/setup-choices" ]; then
		AUTO_UPDATE=$(sed -n 's/^auto_update=\([01]\)$/\1/p' "$STATE_DIR/setup-choices")
	fi
	rm -f "$STATE_DIR/setup-choices"
	grep -A1 '^\[execution\]' "$CONFIG" | grep -q 'enabled = true' || DOWNLOADING=0
}

if [ -f "$CONFIG" ]; then
	step "Keeping your config in $STATE_DIR"
	PORT=$(sed -n 's/^listen = "[^"]*:\([0-9][0-9]*\)"$/\1/p' "$CONFIG" | head -n1)
	PORT=${PORT:-8787}
	grep -A1 '^\[execution\]' "$CONFIG" | grep -q 'enabled = true' || DOWNLOADING=0
elif [ "$TERMINAL" != 1 ] && interactive && [ "$DOWNLOADING" = 1 ] && [ "$PLEX_URL_SET" = 0 ] &&
	[ -z "$PLEX_TOKEN$MOVIES$TV$MOVIE_SECTION$TV_SECTION$DOWNLOADS$TMDB_KEY" ]; then
	# Setup options on the command line (or --search-only) mean the terminal setup.
	browser_setup
else
	configure
fi

# ---------------------------------------------------------------- phone and ChatGPT access

# ChatGPT (on the web, your phone or any device) needs a public HTTPS address and a sign-in.
# A free ngrok domain provides the address; Google sign-in limits Arr to the accounts you list.
# Opens a page in the browser of whoever is at this computer: not over SSH or with --yes,
# and under sudo as the user who ran it, never as root.
open_page() {
	interactive && [ -z "${SSH_CONNECTION:-}" ] || return 0
	url=$1
	if [ "$os" = Darwin ]; then
		if [ "$(id -u)" = 0 ]; then
			[ -n "${SUDO_USER:-}" ] || return 0
			set -- sudo -u "$SUDO_USER" open "$url"
		else
			set -- open "$url"
		fi
	else
		have xdg-open || return 0
		if [ "$(id -u)" = 0 ]; then
			[ -n "${SUDO_USER:-}" ] || return 0
			# The desktop session the setup page found, if it ran; otherwise find it now. Either
			# way the browser gets the user's runtime folder and session bus, not just a display.
			[ -n "${DESKTOP_ENV:-}" ] || desktop_env
			if [ -z "${DESKTOP_ENV:-}" ]; then
				[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] || return 0
				rt="/run/user/$(id -u "$SUDO_USER")"
				DESKTOP_ENV="XDG_RUNTIME_DIR=$rt DBUS_SESSION_BUS_ADDRESS=unix:path=$rt/bus${DISPLAY:+ DISPLAY=$DISPLAY}${WAYLAND_DISPLAY:+ WAYLAND_DISPLAY=$WAYLAND_DISPLAY}"
			fi
			# shellcheck disable=SC2086 # NAME=value words without spaces
			set -- sudo -u "$SUDO_USER" env $DESKTOP_ENV xdg-open "$url"
		else
			[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] || return 0
			set -- xdg-open "$url"
		fi
	fi
	say "  (opening $url)"
	"$@" >/dev/null 2>&1 </dev/null &
}

remote_setup() {
	step "Setting up access from ChatGPT on your phone and other devices"
	if grep -q '^\[mcp\.oauth\]' "$CONFIG"; then
		say "Your config already has remote access; keeping it."
		return 0
	fi
	say "You need a free ngrok account and a Google OAuth client. The full guide:"
	say "  https://github.com/$REPO/blob/main/docs/REMOTE.md"
	if [ -z "$NGROK_TOKEN" ] || [ -z "$NGROK_DOMAIN" ]; then
		say ""
		say "Step 1 of 2: ngrok gives Arr a public HTTPS address (free, no router changes)."
		say "  Sign up or log in, then copy \"Your Authtoken\", and on the Domains page copy your free"
		say "  domain (create one if the list is empty)."
	fi
	if [ -z "$NGROK_TOKEN" ]; then
		open_page https://dashboard.ngrok.com/get-started/your-authtoken
		NGROK_TOKEN=$(ask_secret "  ngrok authtoken")
	fi
	if [ -z "$NGROK_DOMAIN" ]; then
		open_page https://dashboard.ngrok.com/domains
		NGROK_DOMAIN=$(ask "  ngrok domain, like quiet-river-1234.ngrok-free.app" "")
	fi
	NGROK_DOMAIN=$(printf '%s' "$NGROK_DOMAIN" | sed 's#^https\{0,1\}://##; s#/.*$##' | tr 'A-Z' 'a-z')
	[ -n "$NGROK_TOKEN" ] && [ -n "$NGROK_DOMAIN" ] ||
		die "remote access needs --ngrok-token and --ngrok-domain (see docs/REMOTE.md)"
	case "$NGROK_TOKEN" in *[!A-Za-z0-9_-]*) die "that doesn't look like an ngrok authtoken" ;; esac
	case "$NGROK_DOMAIN" in
	*[!a-z0-9.-]* | .* | *. | *..*) die "that doesn't look like a domain: $NGROK_DOMAIN" ;;
	*.*) ;;
	*) die "that doesn't look like a domain: $NGROK_DOMAIN" ;;
	esac
	if [ -z "$GOOGLE_ID" ] || [ -z "$GOOGLE_SECRET" ]; then
		say ""
		say "Step 2 of 2: Google sign-in decides who may use Arr. In Google Cloud:"
		say "  - create a project (any name, for example arr);"
		say "  - Google Auth Platform > Get started: app name Arr, audience External, your email;"
		say "  - Audience: keep it in Testing and add each Google account that may use Arr as a test user;"
		say "  - Clients > Create client: type \"Web application\", with this authorized redirect URI:"
		say "      https://$NGROK_DOMAIN/auth/callback"
		say "  - copy the client ID and client secret."
		open_page https://console.cloud.google.com/auth/clients
	fi
	[ -n "$GOOGLE_ID" ] || GOOGLE_ID=$(ask "  Google OAuth client ID" "")
	[ -n "$GOOGLE_SECRET" ] || GOOGLE_SECRET=$(ask_secret "  Google OAuth client secret")
	[ -n "$ALLOW_EMAILS" ] || ALLOW_EMAILS=$(ask "  Google accounts allowed to use Arr (comma-separated)" "")
	[ -n "$GOOGLE_ID" ] && [ -n "$GOOGLE_SECRET" ] && [ -n "$ALLOW_EMAILS" ] ||
		die "remote access needs --google-client-id, --google-client-secret and --allow-email (see docs/REMOTE.md)"
	emails=""
	for e in $(printf '%s' "$ALLOW_EMAILS" | tr ',' ' '); do
		case "$e" in *[!A-Za-z0-9._%+@-]* | *@*@* | @* | *@) die "not an email address: $e" ;; *@*.*) ;; *) die "not an email address: $e" ;; esac
		emails="$emails${emails:+, }$(toml "$(printf '%s' "$e" | tr 'A-Z' 'a-z')")"
	done

	NGROK=$(command -v ngrok || true)
	if [ -z "$NGROK" ]; then
		say "Installing the ngrok agent"
		case "$os-$arch" in
		Linux-x86_64) n=linux-amd64.tgz ;;
		Linux-aarch64) n=linux-arm64.tgz ;;
		Darwin-x86_64) n=darwin-amd64.zip ;;
		*) n=darwin-arm64.zip ;;
		esac
		fetch -o "$tmp/ngrok-$n" "https://bin.equinox.io/c/bNyj1mQVY4c/ngrok-v3-stable-$n" || die "ngrok download failed"
		case "$n" in
		*.zip) (cd "$tmp" && unzip -oq "ngrok-$n") ;;
		*) tar -xzf "$tmp/ngrok-$n" -C "$tmp" ;;
		esac
		install -m 755 "$tmp/ngrok" "$BIN_DIR/ngrok"
		NGROK="$BIN_DIR/ngrok"
	fi
	umask 077
	# The authtoken stays in a private config file, never on a command line. web_addr: false
	# turns off ngrok's local inspection API, which would otherwise let any local user read the
	# sign-in tokens passing through the tunnel.
	printf 'version: "3"\nagent:\n  authtoken: %s\n  web_addr: false\n' "$NGROK_TOKEN" >"$STATE_DIR/ngrok.yml"
	"$NGROK" config check --config "$STATE_DIR/ngrok.yml" >/dev/null || die "ngrok rejected its config"
	printf '%s' "$GOOGLE_SECRET" >"$STATE_DIR/secrets/google_client_secret"
	[ -f "$STATE_DIR/secrets/mcp_token" ] ||
		od -An -N32 -tx1 /dev/urandom | tr -d ' \n' >"$STATE_DIR/secrets/mcp_token"

	# /mcp must accept the ngrok host; then add the sign-in section.
	cp "$CONFIG" "$tmp/config.before-remote"
	hosts="allowed_hosts = [$(toml "$NGROK_DOMAIN"), \"localhost\", \"127.0.0.1\"]"
	# [[]mcp[]] instead of \[mcp\]: BusyBox awk drops backslashes from -v variables.
	mcp_header='^[[:space:]]*[[]mcp[]][[:space:]]*(#.*)?$'
	if grep -Eq "$mcp_header" "$CONFIG"; then
		grep -Eq '^[[:space:]]*allowed_hosts[[:space:]]*=' "$CONFIG" &&
			die "your config already sets allowed_hosts; add [mcp.oauth] by hand (see docs/MCP.md)"
		awk -v line="$hosts" -v re="$mcp_header" '{ print } $0 ~ re { print line }' "$tmp/config.before-remote" >"$CONFIG"
	else
		printf '\n[mcp]\nenabled = true\ntoken_file = "secrets/mcp_token"\n%s\n' "$hosts" >>"$CONFIG"
	fi
	cat >>"$CONFIG" <<EOF

# Google sign-in for ChatGPT and other remote MCP clients (docs/REMOTE.md).
[mcp.oauth]
public_url = $(toml "https://$NGROK_DOMAIN")
google_client_id = $(toml "$GOOGLE_ID")
google_client_secret_file = "secrets/google_client_secret"
allowed_emails = [$emails]
EOF
	if ! "$ARR" check --config "$CONFIG" >"$tmp/check.json"; then
		cat "$tmp/check.json"
		cp "$tmp/config.before-remote" "$CONFIG"
		die "arr check rejected the remote access settings; your previous config is restored"
	fi
	if ! grep -q '"mcp_enabled": *true' "$tmp/check.json"; then
		cp "$tmp/config.before-remote" "$CONFIG"
		die "your config turns MCP off ([mcp] enabled = false); set it to true and run this again"
	fi
	say "Remote access configured for https://$NGROK_DOMAIN"
}
# Offered once, like on Windows, when nobody asked for it either way.
if [ "$REMOTE" != 1 ] && ! grep -q '^\[mcp\.oauth\]' "$CONFIG" && interactive; then
	say ""
	say "Arr can also work from ChatGPT on your phone and other devices. That needs a free ngrok"
	say "account and a Google sign-in app; this guide walks you through both (about 15 minutes)."
	if confirm "Set that up now?" n; then REMOTE=1; fi
fi
if [ "$REMOTE" = 1 ]; then remote_setup; fi

# A configured tunnel runs as its own service next to Arr.
PUBLIC_URL=""
logs_tunnel="the arr-tunnel service's log"
NGROK=${NGROK:-}
if [ -f "$STATE_DIR/ngrok.yml" ] && grep -q '^\[mcp\.oauth\]' "$CONFIG"; then
	PUBLIC_URL=$(sed -n 's/^public_url = "\(.*\)"$/\1/p' "$CONFIG" | head -n1)
	NGROK=${NGROK:-$(command -v ngrok || printf '%s' "$BIN_DIR/ngrok")}
fi

# ---------------------------------------------------------------- run it

step "Starting Arr"
run_cmd="\"$ARR\" serve --config \"$CONFIG\""
started=0
# systemd quoting: escape \ and " and double %; arguments (not the program) also expand $.
sd_exe() { printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g; s/%/%%/g')"; }
sd() { printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g; s/%/%%/g; s/\$/$$/g')"; }
xml() { printf '%s' "$1" | sed 's/&/\&amp;/g; s/</\&lt;/g; s/>/\&gt;/g; s/"/\&quot;/g'; }
if [ "$SERVICE" = 1 ] && [ "$os" = Linux ] && have systemctl && [ -d /run/systemd/system ]; then
	unit="[Unit]
Description=Arr media manager
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=$(sd_exe "$ARR") serve --config $(sd "$CONFIG")
Restart=on-failure
RestartSec=5
UMask=0002
"
	tunnel="[Unit]
Description=Public HTTPS address for Arr (ngrok)
After=network-online.target arr.service
Wants=network-online.target

[Service]
ExecStart=$(sd_exe "$NGROK") http $(sd "127.0.0.1:$PORT") $(sd "--url=$PUBLIC_URL") --config $(sd "$STATE_DIR/ngrok.yml") --log=stdout --log-level=warn
Restart=always
RestartSec=10
"
	if [ "$(id -u)" = 0 ]; then
		run_as=${SUDO_USER:-root}
		if [ "$run_as" != root ]; then
			chown -R "$run_as" "$STATE_DIR"
			unit="$unit
User=$run_as"
			tunnel="$tunnel
User=$run_as"
		fi
		printf '%s\n[Install]\nWantedBy=multi-user.target\n' "$unit" >/etc/systemd/system/arr.service
		[ -z "$PUBLIC_URL" ] || printf '%s\n[Install]\nWantedBy=multi-user.target\n' "$tunnel" >/etc/systemd/system/arr-tunnel.service
		systemctl daemon-reload && systemctl enable arr >/dev/null 2>&1 && systemctl restart arr && started=1
		[ -z "$PUBLIC_URL" ] || { systemctl enable arr-tunnel >/dev/null 2>&1 && systemctl restart arr-tunnel; }
		logs="journalctl -u arr -f"
		logs_tunnel="journalctl -u arr-tunnel"
	else
		mkdir -p "$HOME/.config/systemd/user"
		printf '%s\n[Install]\nWantedBy=default.target\n' "$unit" >"$HOME/.config/systemd/user/arr.service"
		[ -z "$PUBLIC_URL" ] || printf '%s\n[Install]\nWantedBy=default.target\n' "$tunnel" >"$HOME/.config/systemd/user/arr-tunnel.service"
		systemctl --user daemon-reload && systemctl --user enable arr >/dev/null 2>&1 &&
			systemctl --user restart arr && started=1
		[ -z "$PUBLIC_URL" ] || { systemctl --user enable arr-tunnel >/dev/null 2>&1 && systemctl --user restart arr-tunnel; }
		logs="journalctl --user -u arr -f"
		logs_tunnel="journalctl --user -u arr-tunnel"
		# Keeps the user services running after you log out and starts them at boot. Over SSH
		# this often needs an administrator; say so instead of failing quietly.
		have loginctl && loginctl --no-ask-password enable-linger "$(id -un)" >/dev/null 2>&1 || true
		if ! loginctl show-user "$(id -un)" -p Linger 2>/dev/null | grep -q '=yes'; then
			say "Warning: Arr stops when you log out and won't start after a reboot. Fix it with:"
			say "  sudo loginctl enable-linger $(id -un)"
			say "or install system-wide by running this script with sudo."
		fi
	fi
elif [ "$SERVICE" = 1 ] && [ "$os" = Darwin ]; then
	plist="$HOME/Library/LaunchAgents/io.github.newtonharry.arr.plist"
	mkdir -p "$(dirname "$plist")"
	cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>io.github.newtonharry.arr</string>
  <key>ProgramArguments</key><array>
    <string>$(xml "$ARR")</string><string>serve</string><string>--config</string><string>$(xml "$CONFIG")</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>$(xml "$STATE_DIR/arr.log")</string>
</dict></plist>
EOF
	launchctl unload "$plist" 2>/dev/null || true
	launchctl load "$plist" && started=1
	logs="tail -f \"$STATE_DIR/arr.log\""
	logs_tunnel="$STATE_DIR/tunnel.log"
	if [ -n "$PUBLIC_URL" ]; then
		tplist="$HOME/Library/LaunchAgents/io.github.newtonharry.arr-tunnel.plist"
		cat >"$tplist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>io.github.newtonharry.arr-tunnel</string>
  <key>ProgramArguments</key><array>
    <string>$(xml "$NGROK")</string><string>http</string><string>127.0.0.1:$PORT</string>
    <string>--url=$(xml "$PUBLIC_URL")</string><string>--config</string><string>$(xml "$STATE_DIR/ngrok.yml")</string>
    <string>--log=stdout</string><string>--log-level=warn</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$(xml "$STATE_DIR/tunnel.log")</string>
</dict></plist>
EOF
		launchctl unload "$tplist" 2>/dev/null || true
		launchctl load "$tplist"
	fi
fi
if [ "$started" != 1 ]; then
	say "No background service was installed. Start Arr with:"
	say "  $run_cmd"
	[ -z "$PUBLIC_URL" ] ||
		say "  \"$NGROK\" http 127.0.0.1:$PORT --url=$PUBLIC_URL --config \"$STATE_DIR/ngrok.yml\""
	exit 0
fi

# ---------------------------------------------------------------- automatic updates

# A daily job installs new releases. It runs a local updater written here, never a script
# fetched from GitHub, so it trusts only the release binaries CI builds from version tags
# (checked by SHA-256), and it rolls back if the new version doesn't start.
if [ "$os" = Darwin ]; then kind=launchd; elif [ "$(id -u)" = 0 ]; then kind=system; else kind=user; fi
UPDATER="$BIN_DIR/arr-update"
if [ -z "$AUTO_UPDATE" ]; then
	if [ -f "$UPDATER" ]; then
		AUTO_UPDATE=1
	elif ! interactive; then
		# Updating needs your say-so: without a prompt (--yes, or no terminal), only
		# --auto-update turns it on.
		AUTO_UPDATE=0
	elif confirm "Install new versions of Arr automatically (checked once a day)?" y; then
		AUTO_UPDATE=1
	else
		AUTO_UPDATE=0
	fi
fi
sq() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
# For a system-wide install root runs the updater and the binary, so no other account may
# be able to replace them: the binary and every folder from its own up to / must be owned
# by root and not writable by group or others.
root_safe() {
	p=$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1")
	# A symlink anywhere in the path could be repointed later: only real paths qualify.
	[ "$p" = "$1" ] && [ ! -L "$p" ] || return 1
	while :; do
		owner=$(stat -c %u "$p" 2>/dev/null) && mode=$(stat -c %a "$p" 2>/dev/null) || return 1
		[ "$owner" = 0 ] && [ $((0$mode & 022)) -eq 0 ] || return 1
		[ "$p" = / ] && return 0
		p=$(dirname "$p")
	done
}
# One lock tool per OS, fixed here with its full path: every run (by hand or by the timer,
# whatever its PATH) must use the same kind of lock to keep the others out.
if [ "$os" = Darwin ]; then lock_kind=shlock; lock_tool=/usr/bin/shlock
else lock_kind=flock; lock_tool=$(command -v flock || true); fi
case "$lock_tool" in /*) ;; *) lock_tool="" ;; esac
if [ "$AUTO_UPDATE" = 1 ] && ! [ -x "$lock_tool" ]; then
	say "Automatic updates stay off: this system has no $lock_kind to keep two updates apart."
	AUTO_UPDATE=0
fi
if [ "$AUTO_UPDATE" = 1 ] && [ "$kind" = system ] && ! root_safe "$ARR"; then
	say "Automatic updates stay off: $BIN_DIR can be changed by accounts other than root,"
	say "and root would run the updater from there. Install to a root-owned folder such as"
	say "/usr/local/bin (the default) to use them."
	AUTO_UPDATE=0
fi
if [ "$AUTO_UPDATE" = 1 ]; then
	step "Turning on automatic updates"
	{
		printf '#!/bin/sh\n# Arr updater, written by install.sh. A daily job runs it; you can run it by hand too.\n'
		printf 'REPO=%s\nARR=%s\nTARGET=%s\nKIND=%s\nPORT=%s\nLOCK_KIND=%s\nLOCK_TOOL=%s\nCONFIG=%s\n' \
			"$(sq "$REPO")" "$(sq "$ARR")" "$(sq "$target")" "$(sq "$kind")" "$(sq "$PORT")" \
			"$(sq "$lock_kind")" "$(sq "$lock_tool")" "$(sq "$CONFIG")"
		cat <<'EOF'
set -eu
log() { printf 'arr-update: %s\n' "$*"; }
fetch() { curl -fsSL --retry 3 "$@"; }
sha256() {
	if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
# Is version $1 newer than $2? (vX.Y.Z)
newer() {
	printf '%s\n%s\n' "${1#v}" "${2#v}" | awk -F. '
		NR == 1 { for (i = 1; i <= 3; i++) a[i] = $i + 0 }
		NR == 2 { for (i = 1; i <= 3; i++) b[i] = $i + 0 }
		END { for (i = 1; i <= 3; i++) { if (a[i] > b[i]) exit 0; if (a[i] < b[i]) exit 1 } exit 1 }'
}
# A failed restart must not stop the script before it can roll back: the health check
# afterwards decides whether the new version runs.
restart() {
	case "$KIND" in
	system) systemctl restart arr || true ;;
	user) systemctl --user restart arr || true ;;
	launchd) launchctl kickstart -k "gui/$(id -u)/io.github.newtonharry.arr" || true ;;
	esac
}
# Waits up to 30 seconds for Arr to answer its health check as version $1. Each request
# has its own short timeout, so a server that accepts but never answers can't stall this.
healthy() {
	start=$(date +%s)
	until curl -fsS --max-time 3 "http://127.0.0.1:$PORT/api/v1/health" 2>/dev/null |
		grep -q "\"version\": *\"${1#v}\""; do
		[ $(($(date +%s) - start)) -lt 30 ] || return 1
		sleep 1
	done
}

# One update at a time: a manual run and the daily job must not share .previous and .new.
# flock (Linux) holds a kernel lock that ends with this process, so it can't go stale;
# shlock (macOS) writes its pid atomically and replaces a dead holder's lock safely.
lock="$ARR.update-lock"
tmp=""
# The installer fixed LOCK_KIND and LOCK_TOOL, so every run takes the same kind of lock.
[ -x "$LOCK_TOOL" ] || { log "$LOCK_TOOL is missing; not updating"; exit 1; }
case "$LOCK_KIND" in
flock)
	exec 9>"$lock"
	"$LOCK_TOOL" -n 9 || { log "another update is running"; exit 0; }
	trap 'rm -rf ${tmp:+"$tmp"}' EXIT
	;;
shlock)
	"$LOCK_TOOL" -f "$lock" -p $$ || { log "another update is running"; exit 0; }
	trap 'rm -f "$lock"; rm -rf ${tmp:+"$tmp"}' EXIT
	;;
*)
	log "unknown lock kind $LOCK_KIND; not updating"
	exit 1
	;;
esac

RELEASES=${ARR_RELEASES_URL:-https://github.com/$REPO/releases}
latest=$(curl -fsSLI --retry 3 -o /dev/null -w '%{url_effective}' "$RELEASES/latest" | sed 's#.*/tag/##')
case "$latest" in v[0-9]*) ;; *) log "couldn't find the latest release"; exit 1 ;; esac
current="v$("$ARR" --version | awk '{print $2}')"
if ! newer "$latest" "$current"; then
	log "up to date ($current)"
	exit 0
fi
log "updating $current to $latest"
tmp=$(mktemp -d)
asset="arr-$TARGET.tar.gz"
base="$RELEASES/download/$latest"
fetch -o "$tmp/$asset" "$base/$asset"
fetch -o "$tmp/$asset.sha256" "$base/$asset.sha256"
if [ "$(sha256 "$tmp/$asset")" != "$(cut -d' ' -f1 "$tmp/$asset.sha256")" ]; then
	log "checksum mismatch; not updating"
	exit 1
fi
tar -xzf "$tmp/$asset" -C "$tmp"
"$tmp/arr-$TARGET/arr" --version >/dev/null || { log "$latest doesn't run on this machine; not updating"; exit 1; }
# Swap the binary in one rename, keeping the old one for a rollback.
cp -p "$ARR" "$ARR.previous"
cp "$tmp/arr-$TARGET/arr" "$ARR.new"
chmod 755 "$ARR.new"
mv -f "$ARR.new" "$ARR"
restart
if healthy "$latest"; then
	log "now running $latest"
	exit 0
fi
log "$latest didn't start; going back to $current"
mv -f "$ARR.previous" "$ARR"
restart
healthy "$current" || log "$current didn't start either; check Arr's logs"
# Told to Arr's developer only if the config says so (docs/PRIVACY.md).
"$ARR" report --config "$CONFIG" --component updater --message "$latest didn't start, so Arr went back to $current" >/dev/null 2>&1 || true
exit 1
EOF
	} >"$tmp/arr-update"
	install -m 755 "$tmp/arr-update" "$UPDATER"
	case "$kind" in
	system | user)
		if [ "$kind" = system ]; then udir=/etc/systemd/system; sctl="systemctl"; else udir="$HOME/.config/systemd/user"; sctl="systemctl --user"; fi
		printf '[Unit]\nDescription=Update Arr to the latest release\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=oneshot\nExecStart=/bin/sh %s\n' \
			"$(sd "$UPDATER")" >"$udir/arr-update.service"
		printf '[Unit]\nDescription=Check for Arr updates once a day\n\n[Timer]\nOnCalendar=daily\nRandomizedDelaySec=6h\nPersistent=true\n\n[Install]\nWantedBy=timers.target\n' \
			>"$udir/arr-update.timer"
		$sctl daemon-reload && $sctl enable --now arr-update.timer >/dev/null 2>&1 ||
			die "couldn't start the update timer"
		;;
	launchd)
		uplist="$HOME/Library/LaunchAgents/io.github.newtonharry.arr-update.plist"
		cat >"$uplist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>io.github.newtonharry.arr-update</string>
  <key>ProgramArguments</key><array><string>/bin/sh</string><string>$(xml "$UPDATER")</string></array>
  <key>StartCalendarInterval</key><dict><key>Hour</key><integer>4</integer><key>Minute</key><integer>$(($(od -An -N1 -tu1 /dev/urandom) % 60))</integer></dict>
  <key>StandardOutPath</key><string>$(xml "$STATE_DIR/update.log")</string>
  <key>StandardErrorPath</key><string>$(xml "$STATE_DIR/update.log")</string>
</dict></plist>
EOF
		launchctl unload "$uplist" 2>/dev/null || true
		launchctl load "$uplist" || die "couldn't schedule automatic updates"
		;;
	esac
	say "Arr checks for a new release once a day and installs it. Turn this off with --no-auto-update."
elif [ -f "$UPDATER" ]; then
	step "Turning off automatic updates"
	case "$kind" in
	system) systemctl disable --now arr-update.timer >/dev/null 2>&1 || true ;;
	user) systemctl --user disable --now arr-update.timer >/dev/null 2>&1 || true ;;
	launchd) launchctl unload "$HOME/Library/LaunchAgents/io.github.newtonharry.arr-update.plist" 2>/dev/null || true ;;
	esac
	rm -f "$UPDATER"
fi

API="http://127.0.0.1:$PORT"
i=0
until fetch -o /dev/null "$API/api/v1/health" 2>/dev/null; do
	i=$((i + 1))
	[ $i -lt 30 ] || die "Arr didn't start; see: $logs"
	sleep 1
done

# A config without Plex has nothing to import.
if grep -q '^\[plex\]' "$CONFIG"; then
	step "Learning what you already own from Plex"
	printf 'Authorization: Bearer %s\n' "$(cat "$STATE_DIR/secrets/api_token")" >"$tmp/api.header"
	# Import only the kinds this config has a library for (a movie-only setup has no TV run).
	media=all
	grep -q '^movies = ' "$CONFIG" || media=tv
	grep -q '^tv = ' "$CONFIG" || media=movies
	i=0
	while :; do
		fetch -X POST -H @"$tmp/api.header" -H "Content-Type: application/json" -d "{\"media\":\"$media\"}" \
			-o "$tmp/boot.json" "$API/api/v1/setup/plex/bootstrap" || die "Plex import failed; see: $logs"
		grep -q '"done": *true' "$tmp/boot.json" && break
		i=$((i + 1))
		[ $i -lt 500 ] || die "Plex import didn't finish; run this script again to continue it"
	done
	# "done" also covers a library whose import failed: report it instead of claiming success.
	if grep -q '"status": *"failed"' "$tmp/boot.json"; then
		err=$(sed -n 's/.*"last_error": *"\([^"]*\)".*/\1/p' "$tmp/boot.json" | head -n1)
		die "the Plex import failed${err:+: $err}; run this script again to retry"
	fi
	say "Done. Titles you already have are marked owned, and nothing was downloaded."
fi

cat <<EOF

Arr is running at $API
  API token:   $STATE_DIR/secrets/api_token
  Config:      $CONFIG
  Logs:        $logs
  Downloading: $([ "$DOWNLOADING" = 1 ] && echo on || echo "off (set [execution] enabled = true to turn it on)")

To use Arr from Claude Desktop, Claude Code or another MCP client on this machine, add:

{
  "mcpServers": {
    "arr": {
      "command": "$ARR",
      "args": ["mcp", "--api-url", "$API/", "--token-file", "$STATE_DIR/secrets/api_token"]
    }
  }
}
EOF

if [ -z "$PUBLIC_URL" ]; then
	cat <<EOF

To use Arr from ChatGPT on your phone or any device, run this script again with --remote.
It needs a free ngrok account and a Google OAuth client; see
https://github.com/$REPO/blob/main/docs/REMOTE.md
EOF
	exit 0
fi

step "Checking $PUBLIC_URL"
i=0
tries=$((${ARR_REMOTE_CHECK_SECONDS:-60} / 2)) # the variable is for tests
until fetch "$PUBLIC_URL/.well-known/oauth-protected-resource/mcp" 2>/dev/null | grep -q "\"$PUBLIC_URL/mcp\""; do
	i=$((i + 1))
	[ $i -lt "$tries" ] || die "$PUBLIC_URL doesn't reach Arr yet; the tunnel's log says why (an ERR_NGROK_... code names the problem, often a wrong authtoken or a domain from another account): $logs_tunnel"
	sleep 2
done
cat <<EOF
Arr is reachable at $PUBLIC_URL

Add it to ChatGPT once, on chatgpt.com in a browser; it then works in the ChatGPT app on
your phone and every other device signed in to your account:

  1. Settings > Apps & Connectors > Advanced: turn on Developer mode.
  2. Back in Apps & Connectors, choose Create and fill in:
       Name:            Arr
       MCP server URL:  $PUBLIC_URL/mcp
       Authentication:  OAuth
  3. Create, then sign in: approve Arr's page and pick your Google account.
  4. In a new chat, ask "what's in my library?"

ChatGPT's menu names change now and then; docs/REMOTE.md has the current steps.
EOF
