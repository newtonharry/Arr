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
SETUP=1
SERVICE=1
DOWNLOADING=1

usage() {
	cat <<'EOF'
Install Arr and set it up next to your Plex server.

Usage: install.sh [options]

  --yes                  answer every question with its default (no prompts)
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

Each option can also come from an environment variable: ARR_PLEX_TOKEN, ARR_MOVIES, ...
EOF
}

while [ $# -gt 0 ]; do
	case "$1" in
	--yes | -y) YES=1 ;;
	--install-only) SETUP=0 ;;
	--no-service) SERVICE=0 ;;
	--search-only) DOWNLOADING=0 ;;
	--version) VERSION="$2"; shift ;;
	--bin-dir) BIN_DIR="$2"; shift ;;
	--state-dir) STATE_DIR="$2"; shift ;;
	--plex-url) PLEX_URL="$2"; shift ;;
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
	-h | --help) usage; exit 0 ;;
	*) echo "Unknown option: $1 (see --help)" >&2; exit 2 ;;
	esac
	shift
done

say() { printf '%s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
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
install -m 755 "$tmp/arr-$target/arr" "$BIN_DIR/arr"
ARR="$BIN_DIR/arr"
say "Installed $("$ARR" --version) to $ARR"
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) say "Add $BIN_DIR to your PATH to run 'arr' directly." ;; esac

[ "$SETUP" = 1 ] || exit 0

# ---------------------------------------------------------------- ffprobe

step "Checking for ffprobe (Arr uses it to check every download before import)"
if ! have ffprobe; then
	sudo=""
	[ "$(id -u)" = 0 ] || sudo="sudo"
	if have apt-get; then cmd="$sudo apt-get install -y ffmpeg"
	elif have dnf; then cmd="$sudo dnf install -y ffmpeg-free"
	elif have pacman; then cmd="$sudo pacman -S --noconfirm ffmpeg"
	elif have apk; then cmd="$sudo apk add ffmpeg"
	elif have brew; then cmd="brew install ffmpeg"
	else cmd=""; fi
	[ -n "$cmd" ] || die "install FFmpeg (it provides ffprobe), then run this again"
	if confirm "ffprobe is missing. Install it now with '$cmd'?" y; then
		# shellcheck disable=SC2086
		$cmd </dev/null || die "could not install FFmpeg; install it yourself and run this again"
	else
		die "install FFmpeg (it provides ffprobe), then run this again"
	fi
fi
FFPROBE=$(command -v ffprobe)
say "Found $FFPROBE"

# ---------------------------------------------------------------- state directory

if [ -z "$STATE_DIR" ]; then
	if [ "$os" = Darwin ]; then STATE_DIR="$HOME/Library/Application Support/arr"
	elif [ "$(id -u)" = 0 ]; then STATE_DIR=/var/lib/arr
	else STATE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/arr"; fi
fi
CONFIG="$STATE_DIR/config.toml"
if [ -f "$CONFIG" ]; then
	say "Arr is already set up in $STATE_DIR. Only the binary was updated."
	say "To start over, move that folder aside and run this again."
	exit 0
fi

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
fetch -H "X-Plex-Token: $PLEX_TOKEN" -o "$tmp/sections.xml" "$PLEX_URL/library/sections" ||
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
	[ -d "$1" ] && { printf '%s' "$1"; return; }
	[ -n "$plex_container" ] || return 0
	docker inspect -f '{{range .Mounts}}{{.Destination}}	{{.Source}}{{println}}{{end}}' "$plex_container" 2>/dev/null |
		while IFS='	' read -r dst src; do
			[ -n "$dst" ] || continue
			case "$1" in "$dst" | "$dst"/*) printf '%s%s' "$src" "${1#"$dst"}"; break ;; esac
		done
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
mkdir -p "$DOWNLOADS" || die "can't create $DOWNLOADS"
for d in "$MOVIES" "$TV" "$DOWNLOADS"; do
	[ -z "$d" ] || [ -w "$d" ] || die "$(id -un) can't write to $d; fix its permissions or run as the user Plex files belong to"
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
} >"$CONFIG"
chmod 600 "$CONFIG"

# A config that fails its checks is moved aside, so running this again starts fresh.
failed() { mv "$CONFIG" "$CONFIG.failed"; die "$1 (the config is saved as $CONFIG.failed)"; }
"$ARR" setup --config "$CONFIG" >/dev/null || failed "arr setup failed; see the message above"
"$ARR" check --config "$CONFIG" >"$tmp/check.json" || { cat "$tmp/check.json"; failed "arr check failed"; }
say "Config checked"

# ---------------------------------------------------------------- run it

step "Starting Arr"
run_cmd="\"$ARR\" serve --config \"$CONFIG\""
started=0
if [ "$SERVICE" = 1 ] && [ "$os" = Linux ] && have systemctl && [ -d /run/systemd/system ]; then
	unit="[Unit]
Description=Arr media manager
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=\"$ARR\" serve --config \"$CONFIG\"
Restart=on-failure
RestartSec=5
UMask=0002
"
	if [ "$(id -u)" = 0 ]; then
		run_as=${SUDO_USER:-root}
		if [ "$run_as" != root ]; then
			chown -R "$run_as" "$STATE_DIR" "$DOWNLOADS"
			unit="$unit
User=$run_as"
		fi
		printf '%s\n[Install]\nWantedBy=multi-user.target\n' "$unit" >/etc/systemd/system/arr.service
		systemctl daemon-reload && systemctl enable --now arr >/dev/null 2>&1 && started=1
		logs="journalctl -u arr -f"
	else
		mkdir -p "$HOME/.config/systemd/user"
		printf '%s\n[Install]\nWantedBy=default.target\n' "$unit" >"$HOME/.config/systemd/user/arr.service"
		systemctl --user daemon-reload && systemctl --user enable --now arr >/dev/null 2>&1 && started=1
		logs="journalctl --user -u arr -f"
		have loginctl && loginctl enable-linger "$(id -un)" >/dev/null 2>&1 || true
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
    <string>$ARR</string><string>serve</string><string>--config</string><string>$CONFIG</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>$STATE_DIR/arr.log</string>
</dict></plist>
EOF
	launchctl unload "$plist" 2>/dev/null || true
	launchctl load "$plist" && started=1
	logs="tail -f \"$STATE_DIR/arr.log\""
fi
if [ "$started" != 1 ]; then
	say "No background service was installed. Start Arr with:"
	say "  $run_cmd"
	exit 0
fi

API="http://127.0.0.1:$PORT"
i=0
until fetch -o /dev/null "$API/api/v1/health" 2>/dev/null; do
	i=$((i + 1))
	[ $i -lt 30 ] || die "Arr didn't start; see: $logs"
	sleep 1
done

step "Learning what you already own from Plex"
token=$(cat "$STATE_DIR/secrets/api_token")
i=0
while :; do
	fetch -X POST -H "Authorization: Bearer $token" -H "Content-Type: application/json" -d '{}' \
		-o "$tmp/boot.json" "$API/api/v1/setup/plex/bootstrap" || die "Plex import failed; see: $logs"
	grep -q '"done": *true' "$tmp/boot.json" && break
	i=$((i + 1))
	[ $i -lt 500 ] || die "Plex import didn't finish; run it again later (see README)"
done
say "Done. Titles you already have are marked owned, and nothing was downloaded."

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

To use it from ChatGPT you need a public HTTPS address and Google sign-in; see
https://github.com/$REPO/blob/main/docs/MCP.md
EOF
