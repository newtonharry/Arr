#!/bin/sh
# CI: the auto-update rollback, with the real binaries. install.sh and install.ps1 swap in a
# new release, start it, wait for /api/v1/health to report the new version and otherwise put
# the previous binary back. That only works if the previous release starts on the config and
# state the new one has run on, so this checks it, both ways round:
#
#   1. the new binary (the one under test) creates a deployment as the installer lays it out
#      (init, config, setup, check, serve), then the latest published release runs on it;
#   2. the latest published release creates one, the new binary runs on it, and the latest
#      published release runs on it again.
#
# "Runs" is `arr check` and `arr serve` answering /api/v1/health. The old release is downloaded
# and its .sha256 verified. No Plex is needed (the config has no [plex] section).
#
# Usage: sh tests/upgrade/rollback.sh path/to/new/arr
# Environment: ARR_OLD_BASE   where the old release lives (default: the latest GitHub release);
#                             a file:// or http(s):// folder holding arr-<target>.tar.gz and its .sha256
#              ARR_ROLLBACK_PORT   the port to serve on (default: a free one)
set -eu
fail() { echo "FAIL: $*" >&2; exit 1; }

new=${1:-}
[ -x "$new" ] || fail "usage: $0 path/to/new/arr"
new=$(cd "$(dirname "$new")" && pwd)/$(basename "$new")

case "$(uname -s)-$(uname -m)" in
Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
*) fail "no release binary for $(uname -s)-$(uname -m) in this script" ;;
esac
asset="arr-$target.tar.gz"
base=${ARR_OLD_BASE:-https://github.com/newtonharry/Arr/releases/latest/download}
ffprobe=$(command -v ffprobe) || fail "ffprobe is needed (apt install ffmpeg)"
port=${ARR_ROLLBACK_PORT:-$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')}

work=$(mktemp -d)
pid=""
stop() { [ -z "$pid" ] || { kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; pid=""; }; }
trap 'stop; rm -rf "$work"' EXIT

# --- the previous release, checksum verified
mkdir "$work/release"
curl -fsSL "$base/$asset" -o "$work/release/$asset" || fail "cannot download $base/$asset"
curl -fsSL "$base/$asset.sha256" -o "$work/release/$asset.sha256" || fail "cannot download $base/$asset.sha256"
want=$(cut -d' ' -f1 "$work/release/$asset.sha256")
have=$(sha256sum "$work/release/$asset" | cut -d' ' -f1)
[ -n "$want" ] && [ "$want" = "$have" ] || fail "checksum mismatch for $asset ($want vs $have)"
echo "verified $asset sha256 $have"
tar -xzf "$work/release/$asset" -C "$work/release"
old="$work/release/arr-$target/arr"
[ -x "$old" ] || fail "$asset has no arr-$target/arr"
old_version=$("$old" --version | awk '{print $2}')
new_version=$("$new" --version | awk '{print $2}')
echo "old binary: $old_version   new binary: $new_version"

# --- helpers
# The config as install.sh lays it out (v0.10.0), minus [plex], with its sockets on loopback.
write_config() { # state-dir data-dir (downloads and the library, outside the state dir)
	cat >"$1/config.toml" <<EOF
# Arr configuration, written by install.sh. Secrets live in secrets/ (mode 600).

[server]
listen = "127.0.0.1:$port"
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
enabled = false

[execution]
enabled = true
download_cleanup = true

# Sonarr/Radarr can keep running: Arr only acquires what you add to Arr.
[migration]
mode = "coexistence"

[embedded]
download_dir = "$2/downloads"
listen = "127.0.0.1:0"
trackers = true
dht = false
max_active_downloads = 3
upload_limit_bytes_per_sec = 1048576

[library]
ffprobe = "$ffprobe"
movies = "$2/movies"
tv = "$2/tv"

[mcp]
enabled = true
token_file = "secrets/mcp_token"
EOF
	chmod 600 "$1/config.toml"
	od -An -N32 -tx1 /dev/urandom | tr -d ' \n' >"$1/secrets/mcp_token"
	chmod 600 "$1/secrets/mcp_token"
}

# Creates a deployment the way the installer does, with the given binary.
create() { # binary state-dir
	"$1" init --state-dir "$2" >/dev/null
	mkdir -p "$2.data/downloads" "$2.data/movies" "$2.data/tv"
	write_config "$2" "$2.data"
	"$1" setup --config "$2/config.toml" >/dev/null || fail "$1 setup failed"
}

# `arr check`, then `arr serve` until /api/v1/health answers with this binary's version.
run() { # label binary state-dir
	label=$1 bin=$2 state=$3
	version=$("$bin" --version | awk '{print $2}')
	"$bin" check --config "$state/config.toml" >"$work/check.json" 2>"$work/check.err" ||
		{ cat "$work/check.json" "$work/check.err" >&2; fail "$label: arr check refused the state"; }
	"$bin" serve --config "$state/config.toml" >"$work/serve.log" 2>&1 &
	pid=$!
	i=0
	while :; do
		if health=$(curl -fsS "http://127.0.0.1:$port/api/v1/health" 2>/dev/null); then break; fi
		kill -0 "$pid" 2>/dev/null || { cat "$work/serve.log" >&2; fail "$label: arr serve exited"; }
		i=$((i + 1))
		[ "$i" -lt 100 ] || { cat "$work/serve.log" >&2; fail "$label: /api/v1/health never answered"; }
		sleep 0.2
	done
	case "$health" in
	*'"status":"ok"'*"\"version\":\"$version\""* | *"\"version\":\"$version\""*'"status":"ok"'*) ;;
	*) fail "$label: health said $health (wanted ok, version $version)" ;;
	esac
	stop
	echo "ok: $label ($version): check passed, serve healthy"
}

# --- 1. the new binary creates, the old one runs on it
s1="$work/state-new-first"
create "$new" "$s1"
run "new creates, new runs" "$new" "$s1"
run "new creates, OLD runs (rollback)" "$old" "$s1"
run "then the new one again (the update retried)" "$new" "$s1"

# --- 2. the old binary creates, the new one runs, the old one runs again
s2="$work/state-old-first"
create "$old" "$s2"
run "old creates, old runs" "$old" "$s2"
run "old creates, NEW runs (update)" "$new" "$s2"
run "old creates, new has run, OLD runs (rollback)" "$old" "$s2"

echo "rollback: all checks passed"
