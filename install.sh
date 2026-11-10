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
# Which of these were given, so a run on an existing install changes only those.
PORT_SET=0; [ -z "${ARR_PORT:-}" ] || PORT_SET=1
TORRENT_PORT_SET=0; [ -z "${ARR_TORRENT_PORT:-}" ] || TORRENT_PORT_SET=1
UPLOAD_SET=0; [ -z "${ARR_UPLOAD_LIMIT_KIB:-}" ] || UPLOAD_SET=1
YES="${ARR_YES:-0}"
REMOTE="${ARR_REMOTE:-0}"
NGROK_TOKEN="${ARR_NGROK_TOKEN:-}"
NGROK_DOMAIN="${ARR_NGROK_DOMAIN:-}"
GOOGLE_ID="${ARR_GOOGLE_CLIENT_ID:-}"
GOOGLE_SECRET="${ARR_GOOGLE_CLIENT_SECRET:-}"
ALLOW_EMAILS="${ARR_ALLOW_EMAILS:-}"
AUTO_UPDATE="${ARR_AUTO_UPDATE:-}"
ERROR_REPORTS="${ARR_ERROR_REPORTS:-}"
AT_BOOT="${ARR_AT_BOOT:-}"
SETUP=1
FIRST_INSTALL=0
TERMINAL=0
SERVICE=1
DOWNLOADING=1

usage() {
	cat <<'EOF'
Install Arr and set it up next to your Plex server.

Usage: install.sh [options]

Running this again updates Arr and keeps your setup. Settings you leave out stay as they
are; a setting you give is applied, or refused before anything changes.

  --yes                  answer every question with its default (no prompts)
  --terminal             answer setup questions here instead of on a web page (first setup only)
  --install-only         install the binary and stop (no setup)
  --version vX.Y.Z       install a specific release (default: latest)
  --bin-dir DIR          where to put the arr binary
  --state-dir DIR        where Arr keeps its config and data
  --plex-url URL         Plex server address (default: http://127.0.0.1:32400)
  --plex-token TOKEN     Plex token (default: read from Plex's Preferences.xml if possible)
  --movies DIR           movie library folder as this machine sees it (first setup only)
  --tv DIR               TV library folder as this machine sees it (first setup only)
  --movie-section N      Plex movie library number (default: the first one; first setup only)
  --tv-section N         Plex TV library number (default: the first one; first setup only)
  --downloads DIR        where torrents download before import (first setup only)
  --tmdb-key KEY         optional backup: a TMDB API key or read token (movies come from Plex)
  --port N               API port (default: 8787; first setup only)
  --torrent-port N       BitTorrent port (default: 50413)
  --upload-limit-kib N   upload limit in KiB/s for all torrents (default: 0 = none)
  --search-only          don't let Arr download anything (turns downloading off)
  --no-service           don't install a background service
  --at-boot              start Arr when the computer starts, before anyone logs in. On a Mac
                         that is a launch daemon and needs your admin password once; on Linux
                         a system-wide install always does, and a per-user install turns on
                         lingering (sudo loginctl enable-linger). Asked at a terminal if not given.
  --no-at-boot           start Arr when you log in instead (it stays so until you run this again)
  --auto-update          install new releases automatically (a daily check); asked if not given
  --no-auto-update       don't, and turn automatic updates off if they were on
  --error-reports        send error reports, scrubbed of personal data (docs/PRIVACY.md);
                         asked in the terminal setup if not given (the web setup has a box)
  --no-error-reports     don't

Use Arr from ChatGPT on your phone or any device (docs/REMOTE.md). Run again with these to
add it to an existing install; once it's set up, change it in config.toml instead:

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
	--at-boot) AT_BOOT=1 ;;
	--no-at-boot) AT_BOOT=0 ;;
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
	--port) PORT="$2"; PORT_SET=1; shift ;;
	--torrent-port) TORRENT_PORT="$2"; TORRENT_PORT_SET=1; shift ;;
	--upload-limit-kib) UPLOAD_LIMIT_KIB="$2"; UPLOAD_SET=1; shift ;;
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
case "$AT_BOOT" in "" | 0 | 1) ;; *) echo "ARR_AT_BOOT must be 1 or 0, not '$AT_BOOT'" >&2; exit 2 ;; esac
# Remote settings from environment variables turn remote setup on, as their flags do.
[ -z "$NGROK_TOKEN$NGROK_DOMAIN$GOOGLE_ID$GOOGLE_SECRET$ALLOW_EMAILS" ] || REMOTE=1

say() { printf '%s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }
die() {
	printf 'error: %s\n' "$*" >&2
	undo_bin
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

# tmdb_key_ok KEY: whether TMDB accepts it (asked through arr; the key goes on stdin). If not,
# TMDB_WHY says why.
tmdb_key_ok() {
	TMDB_WHY=$(printf '%s' "$1" | "$ARR" tmdb-key --check 2>&1 >/dev/null) && return 0
	TMDB_WHY=${TMDB_WHY#Error: }
	return 1
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

# Arr's own release downloads: GitHub's download pages sometimes answer 504 for a long
# while, so these retry longer than fetch, then (GitHub's default address only) go through
# GitHub's API, which stays up. Parsed with tr and awk, as there is no jq.
rfetch() { curl -fsSL --retry 6 --retry-delay 10 "$@"; }
gh_api() { curl -fsSL --retry 3 -H 'Accept: application/vnd.github+json' "https://api.github.com/repos/$REPO/$1"; }
# The API address of release asset $2 in release $1 (latest, or tags/<tag>)
api_asset_url() {
	gh_api "releases/$1" | tr ',' '\n' | awk -v want="$2" '
		/\/releases\/assets\/[0-9]+"/ { u = $0; sub(/.*"https/, "https", u); sub(/".*/, "", u) }
		/"name": *"/ { v = $0; sub(/.*"name": *"/, "", v); sub(/".*/, "", v); if (v == want && u != "") { print u; exit } }'
}

# On a re-run the arr this run replaced stays as arr.previous, and config.toml and secrets/
# as they were stay in SNAP (inside this run's private temp folder), until the new one
# answers. A run that stops before then puts them back (restarting Arr if this run had
# restarted it), so a failed re-run leaves the install as it found it.
UNDO_BIN=0 RESTART="" SNAP="" KEEP=""
undo_bin() {
	undone=0
	if [ -n "${SNAP:-}" ]; then
		s=$SNAP; SNAP=""; failed=0
		# Settings first, so the Arr restarted below reads them as they were.
		if [ -d "$s/secrets" ]; then
			rm -rf "$STATE_DIR/secrets.undo" &&
				cp -Rp "$s/secrets" "$STATE_DIR/secrets.undo" && rm -rf "$STATE_DIR/secrets" &&
				mv "$STATE_DIR/secrets.undo" "$STATE_DIR/secrets" || failed=1
		fi
		cp -p "$s/config.toml" "$CONFIG" || failed=1
		# The managed settings pointer and the "Knaben was offered" note change with the config
		# (a re-run may add a search source), so they go back with it.
		for f in active-import.json default-indexers; do
			if [ -f "$s/$f" ]; then cp -p "$s/$f" "$STATE_DIR/$f" || failed=1; else rm -f "$STATE_DIR/$f"; fi
		done
		if [ "$failed" = 0 ]; then
			printf 'Put your previous settings back in %s.\n' "$CONFIG" >&2
			undone=1
		else
			# The only good copy: keep it (the temp folder stays) and say where it is.
			KEEP=$s
			printf 'warning: could not put your previous settings back; they are in %s (copy config.toml and secrets/ into %s)\n' "$s" "$STATE_DIR" >&2
		fi
	fi
	if [ "${UNDO_BIN:-0}" = 1 ]; then
		UNDO_BIN=0
		if mv -f "$BIN_DIR/arr.previous" "$BIN_DIR/arr"; then
			printf 'Put the previous version of Arr back in %s.\n' "$BIN_DIR" >&2
			undone=1
		fi
	fi
	[ "$undone" = 1 ] && [ -n "$RESTART" ] || return 0
	$RESTART >/dev/null 2>&1 || true
}
# An option this run can't apply to an existing install: with nobody to ask, stop; at a
# terminal, say so and carry on without it.
reject() {
	if interactive; then say "warning: $1; carrying on without it." >&2; else die "$1"; fi
}
# conf SECTION KEY: a value from config.toml, strings unquoted (wherever the key sits in it).
conf() {
	awk -v sec="[$1]" -v key="$2" '
		/^[ \t]*\[/ { h = $0; sub(/#.*/, "", h); gsub(/[ \t]/, "", h); cur = h; next }
		cur == sec {
			line = $0; sub(/^[ \t]+/, "", line)
			eq = index(line, "="); if (eq == 0) next
			k = substr(line, 1, eq - 1); sub(/[ \t]+$/, "", k)
			if (k != key) next
			v = substr(line, eq + 1); sub(/^[ \t]+/, "", v)
			q = substr(v, 1, 1); out = ""
			if (q == "\"") {
				for (i = 2; i <= length(v); i++) {
					c = substr(v, i, 1)
					if (c == "\\") { i++; c = substr(v, i, 1) } else if (c == "\"") break
					out = out c
				}
			} else if (q == "\047") { r = substr(v, 2); out = substr(r, 1, index(r, "\047") - 1) }
			else { out = v; sub(/[ \t]*(#.*)?$/, "", out) }
			print out; exit
		}' "$CONFIG" 2>/dev/null || true
}
conf_port() { p=$(conf server listen); p=${p##*:}; printf '%s' "${p:-8787}"; }
# Runs a command as the user Arr runs as (under sudo, not as root).
as_run() {
	if [ "$(id -u)" = 0 ] && [ "$RUN_AS" != root ]; then sudo -u "$RUN_AS" "$@"; else "$@"; fi
}
# "https://Name.ngrok-free.app/" -> name.ngrok-free.app
norm_domain() { printf '%s' "$1" | sed 's#^https\{0,1\}://##; s#/.*$##' | tr 'A-Z' 'a-z'; }
# "A@b.c, d@e.f" -> "a@b.c", "d@e.f" (a TOML list's insides)
toml_emails() {
	out=""
	for e in $(printf '%s' "$1" | tr ',' ' '); do
		out="$out${out:+, }$(toml "$(printf '%s' "$e" | tr 'A-Z' 'a-z')")"
	done
	printf '%s' "$out"
}
# Are the remote access values given the ones already set up? (Then there's nothing to change.)
remote_same() {
	[ -z "$NGROK_TOKEN" ] || [ "$NGROK_TOKEN" = "$(sed -n 's/^  authtoken: //p' "$STATE_DIR/ngrok.yml" 2>/dev/null)" ] || return 1
	[ -z "$NGROK_DOMAIN" ] || [ "https://$(norm_domain "$NGROK_DOMAIN")" = "$(conf mcp.oauth public_url)" ] || return 1
	[ -z "$GOOGLE_ID" ] || [ "$GOOGLE_ID" = "$(conf mcp.oauth google_client_id)" ] || return 1
	[ -z "$GOOGLE_SECRET" ] || [ "$GOOGLE_SECRET" = "$(cat "$STATE_DIR/secrets/google_client_secret" 2>/dev/null)" ] || return 1
	[ -z "$ALLOW_EMAILS" ] || [ "[$(toml_emails "$ALLOW_EMAILS")]" = "$(conf mcp.oauth allowed_emails)" ]
}
# The ngrok agent, next to arr. Sets NGROK.
install_ngrok() {
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
}
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
# On a Mac, Arr is a login item of yours, and root's files in your folders would lock you out.
[ "$os" != Darwin ] || [ "$(id -u)" != 0 ] || die "on a Mac, run this without sudo: Arr runs as you"

# ---------------------------------------------------------------- an existing install

# Running this again must update the Arr that's already here, not set up a second one next to
# it: its service says where its binary and config are, and which user it runs as. A folder
# given with --bin-dir or --state-dir (or ARR_BIN_DIR, ARR_STATE_DIR) wins over its own only.
BIN_GIVEN=$BIN_DIR STATE_GIVEN=$STATE_DIR
FOUND="" FOUND_BIN="" FOUND_CONFIG="" FOUND_USER=""
# Under sudo, HOME may be root's: a per-user Arr is in the home of whoever ran sudo.
U_HOME=$HOME
if [ "$(id -u)" = 0 ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
	case "$SUDO_USER" in *[!A-Za-z0-9._-]*) die "unexpected SUDO_USER: $SUDO_USER" ;; esac
	U_HOME=$( (getent passwd "$SUDO_USER" 2>/dev/null || true) | cut -d: -f6)
	[ -n "$U_HOME" ] || U_HOME=$(eval echo "~$SUDO_USER")
fi
# The same parsing as uninstall.sh: install.sh writes these files, quoted its way.
unquote() { sed 's/%%/%/g; s/\$\$/$/g; s/\\"/"/g; s/\\\\/\\/g'; }
from_unit() {
	line=$(sed -n 's/^ExecStart=//p' "$1" | head -n1)
	FOUND_BIN=$(printf '%s' "$line" | sed -n 's/^"\(\([^"\\]\|\\.\)*\)".*/\1/p' | unquote)
	FOUND_CONFIG=$(printf '%s' "$line" | sed -n 's/.* --config "\(\([^"\\]\|\\.\)*\)".*/\1/p' | unquote)
	FOUND_USER=$(sed -n 's/^User=//p' "$1" | head -n1)
}
from_plist() {
	strings=$(awk '{ while (match($0, /<string>[^<]*<\/string>/)) {
			print substr($0, RSTART + 8, RLENGTH - 17); $0 = substr($0, RSTART + RLENGTH) } }' "$1" |
		sed 's/&lt;/</g; s/&gt;/>/g; s/&quot;/"/g; s/&amp;/\&/g')
	# Only the layout this script writes: <arr> serve --config <config>.
	if [ "$(printf '%s\n' "$strings" | sed -n '3,4p' | tr '\n' ' ')" = "serve --config " ]; then
		FOUND_BIN=$(printf '%s\n' "$strings" | sed -n 2p)
		FOUND_CONFIG=$(printf '%s\n' "$strings" | sed -n 5p)
	fi
}
sys_unit=/etc/systemd/system/arr.service
user_unit="$U_HOME/.config/systemd/user/arr.service"
plist="$U_HOME/Library/LaunchAgents/io.github.newtonharry.arr.plist"
# At boot on a Mac, Arr is a launch daemon that runs as the installing user (UserName).
LD_DIR="${ARR_LAUNCH_DAEMONS_DIR:-/Library/LaunchDaemons}" # (the variable is for tests)
dplist="$LD_DIR/io.github.newtonharry.arr.plist"
CUR_BOOT=0
found_file=""
if [ "$os" = Darwin ]; then
	if [ -f "$dplist" ]; then
		from_plist "$dplist"; FOUND=launchd; found_file=$dplist; CUR_BOOT=1
		FOUND_USER=$(sed -n 's#.*<key>UserName</key><string>\([^<]*\)</string>.*#\1#p' "$dplist" | head -n1)
		[ "$FOUND_USER" = "$(id -un)" ] ||
			die "Arr is installed to start at boot for ${FOUND_USER:-another user} ($dplist); run this as that user"
	elif [ -f "$plist" ]; then
		from_plist "$plist"; FOUND=launchd; found_file=$plist
	fi
elif [ "$(id -u)" = 0 ]; then
	if [ -f "$sys_unit" ]; then
		from_unit "$sys_unit"; FOUND=system; found_file=$sys_unit
	elif [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ] && [ -f "$user_unit" ]; then
		# Whatever HOME sudo kept: U_HOME is the sudo user's own home, looked up above.
		from_unit "$user_unit"
		die "Arr is already installed for $SUDO_USER (${FOUND_CONFIG:+config $FOUND_CONFIG, }service $user_unit); run this again without sudo"
	fi
elif [ -f "$user_unit" ]; then
	from_unit "$user_unit"; FOUND=user; found_file=$user_unit
elif [ -f "$sys_unit" ]; then
	from_unit "$sys_unit"
	die "Arr is already installed system-wide (${FOUND_CONFIG:+config $FOUND_CONFIG, }service $sys_unit); run this again with sudo"
fi
# A service this script can't read (written by hand, say) is never replaced by a fresh setup.
if [ -n "$FOUND" ] && { { [ -z "$FOUND_BIN" ] && [ -z "$BIN_GIVEN" ]; } || { [ -z "$FOUND_CONFIG" ] && [ -z "$STATE_GIVEN" ]; }; }; then
	die "an Arr service not written by this installer exists ($found_file); pass --bin-dir and --state-dir to use it"
fi
[ -n "$BIN_DIR" ] || [ -z "$FOUND_BIN" ] || BIN_DIR=$(dirname "$FOUND_BIN")
[ -n "$STATE_DIR" ] || [ -z "$FOUND_CONFIG" ] || STATE_DIR=$(dirname "$FOUND_CONFIG")
if [ -z "$BIN_DIR" ]; then
	if [ "$(id -u)" = 0 ]; then BIN_DIR=/usr/local/bin; else BIN_DIR="$HOME/.local/bin"; fi
fi
if [ -z "$STATE_DIR" ]; then
	if [ "$os" = Darwin ]; then STATE_DIR="$HOME/Library/Application Support/arr"
	elif [ "$(id -u)" = 0 ]; then STATE_DIR=/var/lib/arr
	else STATE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/arr"; fi
fi
CONFIG="$STATE_DIR/config.toml"

# Under sudo Arr runs as the user who ran sudo. On a re-run it keeps running as whoever it
# runs as already (its service's User=, or who owns its folder), never as someone new.
RUN_AS=""
if [ "$(id -u)" = 0 ]; then
	RUN_AS=${SUDO_USER:-root}
	if [ -f "$CONFIG" ]; then
		if [ "$FOUND" = system ]; then was=${FOUND_USER:-root}
		else was=$(stat -c %U "$STATE_DIR" 2>/dev/null || true); [ "$was" != root ] || was=""; fi
		if [ -n "$was" ] && [ "$was" != "$RUN_AS" ]; then
			[ "$RUN_AS" = root ] || [ "$was" = root ] ||
				die "Arr runs as $was here, not $RUN_AS; run this again from $was's account, with sudo"
			RUN_AS=$was
		fi
	fi
fi

# ---------------------------------------------------------------- options on a re-run

# Settings left out stay as they are. A setting given is applied further down (arr set), or
# refused here, before anything changes.
# in_range VALUE MIN MAX: a whole number from MIN to MAX (the length check keeps test's
# arithmetic from overflowing).
in_range() {
	case "$1" in "" | *[!0-9]*) return 1 ;; esac
	[ "${#1}" -le 9 ] && [ "$1" -ge "$2" ] && [ "$1" -le "$3" ]
}
in_range "$UPLOAD_LIMIT_KIB" 0 4194303 ||
	die "--upload-limit-kib needs a whole number of KiB/s from 0 (no limit) to 4194303"
in_range "$TORRENT_PORT" 1 65535 || die "--torrent-port needs a port number from 1 to 65535"
in_range "$PORT" 1 65535 || die "--port needs a port number from 1 to 65535"
if [ "$SETUP" = 1 ] && [ -f "$CONFIG" ]; then
	first_only() { # first_only OPTION VALUE SECTION KEY
		[ -z "$2" ] || [ "$2" = "$(conf "$3" "$4")" ] ||
			reject "$1 only applies to the first setup: an existing install keeps it (README: \"Changing library folders, Plex libraries or Arr's port\")"
	}
	[ "$PORT_SET" = 0 ] || [ "$PORT" = "$(conf_port)" ] ||
		reject "--port only applies to the first setup: an existing install keeps it (README: \"Changing library folders, Plex libraries or Arr's port\")"
	first_only --movies "$MOVIES" library movies
	first_only --tv "$TV" library tv
	first_only --downloads "$DOWNLOADS" embedded download_dir
	first_only --movie-section "$MOVIE_SECTION" plex movie_section
	first_only --tv-section "$TV_SECTION" plex tv_section
	[ "$TERMINAL" = 0 ] || reject "--terminal only applies to the first setup, and Arr is set up already"
	# Settings for a part of Arr this install doesn't use (at a terminal: carry on without them).
	if { [ -n "$PLEX_TOKEN" ] || { [ "$PLEX_URL_SET" = 1 ] && [ "$PLEX_URL" != "$(conf plex base_url)" ]; }; } &&
		! grep -q '^\[plex\]' "$CONFIG"; then
		reject "--plex-token and --plex-url need Plex in Arr's config ([plex] in $CONFIG), and this install has none"
		PLEX_TOKEN="" PLEX_URL_SET=0
	fi
	if { [ "$UPLOAD_SET" = 1 ] || [ "$TORRENT_PORT_SET" = 1 ]; } && ! grep -q '^\[embedded\]' "$CONFIG"; then
		reject "--upload-limit-kib and --torrent-port need Arr's own downloader ([embedded] in $CONFIG), and this install doesn't use it"
		UPLOAD_SET=0 TORRENT_PORT_SET=0
	fi
	if grep -q '^\[mcp\.oauth\]' "$CONFIG" && ! remote_same; then
		reject "remote access is set up already; to change it, edit [mcp.oauth] in $CONFIG as docs/REMOTE.md explains, then run this again"
	fi
fi

VIA_API=0 GH_DEFAULT=0
if [ -z "$DOWNLOAD_BASE" ]; then
	GH_DEFAULT=1
	if [ "$VERSION" = latest ]; then
		DOWNLOAD_BASE="https://github.com/$REPO/releases/latest/download"
	else
		DOWNLOAD_BASE="https://github.com/$REPO/releases/download/$VERSION"
	fi
fi

step "Downloading Arr ($VERSION, $target)"
tmp=$(mktemp -d)
# A run stopped by any error after the swap below (set -e too) rolls back as die does. Once
# Arr answers, UNDO_BIN and SNAP are cleared and this only removes the temp folder; the
# script's exit status stays its own. A folder holding settings that couldn't be put back
# stays. dash skips EXIT on a signal, so Ctrl-C or a dropped SSH session exits through it.
trap 'rc=$?; trap "" INT TERM HUP; set +e; undo_bin; [ -n "$KEEP" ] || rm -rf "$tmp"; exit "$rc"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
asset="arr-$target.tar.gz"
# Release asset $1 into $2. If GitHub's download page won't answer (default address only),
# the same file comes through the API, and the next one goes straight there.
release_get() {
	if [ "$VIA_API" = 0 ] && rfetch -o "$2" "$DOWNLOAD_BASE/$1"; then return 0; fi
	[ "$GH_DEFAULT" = 1 ] || return 1
	if [ "$VIA_API" = 0 ]; then
		echo "GitHub's download page didn't answer; trying GitHub's API instead"
		VIA_API=1
	fi
	if [ "$VERSION" = latest ]; then rel=latest; else rel="tags/$VERSION"; fi
	api_url=$(api_asset_url "$rel" "$1") && [ -n "$api_url" ] || return 1
	curl -fsSL --retry 3 -H 'Accept: application/octet-stream' -o "$2" "$api_url"
}
release_get "$asset" "$tmp/$asset" || die "download failed: $DOWNLOAD_BASE/$asset"
release_get "$asset.sha256" "$tmp/$asset.sha256" || die "checksum download failed"
[ "$(sha256 "$tmp/$asset")" = "$(cut -d' ' -f1 "$tmp/$asset.sha256")" ] || die "checksum mismatch for $asset"
tar -xzf "$tmp/$asset" -C "$tmp"
# A binary that doesn't run here never replaces one that does.
"$tmp/arr-$target/arr" --version >/dev/null 2>&1 ||
	die "the downloaded Arr doesn't run on this machine ($target); nothing was changed"
mkdir -p "$BIN_DIR"
# The real folder, without symlinks: services and the updater run exactly the path checked.
BIN_DIR=$(cd "$BIN_DIR" && pwd -P)
ARR="$BIN_DIR/arr"
# On a re-run the old binary stays as arr.previous. It and the new one each go in with one
# rename, so nothing ever runs a half-written arr, and an old arr.previous that's still
# running can't block the copy ("Text file busy").
prev=0
if [ -f "$CONFIG" ] && [ -x "$ARR" ]; then
	rm -f "$ARR.previous.new"
	cp -p "$ARR" "$ARR.previous.new"
	mv -f "$ARR.previous.new" "$ARR.previous"
	prev=1
fi
cp "$tmp/arr-$target/arr" "$ARR.new"
chmod 755 "$ARR.new"
mv -f "$ARR.new" "$ARR"
UNDO_BIN=$prev
say "Installed $("$ARR" --version) to $ARR"
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) say "Add $BIN_DIR to your PATH to run 'arr' directly." ;; esac

if [ "$SETUP" != 1 ]; then
	UNDO_BIN=0
	case "$FOUND" in
	system) say "Restart Arr to use the new version: sudo systemctl restart arr" ;;
	user) say "Restart Arr to use the new version: systemctl --user restart arr" ;;
	launchd)
		if [ "$CUR_BOOT" = 1 ]; then
			say "Restart Arr to use the new version: sudo launchctl kickstart -k system/io.github.newtonharry.arr"
		else
			say "Restart Arr to use the new version: launchctl kickstart -k gui/$(id -u)/io.github.newtonharry.arr"
		fi ;;
	esac
	exit 0
fi

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

# ---------------------------------------------------------------- setup

# The service runs as RUN_AS, so check access as that user.
writable() { as_run test -w "$1"; }

# Finds Plex and your libraries, then writes and checks a new config. Skipped when a config
# already exists: running the installer again applies only the options given.
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
	[ "$(id -u)" != 0 ] || [ "$RUN_AS" = root ] || chown "$RUN_AS" "$DOWNLOADS"
fi
for d in "$MOVIES" "$TV" "$DOWNLOADS"; do
	[ -z "$d" ] || writable "$d" ||
		die "${RUN_AS:-$(id -un)} can't write to $d; fix its permissions or run as the user your media belongs to"
done

if [ -n "$TMDB_KEY" ]; then
	# Nothing is written yet, so a key TMDB refuses stops here with the reason.
	tmdb_key_ok "$TMDB_KEY" || die "$TMDB_WHY (the key from --tmdb-key or ARR_TMDB_KEY; see docs/TMDB.md)"
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
# More options: https://github.com/$REPO/blob/main/config/standalone.example.toml

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

[[indexer]]
id = 2
name = "Knaben"
kind = "knaben"
base_url = "https://api.knaben.org/"

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

# Still written so this config also works with the previous release (auto-update rollback),
# which requires it when downloading is on. "coexistence" is the only mode.
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
	# so it listens on this machine's network address; arr wizard refuses a public one and
	# explains how to reach the page over SSH instead. The link carries a secret code.
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
	[ "$ERROR_REPORTS" != 1 ] || set -- "$@" --error-reports
	# Remote access given up front: the page can show its address (it's set up after this).
	[ -z "$NGROK_DOMAIN" ] || set -- "$@" --public-url "https://$(norm_domain "$NGROK_DOMAIN")"
	# Arr runs as RUN_AS, so the page checks folders as that user too, and its files are theirs.
	if [ "$(id -u)" = 0 ] && [ "$RUN_AS" != root ]; then
		chown -R "$RUN_AS" "$STATE_DIR"
		# shellcheck disable=SC2086 # NAME=value words without spaces
		set -- sudo -u "$RUN_AS" env $DESKTOP_ENV "$@"
	fi
	# The page writes config.toml only once setup has succeeded, so a run that stops here
	# leaves nothing half-done.
	"$@" </dev/null || die "setup wasn't finished, so nothing was saved; run this again to start over"
	setup_choices
}

# The setup page's answer about automatic updates, used once (an option given wins).
setup_choices() {
	if [ -z "$AUTO_UPDATE" ] && [ -f "$STATE_DIR/setup-choices" ]; then
		AUTO_UPDATE=$(sed -n 's/^auto_update=\([01]\)$/\1/p' "$STATE_DIR/setup-choices")
	fi
	rm -f "$STATE_DIR/setup-choices"
}

# set_conf NAME [VALUE]: one setting through arr set, which checks it first and changes
# nothing if it's refused.
set_conf() {
	if out=$(as_run "$ARR" set --config "$CONFIG" "$@" 2>&1); then say "$out"; else reject "${out#Error: }"; fi
}
# set_secret NAME VALUE: the value goes to arr on stdin, never on a command line.
set_secret() {
	(umask 077 && printf '%s' "$2" >"$tmp/value")
	set_conf "$1" <"$tmp/value"
	rm -f "$tmp/value"
}

# Plex can stop accepting Arr's sign-in, for example after "sign out of all devices".
plex_signin() {
	rc=0
	out=$(as_run "$ARR" plex-signin --config "$CONFIG" --check 2>&1) || rc=$?
	case "$rc" in
	0) return 0 ;;
	1) ;;
	*) say "warning: ${out#Error: }; carrying on without checking Arr's Plex sign-in." >&2; return 0 ;;
	esac
	if ! interactive; then
		say "warning: Plex no longer accepts Arr's sign-in (for example after 'sign out of all devices')." >&2
		say "  Arr can't see your Plex libraries until you run this again in a terminal, or with --plex-token." >&2
		return 0
	fi
	step "Plex no longer accepts Arr's sign-in"
	say "That happens after 'sign out of all devices' in Plex, for example. Sign in again:"
	# The link opens as soon as arr prints it; arr's exit status comes last, on its own line.
	if { rc=0; as_run "$ARR" plex-signin --config "$CONFIG" </dev/null || rc=$?; echo "arr-exit $rc"; } | {
		rc=1
		while IFS= read -r line; do
			case "$line" in
			"arr-exit "*) rc=${line#arr-exit } ;;
			*": http"*) say "$line"; open_page "${line##* }" ;;
			*) say "$line" ;;
			esac
		done
		exit "$rc"
	}; then
		say "Arr is signed in to Plex again."
	else
		say "warning: the Plex sign-in didn't finish; Arr carries on without Plex. Run this again to retry." >&2
	fi
}

if [ -f "$CONFIG" ]; then
	step "Keeping your config in $STATE_DIR"
	# Arr's files stay its service user's and private, whatever happened to them since.
	[ "$(id -u)" != 0 ] || [ "$RUN_AS" = root ] || chown -R "$RUN_AS" "$STATE_DIR"
	chmod 700 "$STATE_DIR"
	chmod 600 "$CONFIG"
	if [ -d "$STATE_DIR/secrets" ]; then
		chmod 700 "$STATE_DIR/secrets"
		for f in "$STATE_DIR"/secrets/*; do [ ! -f "$f" ] || chmod 600 "$f"; done
	fi
	# Nothing restarts until the config passes with this version: until then the running Arr
	# keeps going as it was.
	if ! as_run "$ARR" check --config "$CONFIG" >"$tmp/check.json" 2>"$tmp/check.err"; then
		cat "$tmp/check.err" >&2
		why=$(sed -n '1s/^Error: //p' "$tmp/check.err")
		die "Arr's config has a problem${why:+: $why}. Arr keeps running as it was. Fix that in $CONFIG, or move the file aside and run this again to set Arr up from the start"
	fi
	PORT=$(conf_port)
	setup_choices
	# The settings as they are, for undo_bin to put back if this run stops before Arr answers.
	# The temp folder is private (mktemp -d); cp -p keeps each file's owner and mode.
	mkdir "$tmp/snap"
	cp -p "$CONFIG" "$tmp/snap/config.toml"
	for f in active-import.json default-indexers; do
		[ ! -f "$STATE_DIR/$f" ] || cp -p "$STATE_DIR/$f" "$tmp/snap/$f"
	done
	[ ! -d "$STATE_DIR/secrets" ] || cp -Rp "$STATE_DIR/secrets" "$tmp/snap/secrets"
	SNAP="$tmp/snap"
	# The address first: Plex checks the token (and Arr's sign-in) at the address saved.
	[ "$PLEX_URL_SET" = 0 ] || [ "$PLEX_URL" = "$(conf plex base_url)" ] || set_conf plex-url "$PLEX_URL"
	if [ -n "$PLEX_TOKEN" ]; then
		set_secret plex-token "$PLEX_TOKEN"
	elif grep -q '^\[plex\]' "$CONFIG"; then
		plex_signin
	fi
	# Movies come from the Plex sign-in. A TMDB key is an optional backup, added with
	# --tmdb-key or ARR_TMDB_KEY; an existing one is kept.
	[ -z "$TMDB_KEY" ] || set_secret tmdb-key "$TMDB_KEY"
	if [ -n "$TMDB_KEY" ] || [ "$(conf tmdb enabled)" = true ]; then
		say "Movies now come from your Plex sign-in; your TMDB key stays as a backup."
	else
		say "Movies now come from your Plex sign-in — no TMDB key needed."
	fi
	[ "$DOWNLOADING" = 1 ] || set_conf downloading off
	[ "$UPLOAD_SET" = 0 ] || set_conf upload-limit-kib "$UPLOAD_LIMIT_KIB"
	[ "$TORRENT_PORT_SET" = 0 ] || set_conf torrent-port "$TORRENT_PORT"
	case "$ERROR_REPORTS" in 1) set_conf error-reports on ;; 0) set_conf error-reports off ;; esac
	# Knaben next to The Pirate Bay, once (arr set default-indexers adds it only to an install
	# that still has the default source, and never again after it was taken out). Optional: a
	# failure carries on without it. Running setup again gives it its policy and keeps the
	# profiles and goals already there. Until Arr answers, a failed run puts the config, the
	# settings pointer and the note that Knaben was offered back as the snapshot above has them.
	cp -p "$CONFIG" "$tmp/config.before-indexers"
	if out=$(as_run "$ARR" set --config "$CONFIG" default-indexers 2>&1); then
		say "$out"
		if ! cmp -s "$tmp/config.before-indexers" "$CONFIG" && ! as_run "$ARR" setup --config "$CONFIG" >/dev/null 2>&1; then
			cp -p "$tmp/config.before-indexers" "$CONFIG"
			rm -f "$STATE_DIR/default-indexers"
			say "warning: arr setup didn't accept the new search source, so it was not added; Arr carries on as it was." >&2
		fi
	else
		say "warning: ${out#Error: }; carrying on without the new search source." >&2
	fi
else
	FIRST_INSTALL=1
	[ ! -f "$CONFIG.failed" ] || say "Setting Arr up from the start ($CONFIG.failed holds the last attempt, which failed)."
	if [ "$TERMINAL" != 1 ] && interactive && [ "$DOWNLOADING" = 1 ] && [ "$PLEX_URL_SET" = 0 ] &&
		[ -z "$PLEX_TOKEN$MOVIES$TV$MOVIE_SECTION$TV_SECTION$DOWNLOADS$TMDB_KEY" ]; then
		# Setup options on the command line (or --search-only) mean the terminal setup.
		browser_setup
	else
		configure
	fi
fi
DOWNLOADING=0
[ "$(conf execution enabled)" != true ] || DOWNLOADING=1

# ---------------------------------------------------------------- phone and ChatGPT access

# ChatGPT (on the web, your phone or any device) needs a public HTTPS address and a sign-in.
# A free ngrok domain provides the address; Google sign-in limits Arr to the accounts you list.

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
	NGROK_DOMAIN=$(norm_domain "$NGROK_DOMAIN")
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
	for e in $(printf '%s' "$ALLOW_EMAILS" | tr ',' ' '); do
		case "$e" in *[!A-Za-z0-9._%+@-]* | *@*@* | @* | *@) die "not an email address: $e" ;; *@*.*) ;; *) die "not an email address: $e" ;; esac
	done
	emails=$(toml_emails "$ALLOW_EMAILS")

	NGROK=$(command -v ngrok || true)
	[ -n "$NGROK" ] || install_ngrok
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
# Files written above as root (ngrok.yml, a secret) are the service user's too, with or
# without a service.
[ "$(id -u)" != 0 ] || [ "$RUN_AS" = root ] || chown -R "$RUN_AS" "$STATE_DIR"

# A configured tunnel runs as its own service next to Arr, and needs the ngrok agent.
PUBLIC_URL=""
logs_tunnel="the arr-tunnel service's log"
NGROK=${NGROK:-}
if [ -f "$STATE_DIR/ngrok.yml" ] && grep -q '^\[mcp\.oauth\]' "$CONFIG"; then
	PUBLIC_URL=$(sed -n 's/^public_url = "\(.*\)"$/\1/p' "$CONFIG" | head -n1)
	[ -n "$NGROK" ] || NGROK=$(command -v ngrok || true)
	[ -n "$NGROK" ] || [ ! -x "$BIN_DIR/ngrok" ] || NGROK="$BIN_DIR/ngrok"
	[ -n "$NGROK" ] || install_ngrok
fi

# ---------------------------------------------------------------- run it

step "Starting Arr"
run_cmd="\"$ARR\" serve --config \"$CONFIG\""
started=0
# systemd quoting: escape \ and " and double %; arguments (not the program) also expand $.
sd_exe() { printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g; s/%/%%/g')"; }
sd() { printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g; s/%/%%/g; s/\$/$$/g')"; }
xml() { printf '%s' "$1" | sed 's/&/\&amp;/g; s/</\&lt;/g; s/>/\&gt;/g; s/"/\&quot;/g'; }
# Runs one service manager command; if it fails, says which and where Arr's log is.
svc() {
	"$@" >"$tmp/svc.out" 2>&1 || { cat "$tmp/svc.out" >&2; die "'$*' failed; see: $logs"; }
}
sq() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
# Is there a terminal to ask for an administrator's password on?
have_tty() { [ -r /dev/tty ] && (: </dev/tty) 2>/dev/null; }
# A per-user systemd service runs while you're logged out, and starts at boot, only with
# lingering on. Many systems leave that to an administrator (over SSH, polkit says no), so:
# try without a password; then, at a terminal or with --at-boot, ask sudo once; otherwise
# say how.
enable_linger() {
	me=$(id -un)
	lingering() { loginctl show-user "$me" -p Linger 2>/dev/null | grep -q '=yes'; }
	if [ "$AT_BOOT" = 0 ]; then
		lingering || say "Not turning on lingering (--no-at-boot): Arr starts when you log in and stops when you log out."
		return 0
	fi
	have loginctl && loginctl --no-ask-password enable-linger "$me" >/dev/null 2>&1 || true
	lingering && return 0
	if have loginctl && have sudo && { [ "$AT_BOOT" = 1 ] || interactive; }; then
		say "Arr needs your administrator password once to start when the computer does:"
		say "  sudo loginctl enable-linger $me"
		# With --at-boot and nobody at a terminal, sudo must fail instead of waiting.
		if have_tty; then sudo loginctl enable-linger "$me" </dev/null || true
		else sudo -n loginctl enable-linger "$me" </dev/null || true; fi
	fi
	lingering && return 0
	say "Warning: Arr stops when you log out and won't start after a reboot. Fix it with:"
	say "  sudo loginctl enable-linger $me"
}
# ---- macOS. Arr (and its tunnel and updater) is a login item (a LaunchAgent in your
# ~/Library/LaunchAgents) or, to start when the Mac does, a launch daemon in /Library/LaunchDaemons
# with the same label that runs as you (UserName): the same process, files and log either way.
# Writing /Library takes an administrator, so only a run at a terminal, or --at-boot, does it.
# Every other run keeps whichever kind is installed and never asks for a password.
L_ARR=io.github.newtonharry.arr L_TUN=io.github.newtonharry.arr-tunnel L_UPD=io.github.newtonharry.arr-update
LA_DIR="$HOME/Library/LaunchAgents"
BOOT_NOW=$CUR_BOOT # how Arr runs when this script is done
ELEV_OK=0          # may this run ask for an administrator's password?
# mac_plist LABEL BOOT ARGUMENTS_XML OTHER_KEYS_XML. Label and ProgramArguments come first,
# in this order: install.sh and uninstall.sh (this version's and older ones) read them back.
mac_plist() {
	printf '<?xml version="1.0" encoding="UTF-8"?>\n'
	printf '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n'
	printf '<plist version="1.0"><dict>\n  <key>Label</key><string>%s</string>\n' "$1"
	printf '  <key>ProgramArguments</key><array>\n%s\n  </array>\n' "$3"
	# As a daemon it runs as you, with your home, not as root.
	[ "$2" != 1 ] || printf '  <key>UserName</key><string>%s</string>\n  <key>EnvironmentVariables</key><dict><key>HOME</key><string>%s</string></dict>\n' \
		"$(xml "$(id -un)")" "$(xml "$HOME")"
	printf '%s\n</dict></plist>\n' "$4"
}
mac_arr_plist() { # mac_arr_plist BOOT
	mac_plist "$L_ARR" "$1" "    <string>$(xml "$ARR")</string><string>serve</string><string>--config</string><string>$(xml "$CONFIG")</string>" \
		"  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>$(xml "$STATE_DIR/arr.log")</string>"
}
mac_tunnel_plist() { # mac_tunnel_plist BOOT
	mac_plist "$L_TUN" "$1" "    <string>$(xml "$NGROK")</string><string>http</string><string>127.0.0.1:$PORT</string>
    <string>--url=$(xml "$PUBLIC_URL")</string><string>--config</string><string>$(xml "$STATE_DIR/ngrok.yml")</string>
    <string>--log=stdout</string><string>--log-level=warn</string>" \
		"  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$(xml "$STATE_DIR/tunnel.log")</string>"
}
# Your login items: stop one, or stop it and delete it, or install one from a file.
agent_stop() { launchctl unload "$LA_DIR/$1.plist" 2>/dev/null || true; }
agent_remove() { [ ! -f "$LA_DIR/$1.plist" ] || { agent_stop "$1"; rm -f "$LA_DIR/$1.plist"; }; }
agent_load() { # agent_load LABEL PLIST_FILE
	mkdir -p "$LA_DIR"
	cp "$2" "$LA_DIR/$1.plist"
	agent_stop "$1"
	svc launchctl load "$LA_DIR/$1.plist"
}
# Administrator work is one script, kept in this shell's memory ($EL) and handed to
# `sudo /bin/sh -s` on its standard input: nothing is read from disk as root, so no other
# process of yours can swap in a file between the checks and the commands. The daemons' plists
# are written by that script itself (here-documents), root-owned; your folders are not used.
mac_sudo() {
	sudo -n true 2>/dev/null && return 0
	have_tty || { say "Arr needs an administrator (sudo) to start at boot, and there is no terminal to ask on."; return 1; }
	say "Arr needs your administrator password once to start when the computer does."
	sudo -v </dev/null
}
EL=""
el_start() {
	EL='set -e
umask 022
# launchctl bootstrap can fail for a moment right after a bootout of the same job.
bs() { i=0; while ! launchctl bootstrap system "$1" </dev/null; do i=$((i + 1)); [ "$i" -lt 8 ] || return 1; sleep 1; done; }'
}
el_add() { # el_add LABEL PLIST_TEXT: write the daemon, root-owned, and start it
	EL="$EL
printf '%s\n' $(sq "$2") >$(sq "$LD_DIR/$1.plist.new")
chown root:wheel $(sq "$LD_DIR/$1.plist.new")
chmod 644 $(sq "$LD_DIR/$1.plist.new")
mv -f $(sq "$LD_DIR/$1.plist.new") $(sq "$LD_DIR/$1.plist")
launchctl bootout system/$1 </dev/null 2>/dev/null || true
bs $(sq "$LD_DIR/$1.plist")"
}
el_remove() { # el_remove LABEL: stop the daemon and delete it
	EL="$EL
launchctl bootout system/$1 </dev/null 2>/dev/null || true
rm -f $(sq "$LD_DIR/$1.plist")"
}
elevate() { printf '%s\n' "$EL" | sudo /bin/sh -s; }
# Your own `arr serve` for this config (not `arr mcp`, which Claude Desktop may be running).
arr_pids() {
	ps -u "$(id -u)" -o pid= -o command= | awk -v want="$ARR serve --config $CONFIG" '
		{ p = $1; sub(/^[ ]*[0-9]+[ ]+/, ""); if ($0 == want) print p }'
}
# Restarts Arr without an administrator: the daemon runs as you with KeepAlive, so ending your
# own process makes launchd start it again from the binary on disk. (A user can't launchctl
# kickstart or unload a system daemon.)
boot_restart() {
	pids=$(arr_pids)
	[ -n "$pids" ] || return 0
	# shellcheck disable=SC2086 # process numbers
	kill $pids 2>/dev/null || true
	# SIGTERM returns before Arr has exited (a busy one finishes its worker pass first, up to
	# two minutes), and launchd starts the replacement only then: wait for the old one (at most
	# 150 s) so the health check's 30 s don't start counting before that.
	i=0
	while [ "$i" -lt 150 ]; do
		alive=0
		for p in $pids; do kill -0 "$p" 2>/dev/null && alive=1; done
		[ "$alive" = 1 ] || break
		i=$((i + 1))
		sleep 1
	done
}
# Once unloaded, a login item can't be kickstarted: a rollback loads it again instead.
launchd_restart() { agent_stop "$L_ARR"; launchctl load "$LA_DIR/$L_ARR.plist"; }
mac_service() {
	ELEVATED=0
	if [ -n "$AT_BOOT" ]; then BOOT_NOW=$AT_BOOT
	elif [ "$CUR_BOOT" = 1 ]; then BOOT_NOW=1
	elif interactive && confirm "Start Arr when this computer starts, even before anyone logs in? It needs your admin password once." y; then BOOT_NOW=1
	else BOOT_NOW=0; fi
	if [ "$AT_BOOT" = 1 ] || interactive; then ELEV_OK=1; fi
	D_ARR="$LD_DIR/$L_ARR.plist" D_TUN="$LD_DIR/$L_TUN.plist"

	# Back to a login item (--no-at-boot on a start-at-boot install): removing daemons takes an administrator.
	if [ "$BOOT_NOW" = 0 ] && [ "$CUR_BOOT" = 1 ]; then
		if [ "$ELEV_OK" = 1 ] && mac_sudo; then
			el_start; el_remove "$L_ARR"; el_remove "$L_TUN"; el_remove "$L_UPD"
			if elevate; then CUR_BOOT=0; else say "Warning: couldn't remove Arr's start-at-boot services; Arr keeps starting at boot."; BOOT_NOW=1; fi
		else
			say "Arr starts at boot, and taking that away needs an administrator: run this again at a terminal with --no-at-boot."
			BOOT_NOW=1
		fi
	fi

	if [ "$BOOT_NOW" = 1 ]; then
		# launchd opens the logs as root: create them as you first, so they stay yours.
		for f in arr.log tunnel.log update.log; do [ -e "$STATE_DIR/$f" ] || : >"$STATE_DIR/$f"; done
		arr_d=$(mac_arr_plist 1)
		need=0
		printf '%s\n' "$arr_d" | cmp -s - "$D_ARR" 2>/dev/null || need=1
		if [ -n "$PUBLIC_URL" ]; then
			tun_d=$(mac_tunnel_plist 1)
			printf '%s\n' "$tun_d" | cmp -s - "$D_TUN" 2>/dev/null || need=1
		elif [ -f "$D_TUN" ]; then need=1; fi
		if [ "$need" = 1 ] && [ "$ELEV_OK" = 1 ]; then
			ok=0
			if mac_sudo; then
				# The login items stop first, so the daemons get the port.
				if [ "$CUR_BOOT" = 0 ]; then agent_stop "$L_ARR"; agent_stop "$L_TUN"; fi
				el_start; el_add "$L_ARR" "$arr_d"
				if [ -n "$PUBLIC_URL" ]; then el_add "$L_TUN" "$tun_d"; elif [ -f "$D_TUN" ]; then el_remove "$L_TUN"; fi
				if elevate; then ok=1; fi
			fi
			if [ "$ok" = 1 ]; then ELEVATED=1; CUR_BOOT=1
			elif [ "$CUR_BOOT" = 1 ]; then
				say "Warning: couldn't update Arr's start-at-boot services; they stay as they were."
			else
				# Nothing may be left half-installed, and Arr must run: the login items come back.
				el_start; el_remove "$L_ARR"; el_remove "$L_TUN"; { sudo -n true && elevate; } >/dev/null 2>&1 || true
				RESTART=launchd_restart
				for l in "$L_ARR" "$L_TUN"; do
					[ ! -f "$LA_DIR/$l.plist" ] || launchctl load "$LA_DIR/$l.plist" 2>/dev/null || true
				done
				[ "$AT_BOOT" != 1 ] || die "couldn't set up start at boot; Arr keeps starting when you log in"
				say "Warning: couldn't set up start at boot (it needs an administrator); Arr starts when you log in."
				BOOT_NOW=0
			fi
		fi
	fi

	if [ "$BOOT_NOW" = 1 ]; then
		# The daemons are Arr's now: two copies would fight over the port.
		agent_remove "$L_ARR"
		RESTART=boot_restart
		# A daemon just started already runs the new binary; otherwise end Arr so launchd starts it again.
		[ "$ELEVATED" = 1 ] || boot_restart
		if [ -n "$PUBLIC_URL" ] && [ ! -f "$D_TUN" ]; then
			say "Note: the tunnel starts when you log in; to start it at boot, run this again at a terminal (it needs an administrator)."
			mac_tunnel_plist 0 >"$tmp/tun.a.plist"
			agent_load "$L_TUN" "$tmp/tun.a.plist"
		elif [ -n "$PUBLIC_URL" ]; then agent_remove "$L_TUN"
		else
			agent_remove "$L_TUN"
			[ ! -f "$D_TUN" ] || say "Warning: Arr's tunnel service ($D_TUN) is still installed; run this again at a terminal to remove it."
		fi
		return 0
	fi

	mkdir -p "$LA_DIR"
	mac_arr_plist 0 >"$tmp/arr.a.plist"
	cp "$tmp/arr.a.plist" "$LA_DIR/$L_ARR.plist"
	RESTART=launchd_restart
	agent_stop "$L_ARR"
	svc launchctl load "$LA_DIR/$L_ARR.plist"
	if [ -n "$PUBLIC_URL" ]; then
		mac_tunnel_plist 0 >"$tmp/tun.a.plist"
		agent_load "$L_TUN" "$tmp/tun.a.plist"
	else
		# Remote access is gone from the config, so its tunnel goes too.
		agent_remove "$L_TUN"
	fi
}

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
		sdir=/etc/systemd/system sctl=systemctl wanted=multi-user.target
		logs="journalctl -u arr -f"
		logs_tunnel="journalctl -u arr-tunnel"
		if [ "$RUN_AS" != root ]; then
			chown -R "$RUN_AS" "$STATE_DIR"
			unit="$unit
User=$RUN_AS"
			tunnel="$tunnel
User=$RUN_AS"
		fi
	else
		sdir="$HOME/.config/systemd/user" sctl="systemctl --user" wanted=default.target
		logs="journalctl --user -u arr -f"
		logs_tunnel="journalctl --user -u arr-tunnel"
	fi
	mkdir -p "$sdir"
	printf '%s\n[Install]\nWantedBy=%s\n' "$unit" "$wanted" >"$sdir/arr.service"
	if [ -n "$PUBLIC_URL" ]; then
		printf '%s\n[Install]\nWantedBy=%s\n' "$tunnel" "$wanted" >"$sdir/arr-tunnel.service"
	elif [ -f "$sdir/arr-tunnel.service" ]; then
		# Remote access is gone from the config, so its tunnel goes too.
		$sctl disable --now arr-tunnel >/dev/null 2>&1 || true
		rm -f "$sdir/arr-tunnel.service"
	fi
	# shellcheck disable=SC2086 # $sctl is "systemctl" or "systemctl --user"
	{
		svc $sctl daemon-reload
		svc $sctl enable arr
		RESTART="$sctl restart arr"
		svc $sctl restart arr
		if [ -n "$PUBLIC_URL" ]; then
			svc $sctl enable arr-tunnel
			svc $sctl restart arr-tunnel
		fi
	}
	started=1
	[ "$(id -u)" = 0 ] || enable_linger
elif [ "$SERVICE" = 1 ] && [ "$os" = Darwin ]; then
	logs="tail -f \"$STATE_DIR/arr.log\""
	logs_tunnel="$STATE_DIR/tunnel.log"
	mac_service
	started=1
fi
if [ "$started" != 1 ]; then
	UNDO_BIN=0 SNAP=""
	say "No background service was installed. Start Arr with:"
	say "  $run_cmd"
	[ -z "$PUBLIC_URL" ] ||
		say "  \"$NGROK\" http 127.0.0.1:$PORT --url=$PUBLIC_URL --config \"$STATE_DIR/ngrok.yml\""
	exit 0
fi

# A new version that doesn't answer is swapped back for the one that did.
API="http://127.0.0.1:$PORT"
healthy() {
	i=0
	until fetch --max-time 3 -o /dev/null "$API/api/v1/health" 2>/dev/null; do
		i=$((i + 1))
		[ $i -lt 30 ] || return 1
		sleep 1
	done
}
if ! healthy; then
	if [ "$UNDO_BIN" = 1 ]; then
		undo_bin
		if healthy; then die "the new version of Arr didn't start, so the previous one is running again; see: $logs"; fi
		die "the new version of Arr didn't start, and the previous one didn't either; see: $logs"
	fi
	die "Arr didn't start; see: $logs"
fi
UNDO_BIN=0 SNAP=""
rm -rf "$tmp/snap"

# ---------------------------------------------------------------- automatic updates

# A daily job installs new releases. It runs a local updater written here, never a script
# fetched from GitHub, so it trusts only the release binaries CI builds from version tags
# (checked by SHA-256), and it rolls back if the new version doesn't start.
if [ "$os" = Darwin ]; then kind=launchd; elif [ "$(id -u)" = 0 ]; then kind=system; else kind=user; fi
UPDATER="$BIN_DIR/arr-update"
case "$kind" in
system) udir=/etc/systemd/system sctl="systemctl" ;;
*) udir="$HOME/.config/systemd/user" sctl="systemctl --user" ;;
esac
uplist="$HOME/Library/LaunchAgents/io.github.newtonharry.arr-update.plist"
udplist="$LD_DIR/io.github.newtonharry.arr-update.plist"
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
		printf 'REPO=%s\nARR=%s\nTARGET=%s\nKIND=%s\nPORT=%s\nLOCK_KIND=%s\nLOCK_TOOL=%s\nCONFIG=%s\nLD_DIR=%s\nSTATE_DIR=%s\nOWNER=%s\n' \
			"$(sq "$REPO")" "$(sq "$ARR")" "$(sq "$target")" "$(sq "$kind")" "$(sq "$PORT")" \
			"$(sq "$lock_kind")" "$(sq "$lock_tool")" "$(sq "$CONFIG")" "$(sq "$LD_DIR")" "$(sq "$STATE_DIR")" \
			"$(sq "$({ [ "$(id -u)" = 0 ] && printf '%s' "$RUN_AS"; } || id -un)")"
		cat <<'EOF'
set -eu
log() { printf 'arr-update: %s\n' "$*"; }
# arr-update-trigger: the web app's "Update now" starts this by creating a request file in the
# state folder (a systemd path unit, or the launchd job's QueueDirectories, watches for it). The
# file goes first thing, so a run that fails can't start itself again, and update-status.json
# says how it went.
#
# The state folder belongs to the account that runs Arr (OWNER, fixed when this was written, never
# looked up from the folder), and this may run as root (a system install). Root therefore touches
# nothing in that folder: whatever is done there is done as OWNER, through setpriv or runuser, so a
# link or a swapped folder could only reach what that account could already change anyway. With
# neither tool, or with no OWNER, nothing is done: no status (the page treats it as unknown).
# owner_sh SCRIPT ARG: runs `sh -c SCRIPT sh ARG` as OWNER.
owner_sh() {
	if [ "$(id -u)" != 0 ]; then sh -c "$1" sh "$2"
	elif [ -z "$OWNER" ]; then return 1
	elif [ "$OWNER" = root ]; then sh -c "$1" sh "$2"
	elif command -v setpriv >/dev/null 2>&1; then
		setpriv --reuid="$OWNER" --regid="$(id -g "$OWNER")" --init-groups sh -c "$1" sh "$2"
	elif command -v runuser >/dev/null 2>&1; then runuser -u "$OWNER" -- sh -c "$1" sh "$2"
	else return 1; fi
}
STATUS_FILE="$STATE_DIR/update-status.json"
# The request goes by its one fixed name: no globbing, no walking a folder, a link is unlinked and
# never followed (left in place it would start this again and again).
if [ "$KIND" = launchd ]; then TRIGGER="$STATE_DIR/update-queue/requested"; else TRIGGER="$STATE_DIR/update-requested"; fi
owner_sh 'if [ -f "$1" ] || [ -L "$1" ]; then rm -f "$1"; fi' "$TRIGGER" 2>/dev/null || true
# status STATE FROM TO: a new file renamed over the old one, so Arr never reads half of it.
status() {
	[ "$(id -u)" != 0 ] || { [ -n "$OWNER" ] && [ "$OWNER" != root ]; } || return 0
	json=$(printf '{"state":"%s","from":"%s","to":"%s","at":"%s"}' "$1" "${2#v}" "${3#v}" "$(date -u +%Y-%m-%dT%H:%M:%SZ)")
	printf '%s\n' "$json" | owner_sh 'd=$(dirname "$1"); f=$(mktemp "$d/.update-status.XXXXXX") || exit 0; cat >"$f" && chmod 644 "$f" && mv -f "$f" "$1" || rm -f "$f"' "$STATUS_FILE" 2>/dev/null || true
	return 0
}
# Whatever ends the run without saying how it went was a failure.
FINAL=0
finish() { [ "$FINAL" = 1 ] || status failed "${current:-}" "${latest:-}"; }
rfetch() { curl -fsSL --retry 6 --retry-delay 10 "$@"; }
# GitHub's download pages sometimes answer 504 for a long while; its API stays up. Parsed with
# tr and awk, as there is no jq.
gh_api() { curl -fsSL --retry 3 -H 'Accept: application/vnd.github+json' "https://api.github.com/repos/$REPO/$1"; }
api_asset_url() { # release $1 (latest, or tags/<tag>), asset $2
	gh_api "releases/$1" | tr ',' '\n' | awk -v want="$2" '
		/\/releases\/assets\/[0-9]+"/ { u = $0; sub(/.*"https/, "https", u); sub(/".*/, "", u) }
		/"name": *"/ { v = $0; sub(/.*"name": *"/, "", v); sub(/".*/, "", v); if (v == want && u != "") { print u; exit } }'
}
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
	launchd)
		if [ -f "$LD_DIR/io.github.newtonharry.arr.plist" ]; then
			# Started at boot: Arr is a daemon that runs as you, and a user can't launchctl a
			# system daemon. Ending your own arr makes launchd (KeepAlive) start it again from
			# the binary now on disk.
			pids=$(ps -u "$(id -u)" -o pid= -o command= | awk -v want="$ARR serve --config $CONFIG" '
				{ p = $1; sub(/^[ ]*[0-9]+[ ]+/, ""); if ($0 == want) print p }')
			for p in $pids; do kill "$p" 2>/dev/null || true; done
			# SIGTERM returns before Arr has exited (a busy one finishes its worker pass first, up
			# to two minutes), and launchd starts the replacement only then: wait for the old one
			# (at most 150 s) before the health check's 30 s begin.
			i=0
			while [ "$i" -lt 150 ]; do
				alive=0
				for p in $pids; do kill -0 "$p" 2>/dev/null && alive=1; done
				[ "$alive" = 1 ] || break
				i=$((i + 1))
				sleep 1
			done
		else
			launchctl kickstart -k "gui/$(id -u)/io.github.newtonharry.arr" || true
		fi ;;
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
	trap 'finish; rm -rf ${tmp:+"$tmp"}' EXIT
	;;
shlock)
	"$LOCK_TOOL" -f "$lock" -p $$ || { log "another update is running"; exit 0; }
	trap 'finish; rm -f "$lock"; rm -rf ${tmp:+"$tmp"}' EXIT
	;;
*)
	log "unknown lock kind $LOCK_KIND; not updating"
	exit 1
	;;
esac
status checking "" ""

GH_DEFAULT=0 VIA_API=0
[ -n "${ARR_RELEASES_URL:-}" ] || GH_DEFAULT=1
RELEASES=${ARR_RELEASES_URL:-https://github.com/$REPO/releases}
latest=$(curl -fsSLI --retry 6 --retry-delay 10 -o /dev/null -w '%{url_effective}' "$RELEASES/latest" | sed 's#.*/tag/##') || latest=
case "$latest" in v[0-9]*) ;; *)
	if [ "$GH_DEFAULT" = 1 ]; then
		log "GitHub's download page didn't answer; trying GitHub's API instead"
		VIA_API=1
		latest=$(gh_api releases/latest | tr ',' '\n' | awk '/"tag_name": *"/ { v = $0; sub(/.*"tag_name": *"/, "", v); sub(/".*/, "", v); print v; exit }') || latest=
	fi ;;
esac
case "$latest" in v[0-9]*) ;; *) log "couldn't find the latest release"; exit 1 ;; esac
current="v$("$ARR" --version | awk '{print $2}')"
if ! newer "$latest" "$current"; then
	log "up to date ($current)"
	status up_to_date "$current" "$latest"
	FINAL=1
	exit 0
fi
log "updating $current to $latest"
status updating "$current" "$latest"
tmp=$(mktemp -d)
asset="arr-$TARGET.tar.gz"
base="$RELEASES/download/$latest"
release_get() { # asset $1 into $2; through the API if GitHub's download page won't answer
	if [ "$VIA_API" = 0 ] && rfetch -o "$2" "$base/$1"; then return 0; fi
	[ "$GH_DEFAULT" = 1 ] || return 1
	if [ "$VIA_API" = 0 ]; then
		log "GitHub's download page didn't answer; trying GitHub's API instead"
		VIA_API=1
	fi
	api_url=$(api_asset_url "tags/$latest" "$1") && [ -n "$api_url" ] || return 1
	curl -fsSL --retry 3 -H 'Accept: application/octet-stream' -o "$2" "$api_url"
}
release_get "$asset" "$tmp/$asset"
release_get "$asset.sha256" "$tmp/$asset.sha256"
if [ "$(sha256 "$tmp/$asset")" != "$(cut -d' ' -f1 "$tmp/$asset.sha256")" ]; then
	log "checksum mismatch; not updating"
	exit 1
fi
tar -xzf "$tmp/$asset" -C "$tmp"
"$tmp/arr-$TARGET/arr" --version >/dev/null || { log "$latest doesn't run on this machine; not updating"; exit 1; }
# Swap the binary in one rename, keeping the old one for a rollback (copied under a fresh
# name first: an old arr.previous may still be running).
rm -f "$ARR.previous.new"
cp -p "$ARR" "$ARR.previous.new"
mv -f "$ARR.previous.new" "$ARR.previous"
cp "$tmp/arr-$TARGET/arr" "$ARR.new"
chmod 755 "$ARR.new"
mv -f "$ARR.new" "$ARR"
restart
if healthy "$latest"; then
	log "now running $latest"
	status done "$current" "$latest"
	FINAL=1
	exit 0
fi
log "$latest didn't start; going back to $current"
mv -f "$ARR.previous" "$ARR"
restart
healthy "$current" || log "$current didn't start either; check Arr's logs"
status failed "$current" "$latest"
FINAL=1
# Told to Arr's developer only if the config says so (docs/PRIVACY.md).
"$ARR" report --config "$CONFIG" --component updater --message "$latest didn't start, so Arr went back to $current" >/dev/null 2>&1 || true
exit 1
EOF
	} >"$tmp/arr-update"
	install -m 755 "$tmp/arr-update" "$UPDATER"
	case "$kind" in
	system | user)
		printf '[Unit]\nDescription=Update Arr to the latest release\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=oneshot\nExecStart=/bin/sh %s\n' \
			"$(sd "$UPDATER")" >"$udir/arr-update.service"
		printf '[Unit]\nDescription=Check for Arr updates once a day\n\n[Timer]\nOnCalendar=daily\nRandomizedDelaySec=6h\nPersistent=true\n\n[Install]\nWantedBy=timers.target\n' \
			>"$udir/arr-update.timer"
		$sctl daemon-reload && $sctl enable --now arr-update.timer >/dev/null 2>&1 ||
			die "couldn't start the update timer"
		# "Update now" in the web app: Arr creates this file (it can't start the updater itself;
		# for a system install the updater runs as root and Arr doesn't), and systemd starts the
		# updater when it appears.
		printf '[Unit]\nDescription=Start the Arr update when the web app asks for it\n\n[Path]\nPathExists=%s/update-requested\nUnit=arr-update.service\n\n[Install]\nWantedBy=paths.target\n' \
			"$(printf '%s' "$STATE_DIR" | sed 's/%/%%/g')" >"$udir/arr-update.path"
		if ! { $sctl daemon-reload && $sctl enable --now arr-update.path >/dev/null 2>&1; }; then
			rm -f "$udir/arr-update.path"
			$sctl daemon-reload || true
			say "Note: the web app's \"Update now\" isn't available (systemd wouldn't start its watcher); updates still run once a day."
		fi
		;;
	launchd)
		upd_args="    <string>/bin/sh</string><string>$(xml "$UPDATER")</string>"
		upd_keys="  <key>StartCalendarInterval</key><dict><key>Hour</key><integer>4</integer><key>Minute</key><integer>$(($(od -An -N1 -tu1 /dev/urandom) % 60))</integer></dict>
  <key>StandardOutPath</key><string>$(xml "$STATE_DIR/update.log")</string>
  <key>StandardErrorPath</key><string>$(xml "$STATE_DIR/update.log")</string>
  <key>QueueDirectories</key><array><string>$(xml "$STATE_DIR/update-queue")</string></array>"
		# The web app's "Update now" puts a file in this folder; launchd starts the job while
		# there is one.
		mkdir -p "$STATE_DIR/update-queue"
		# At boot the daily job is a daemon too: it runs as you, so it needs no password to
		# run, and it still runs when nobody is logged in. Otherwise it is a login item.
		upd_d=$(mac_plist "$L_UPD" 1 "$upd_args" "$upd_keys")
		mac_plist "$L_UPD" 0 "$upd_args" "$upd_keys" >"$tmp/upd.a.plist"
		if [ "$BOOT_NOW" = 1 ] && [ -f "$udplist" ]; then
			# Already a daemon; it stays (changing it would take an administrator).
			agent_remove "$L_UPD"
		elif [ "$BOOT_NOW" = 1 ] && [ "$ELEV_OK" = 1 ] && mac_sudo && { el_start; el_add "$L_UPD" "$upd_d"; elevate; }; then
			agent_remove "$L_UPD"
		else
			[ "$BOOT_NOW" != 1 ] ||
				say "Note: automatic updates run while you're logged in; to run them at boot too, run this again at a terminal (it needs an administrator)."
			agent_load "$L_UPD" "$tmp/upd.a.plist"
		fi
		;;
	esac
	say "Arr checks for a new release once a day and installs it. Turn this off with --no-auto-update."
elif [ -f "$UPDATER" ] || [ -f "$udir/arr-update.timer" ] || [ -f "$uplist" ] || [ -f "$udplist" ]; then
	# Whatever is left of them: the daily job without its updater would only fail.
	step "Turning off automatic updates"
	case "$kind" in
	system | user)
		$sctl disable --now arr-update.path >/dev/null 2>&1 || true
		$sctl disable --now arr-update.timer >/dev/null 2>&1 || true
		rm -f "$udir/arr-update.path" "$udir/arr-update.timer" "$udir/arr-update.service"
		$sctl daemon-reload || true
		;;
	launchd)
		agent_remove "$L_UPD"
		if [ -f "$udplist" ]; then
			# A daemon: removing it takes an administrator. Without one it stays, and finds
			# no updater to run.
			if [ "$ELEV_OK" = 1 ] && mac_sudo && { el_start; el_remove "$L_UPD"; elevate; }; then :
			else say "Note: the daily update job ($udplist) is still installed but does nothing; run this again at a terminal to remove it."; fi
		fi
		;;
	esac
	rm -f "$UPDATER"
fi

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

echo
echo "Arr is running. Open it at:"
echo "  this computer: http://localhost:$PORT"
[ -z "$PUBLIC_URL" ] || echo "  any device:    $PUBLIC_URL   (sign in with Google)"
# A first install has nobody signed in yet: a link that works once, for 15 minutes. Anyone
# who has it can sign in, so it is made only when a person is at the terminal (not --yes, not a
# pipe or a log), and goes to the terminal itself. (With remote access it would name the public
# address, which may not be reachable yet, so it names this computer.)
if [ "$FIRST_INSTALL" = 1 ] && interactive && [ -t 1 ]; then
	link=$(as_run "$ARR" app-link --config "$CONFIG" 2>/dev/null | sed "s#^https://[^/]*#http://localhost:$PORT#") || link=""
	if [ -n "$link" ]; then
		{
			echo
			echo "To sign in on this computer, open this link. It works once, within 15 minutes:"
			echo "  $link"
		} >/dev/tty
	fi
fi
cat <<EOF

Arr API: $API
  API token:   $STATE_DIR/secrets/api_token
  Config:      $CONFIG
  Logs:        $logs
  Downloading: $([ "$DOWNLOADING" = 1 ] && echo on || echo "off (turn it on: \"$ARR\" set --config \"$CONFIG\" downloading on)")

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
