#!/bin/sh
# Removes Arr, Plex Media Server, or both, from this computer:
#
#   curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/uninstall.sh | sh
#
# It asks what to remove, and separately whether to delete each one's data (settings,
# database). Your movie and TV files, and anything in the download folder, are never deleted.
# Run with --dry-run to see what it would do, or --help for the options.
set -eu
TARGET=""          # arr, plex or all; asked if empty
DELETE_DATA="${ARR_DELETE_DATA:-}"
YES="${ARR_YES:-0}"
DRY_RUN=0
BIN_DIR="${ARR_BIN_DIR:-}"
STATE_DIR="${ARR_STATE_DIR:-}"

usage() {
	cat <<'EOF'
Remove Arr, Plex Media Server, or both.

Usage: uninstall.sh [options]

  --arr            remove Arr (the default with --yes)
  --plex           remove Plex Media Server
  --all            remove both
  --delete-data    also delete their settings and databases (asked if not given;
                   with --yes, data is kept unless this is given)
  --keep-data      keep them, without asking
  --yes            don't ask anything
  --dry-run        only show what would be done
  --bin-dir DIR    where arr is, if it isn't found
  --state-dir DIR  Arr's folder, if it isn't found

Your movie and TV files, and the download folder, are never deleted.
EOF
}

while [ $# -gt 0 ]; do
	case "$1" in
	--arr) if [ "$TARGET" = plex ]; then TARGET=all; else TARGET=arr; fi ;;
	--plex) if [ "$TARGET" = arr ]; then TARGET=all; else TARGET=plex; fi ;;
	--all) TARGET=all ;;
	--delete-data) DELETE_DATA=1 ;;
	--keep-data) DELETE_DATA=0 ;;
	--yes | -y) YES=1 ;;
	--dry-run) DRY_RUN=1 ;;
	--bin-dir) BIN_DIR="$2"; shift ;;
	--state-dir) STATE_DIR="$2"; shift ;;
	-h | --help) usage; exit 0 ;;
	*) echo "Unknown option: $1 (see --help)" >&2; exit 2 ;;
	esac
	shift
done

say() { printf '%s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }
interactive() { [ "$YES" != 1 ] && [ -r /dev/tty ] && (: </dev/tty) 2>/dev/null; }
ask() { # ask "Question" default -> answer
	if interactive; then
		printf '%s [%s]: ' "$1" "$2" >/dev/tty
		read -r a </dev/tty || a=""
		printf '%s' "${a:-$2}"
	else printf '%s' "$2"; fi
}
confirm() { case "$(ask "$1 (y/n)" "$2")" in [Yy]*) return 0 ;; *) return 1 ;; esac; }
# Runs a command, or only shows it with --dry-run.
run() {
	if [ "$DRY_RUN" = 1 ]; then say "  would run: $*"; else "$@"; fi
}
os=$(uname -s)
SUDO=""
[ "$(id -u)" = 0 ] || SUDO="sudo"
# Under sudo, HOME is usually root's: a per-user Arr is in the home of whoever ran sudo, and
# its services run in that user's session.
U_HOME=$HOME U_UID=$(id -u) AS_USER="" XDG_CFG=${XDG_CONFIG_HOME:-} XDG_DATA=${XDG_DATA_HOME:-}
if [ "$(id -u)" = 0 ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
	case "$SUDO_USER" in *[!A-Za-z0-9._-]*) die "unexpected SUDO_USER: $SUDO_USER" ;; esac
	U_HOME=$( (getent passwd "$SUDO_USER" 2>/dev/null || true) | cut -d: -f6)
	[ -n "$U_HOME" ] || U_HOME=$(eval echo "~$SUDO_USER")
	U_UID=$(id -u "$SUDO_USER")
	AS_USER="sudo -u $SUDO_USER env XDG_RUNTIME_DIR=/run/user/$U_UID"
	XDG_CFG="" XDG_DATA=""
fi
# Folders that hold media: never deleted, nor anything that contains them.
PROTECT=""
# Any video file, or any file over 500 MB (a download in progress has no video name yet)?
# A database is the only big file an app's own folder should have.
has_media() { # has_media <dir>
	[ -n "$(find "$1" -type f \( -iname '*.mkv' -o -iname '*.mp4' -o -iname '*.m4v' -o -iname '*.avi' \
		-o -iname '*.mov' -o -iname '*.wmv' -o -iname '*.ts' -o -iname '*.m2ts' -o -iname '*.mpg' \
		-o -iname '*.mpeg' -o -iname '*.webm' -o -iname '*.iso' \
		-o \( -size +500M ! -name '*.sqlite*' ! -name '*.db' ! -name '*.db-*' \) \) -print 2>/dev/null | head -n1)" ]
}
# Plex keeps everything of its own in this folder, under its data or /config folder.
PLEX_OWN="Library/Application Support/Plex Media Server"
# Deletes an app's own data folder; refuses one that isn't clearly that alone.
remove_dir() { # remove_dir <dir> <why>
	d=${1%/}
	case "$d" in
	"" | / | "$U_HOME" | /home | /Users | /root | /var | /var/lib | /usr | /usr/local | /etc | /opt | /srv | /mnt | \
		/media | /Applications | "$U_HOME/Library" | "$U_HOME/Library/Application Support" | "$U_HOME/.local" | \
		"$U_HOME/.local/share")
		say "  not deleting $1: that's not a folder of its own"
		return 0
		;;
	esac
	[ -e "$d" ] || return 0
	if [ "$CONFIG_UNREADABLE" = 1 ]; then
		say "  not deleting $1: couldn't work out Arr's media folders from $ARR_CONFIG (unreadable, or paths written with escapes); delete it yourself if you're sure"
		return 0
	fi
	held=""
	while IFS= read -r p; do
		[ -n "$p" ] || continue
		case "$p/" in "$d"/*) held=$p ;; esac
	done <<EOF
$PROTECT
EOF
	if [ -n "$held" ]; then
		say "  not deleting $1: it holds $held, which Arr uses for media or downloads; delete it yourself if you're sure"
		return 0
	fi
	if has_media "$d"; then
		say "  not deleting $1: it has movie or TV files (or other large files) in it; delete it yourself if you're sure"
		return 0
	fi
	if [ -w "$(dirname "$1")" ]; then run rm -rf -- "$1"; else run $SUDO rm -rf -- "$1"; fi
	[ "$DRY_RUN" = 1 ] || say "  deleted $1 ($2)"
}
remove_file() { # remove_file <file>
	[ -e "$1" ] || [ -L "$1" ] || return 0
	if [ -w "$(dirname "$1")" ]; then run rm -f -- "$1"; else run $SUDO rm -f -- "$1"; fi
}

# ---------------------------------------------------------------- what's here

# Arr: the service files say where the binary and config are; otherwise the defaults.
ARR_FOUND=""
SYSTEM_UNIT=/etc/systemd/system/arr.service
USER_UNIT="${XDG_CFG:-$U_HOME/.config}/systemd/user/arr.service"
PLIST="$U_HOME/Library/LaunchAgents/io.github.newtonharry.arr.plist"
unquote() { sed 's/%%/%/g; s/\$\$/$/g; s/\\"/"/g; s/\\\\/\\/g'; }
from_unit() { # from_unit <unit file>: sets ARR_BIN and ARR_CONFIG from ExecStart
	line=$(sed -n 's/^ExecStart=//p' "$1" | head -n1)
	ARR_BIN=$(printf '%s' "$line" | sed -n 's/^"\(\([^"\\]\|\\.\)*\)".*/\1/p' | unquote)
	ARR_CONFIG=$(printf '%s' "$line" | sed -n 's/.* --config "\(\([^"\\]\|\\.\)*\)".*/\1/p' | unquote)
}
from_plist() {
	# Every <string>, in order, however many share a line.
	strings=$(awk '{ while (match($0, /<string>[^<]*<\/string>/)) {
			print substr($0, RSTART + 8, RLENGTH - 17); $0 = substr($0, RSTART + RLENGTH) } }' "$1" |
		sed 's/&lt;/</g; s/&gt;/>/g; s/&quot;/"/g; s/&amp;/\&/g')
	ARR_BIN=$(printf '%s\n' "$strings" | sed -n 2p)
	ARR_CONFIG=$(printf '%s\n' "$strings" | sed -n 5p)
}
# Absolute paths assigned to keys matching $2 in TOML file $1, in any TOML spelling:
# key="..." or key = '...', with spaces or comments around, \\ and \" unescaped.
toml_paths() { # toml_paths <file> <key regex>
	awk -v keys="^($2)\$" '
	{
		line = $0
		sub(/^[ \t]+/, "", line)
		eq = index(line, "=")
		if (eq == 0) next
		key = substr(line, 1, eq - 1)
		sub(/[ \t]+$/, "", key)
		if (key !~ keys) next
		val = substr(line, eq + 1)
		sub(/^[ \t]+/, "", val)
		q = substr(val, 1, 1)
		out = ""
		if (q == "\"") {
			for (i = 2; i <= length(val); i++) {
				c = substr(val, i, 1)
				if (c == "\\") { i++; c = substr(val, i, 1) }
				else if (c == "\"") break
				out = out c
			}
		} else if (q == "\047") {
			rest = substr(val, 2)
			out = substr(rest, 1, index(rest, "\047") - 1)
		}
		if (substr(out, 1, 1) == "/") print out
	}' "$1" 2>/dev/null
}
ARR_BIN="" ARR_CONFIG=""
if [ -f "$SYSTEM_UNIT" ]; then from_unit "$SYSTEM_UNIT"; ARR_FOUND="system service"
elif [ -f "$USER_UNIT" ]; then from_unit "$USER_UNIT"; ARR_FOUND="user service"
elif [ -f "$PLIST" ]; then from_plist "$PLIST"; ARR_FOUND="login item"; fi
[ -z "$BIN_DIR" ] || ARR_BIN="$BIN_DIR/arr"
[ -z "$STATE_DIR" ] || ARR_CONFIG="$STATE_DIR/config.toml"
if [ -z "$ARR_BIN" ]; then
	for b in "$(command -v arr 2>/dev/null || true)" /usr/local/bin/arr "$U_HOME/.local/bin/arr"; do
		if [ -n "$b" ] && [ -x "$b" ] && "$b" --version 2>/dev/null | grep -q '^arr '; then ARR_BIN=$b; break; fi
	done
fi
if [ -z "$ARR_CONFIG" ]; then
	for d in /var/lib/arr "${XDG_DATA:-$U_HOME/.local/share}/arr" "$U_HOME/Library/Application Support/arr"; do
		if [ -f "$d/config.toml" ] || [ -f "$d/state.sqlite" ]; then ARR_CONFIG="$d/config.toml"; break; fi
	done
fi
ARR_STATE=""
[ -z "$ARR_CONFIG" ] || ARR_STATE=$(dirname "$ARR_CONFIG")
[ -n "$ARR_FOUND" ] || { [ -z "$ARR_BIN$ARR_STATE" ] || ARR_FOUND="files"; }
DOWNLOADS=""
[ -z "$ARR_CONFIG" ] || [ ! -r "$ARR_CONFIG" ] ||
	DOWNLOADS=$(toml_paths "$ARR_CONFIG" download_dir | head -n1)
# Every folder the config mentions (libraries, downloads) is protected from deletion: any
# quoted absolute path anywhere in it, whatever its key, so no TOML spelling slips past.
# If the config can't be read, nothing is deleted.
CONFIG_UNREADABLE=0
if [ -n "$ARR_CONFIG" ] && [ -e "$ARR_CONFIG" ] && ! [ -r "$ARR_CONFIG" ]; then
	CONFIG_UNREADABLE=1
elif [ -n "$ARR_CONFIG" ] && [ -r "$ARR_CONFIG" ]; then
	# Literal strings ('...') are taken as written. A basic string ("...") with an escape in
	# it can't be decoded safely here, so it marks the config as unreadable instead.
	PROTECT=$(awk '{
		line = $0
		while (match(line, /"([^"\\]|\\.)*"|\047[^\047]*\047/)) {
			q = substr(line, RSTART, 1)
			s = substr(line, RSTART + 1, RLENGTH - 2)
			line = substr(line, RSTART + RLENGTH)
			if (q == "\"" && index(s, "\\") > 0 && (substr(s, 1, 1) == "/" || substr(s, 1, 1) == "\\")) {
				print "\001escaped"; continue
			}
			if (substr(s, 1, 1) == "/") print s
		}
	}' "$ARR_CONFIG" | while IFS= read -r p; do
		if [ "$p" = "$(printf '\001')escaped" ] || [ -d "$p" ]; then printf '%s\n' "${p%/}"; fi
	done)
	case "$PROTECT" in *"$(printf '\001')escaped"*) CONFIG_UNREADABLE=1 ;; esac
	# Multi-line strings ("""...""" or '''...''') aren't read here either.
	if grep -q "\"\"\"\|'''" "$ARR_CONFIG"; then CONFIG_UNREADABLE=1; fi
fi

# Plex: a system package, a snap, Docker containers, or the macOS app.
PLEX_KIND="" PLEX_DATA="" PLEX_CONTAINERS=""
if have dpkg-query && dpkg-query -W -f='${Status}' plexmediaserver 2>/dev/null | grep -q 'ok installed'; then
	PLEX_KIND=deb PLEX_DATA=/var/lib/plexmediaserver
elif have rpm && rpm -q plexmediaserver >/dev/null 2>&1; then
	PLEX_KIND=rpm PLEX_DATA=/var/lib/plexmediaserver
elif have snap && snap list plexmediaserver >/dev/null 2>&1; then
	PLEX_KIND=snap
elif [ "$os" = Darwin ] && [ -d "/Applications/Plex Media Server.app" ]; then
	PLEX_KIND=mac PLEX_DATA="$U_HOME/Library/Application Support/Plex Media Server"
fi
if have docker; then
	PLEX_CONTAINERS=$(docker ps -a --format '{{.Names}} {{.Image}}' 2>/dev/null |
		awk '$2 ~ /(^|\/)(pms-docker|plex)(:|$)/ { print $1 }' || true)
fi

say "Found on this computer:"
if [ -n "$ARR_FOUND" ]; then
	say "  Arr: ${ARR_BIN:-program not found}${ARR_STATE:+, data in $ARR_STATE} ($ARR_FOUND)"
else
	say "  Arr: not installed"
fi
case "$PLEX_KIND" in
deb | rpm) say "  Plex Media Server: system package, data in $PLEX_DATA" ;;
snap) say "  Plex Media Server: snap package" ;;
mac) say "  Plex Media Server: /Applications/Plex Media Server.app, data in $PLEX_DATA" ;;
esac
for c in $PLEX_CONTAINERS; do say "  Plex Media Server: Docker container \"$c\""; done
[ -n "$PLEX_KIND$PLEX_CONTAINERS" ] || say "  Plex Media Server: not installed"

# ---------------------------------------------------------------- what to remove

if [ -z "$TARGET" ]; then
	if ! interactive; then
		TARGET=arr
	else
		say ""
		say "What should be removed?"
		say "  1) Arr"
		say "  2) Plex Media Server"
		say "  3) Both"
		case "$(ask "Choose 1, 2 or 3" 1)" in
		1) TARGET=arr ;; 2) TARGET=plex ;; 3) TARGET=all ;;
		*) die "nothing chosen; nothing was changed" ;;
		esac
	fi
fi
DO_ARR=0 DO_PLEX=0
case "$TARGET" in arr) DO_ARR=1 ;; plex) DO_PLEX=1 ;; all) DO_ARR=1 DO_PLEX=1 ;; esac
[ "$DO_ARR" = 0 ] || [ -n "$ARR_FOUND" ] || { say "Arr isn't installed here."; DO_ARR=0; }
[ "$DO_PLEX" = 0 ] || [ -n "$PLEX_KIND$PLEX_CONTAINERS" ] || { say "Plex isn't installed here."; DO_PLEX=0; }
[ "$DO_ARR$DO_PLEX" != 00 ] || { say "Nothing to remove."; exit 0; }

ARR_DATA=0 PLEX_WIPE=0
if [ "$DO_ARR" = 1 ] && [ -n "$ARR_STATE" ] && { [ -f "$ARR_STATE/config.toml" ] || [ -f "$ARR_STATE/state.sqlite" ]; }; then
	case "$DELETE_DATA" in
	1) ARR_DATA=1 ;;
	0) ;;
	*) confirm "Also delete Arr's settings, secrets and database in $ARR_STATE?" n && ARR_DATA=1 ;;
	esac
fi
if [ "$DO_PLEX" = 1 ]; then
	case "$DELETE_DATA" in
	1) PLEX_WIPE=1 ;;
	0) ;;
	*) confirm "Also delete Plex's own data (its library database, watch history, posters and settings)?" n &&
		PLEX_WIPE=1 ;;
	esac
fi

say ""
say "This will:"
[ "$DO_ARR" = 0 ] || say "  - stop and remove Arr$([ "$ARR_DATA" = 1 ] && echo ", and delete its data" || echo ", keeping its data")"
[ "$DO_PLEX" = 0 ] || say "  - stop and remove Plex Media Server$([ "$PLEX_WIPE" = 1 ] && echo ", and delete its data" || echo ", keeping its data")"
say "  - keep your movie and TV files${DOWNLOADS:+, and the download folder $DOWNLOADS}"
if interactive && ! confirm "Go ahead?" n; then die "nothing was changed"; fi

# ---------------------------------------------------------------- Arr

if [ "$DO_ARR" = 1 ]; then
	step "Removing Arr"
	if [ -f "$SYSTEM_UNIT" ]; then
		for u in arr-update.timer arr-update.service arr-tunnel.service arr.service; do
			run $SUDO systemctl disable --now "$u" >/dev/null 2>&1 || true
			remove_file "/etc/systemd/system/$u"
		done
		run $SUDO systemctl daemon-reload || true
	fi
	udir=$(dirname "$USER_UNIT")
	if [ -f "$USER_UNIT" ]; then
		for u in arr-update.timer arr-update.service arr-tunnel.service arr.service; do
			run $AS_USER systemctl --user disable --now "$u" >/dev/null 2>&1 || true
			remove_file "$udir/$u"
		done
		run $AS_USER systemctl --user daemon-reload || true
	fi
	for p in arr arr-tunnel arr-update; do
		f="$U_HOME/Library/LaunchAgents/io.github.newtonharry.$p.plist"
		[ -f "$f" ] || continue
		if [ -n "$AS_USER" ]; then run launchctl bootout "gui/$U_UID" "$f" 2>/dev/null || true
		else run launchctl unload "$f" 2>/dev/null || true; fi
		remove_file "$f"
	done
	if [ -n "$ARR_BIN" ]; then
		d=$(dirname "$ARR_BIN")
		for f in arr arr.previous arr.new arr-update arr.update-lock; do remove_file "$d/$f"; done
		[ "$DRY_RUN" = 1 ] || say "  removed the arr program from $d"
	fi
	if [ "$ARR_DATA" = 1 ]; then
		remove_dir "$ARR_STATE" "Arr's settings and database"
	elif [ -n "$ARR_STATE" ]; then
		say "  kept $ARR_STATE (delete it yourself, or run this again with --delete-data)"
	fi
	[ ! -x "${ARR_BIN%/arr}/ngrok" ] || say "  kept ${ARR_BIN%/arr}/ngrok (remote access tunnel; delete it if you don't use ngrok)"
fi

# ---------------------------------------------------------------- Plex

if [ "$DO_PLEX" = 1 ]; then
	step "Removing Plex Media Server"
	case "$PLEX_KIND" in
	deb)
		run $SUDO apt-get remove -y plexmediaserver
		for f in /etc/apt/sources.list.d/*plex*; do
			if [ -f "$f" ] && grep -qi 'plex\.tv' "$f"; then remove_file "$f"; fi
		done
		remove_file /usr/share/keyrings/plexmediaserver.v2.gpg
		;;
	rpm)
		if have dnf; then run $SUDO dnf remove -y plexmediaserver; else run $SUDO yum remove -y plexmediaserver; fi
		for f in /etc/yum.repos.d/*plex*; do
			if [ -f "$f" ] && grep -qi 'plex\.tv' "$f"; then remove_file "$f"; fi
		done
		;;
	snap)
		if [ "$PLEX_WIPE" = 1 ]; then run $SUDO snap remove --purge plexmediaserver
		else run $SUDO snap remove plexmediaserver; fi
		;;
	mac)
		run osascript -e 'quit app "Plex Media Server"' 2>/dev/null || true
		remove_dir "/Applications/Plex Media Server.app" "the Plex app"
		;;
	esac
	if [ "$PLEX_WIPE" = 1 ] && [ -n "$PLEX_DATA" ]; then
		case "$PLEX_KIND" in
		mac) remove_dir "$PLEX_DATA" "Plex's library database and settings" ;;
		*) remove_dir "$PLEX_DATA/$PLEX_OWN" "Plex's library database and settings" ;;
		esac
	fi
	if [ "$PLEX_WIPE" = 1 ] && [ "$PLEX_KIND" = mac ]; then
		remove_dir "$U_HOME/Library/Caches/PlexMediaServer" "Plex's cache"
		remove_file "$U_HOME/Library/Preferences/com.plexapp.plexmediaserver.plist"
	fi
	for c in $PLEX_CONTAINERS; do
		# Plex's data in a container lives in the folder mounted at /config.
		cfg=$(docker inspect -f '{{range .Mounts}}{{if eq .Destination "/config"}}{{.Source}}{{end}}{{end}}' "$c" 2>/dev/null || true)
		project=$(docker inspect -f '{{index .Config.Labels "com.docker.compose.project"}}' "$c" 2>/dev/null || true)
		run docker rm -f "$c" >/dev/null
		say "  removed the Docker container \"$c\""
		[ -z "$project" ] || say "  it came from Docker Compose project \"$project\": remove it from that compose file too, or 'docker compose up' brings it back"
		if [ "$PLEX_WIPE" = 1 ] && [ -n "$cfg" ] && [ -d "$cfg/$PLEX_OWN" ]; then
			remove_dir "$cfg/$PLEX_OWN" "Plex's library database and settings"
		elif [ "$PLEX_WIPE" = 1 ] && [ -n "$cfg" ]; then
			say "  kept $cfg: it doesn't look like Plex's own data folder; delete it yourself if you're sure"
		elif [ -n "$cfg" ]; then
			say "  kept its data in $cfg"
		fi
	done
	[ "$PLEX_WIPE" = 1 ] || [ -z "$PLEX_DATA" ] || [ ! -d "$PLEX_DATA" ] ||
		say "  kept Plex's data in $PLEX_DATA (it's used again if you reinstall Plex)"
fi

say ""
say "Done. Your movie and TV files are untouched."
[ "$DRY_RUN" = 0 ] || say "(This was a dry run: nothing was changed.)"
